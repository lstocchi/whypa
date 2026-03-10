// IOAPIC emulation for Windows Hypervisor Platform (WHP)
//
// This implements a software IOAPIC that intercepts guest MMIO accesses to the
// IOAPIC registers and injects interrupts into the guest via WHvRequestInterrupt.

use std::sync::Mutex;

use anyhow::Error as DeviceError;
use tracing::{debug, error, warn};
use windows::Win32::System::Hypervisor::*;

use crate::devices::bus::BusDevice;
use crate::devices::event::WindowsEvent;
use crate::devices::legacy::irqchip::{IrqChip, IrqChipT};
use crate::memory::layout;
const IOAPIC_NUM_PINS: usize = 24;

// MMIO register offsets
const IO_REG_SEL: u64 = 0x00;
const IO_WIN: u64 = 0x10;
const IO_EOI: u64 = 0x40;

// Indirect register indices
const IO_APIC_ID: u8 = 0x00;
const IO_APIC_VER: u8 = 0x01;
const IO_APIC_ARB: u8 = 0x02;

// Redirection table entry bit positions
const IOAPIC_LVT_DELIV_MODE_SHIFT: u64 = 8;
const IOAPIC_LVT_DEST_MODE_SHIFT: u64 = 11;
const IOAPIC_LVT_DELIV_STATUS_SHIFT: u64 = 12;
const IOAPIC_LVT_REMOTE_IRR_SHIFT: u64 = 14;
const IOAPIC_LVT_TRIGGER_MODE_SHIFT: u64 = 15;
const IOAPIC_LVT_MASKED_SHIFT: u64 = 16;
const IOAPIC_LVT_DEST_IDX_SHIFT: u64 = 56;

const IOAPIC_VER_ENTRIES_SHIFT: u64 = 16;
const IOAPIC_ID_SHIFT: u64 = 24;

// Redirection table entry bit masks
const IOAPIC_LVT_REMOTE_IRR: u64 = 1 << IOAPIC_LVT_REMOTE_IRR_SHIFT;
const IOAPIC_LVT_TRIGGER_MODE: u64 = 1 << IOAPIC_LVT_TRIGGER_MODE_SHIFT;
const IOAPIC_LVT_DELIV_STATUS: u64 = 1 << IOAPIC_LVT_DELIV_STATUS_SHIFT;

const IOAPIC_RO_BITS: u64 = IOAPIC_LVT_REMOTE_IRR | IOAPIC_LVT_DELIV_STATUS;
const IOAPIC_RW_BITS: u64 = !IOAPIC_RO_BITS;

const IOAPIC_DM_MASK: u64 = 0x7;
const IOAPIC_ID_MASK: u64 = 0xf;
const IOAPIC_VECTOR_MASK: u64 = 0xff;

// IOAPIC delivery modes (match x86 APIC delivery mode encoding)
const IOAPIC_DM_FIXED: u64 = 0x0;
const IOAPIC_DM_LOWEST_PRIORITY: u64 = 0x1;
const IOAPIC_DM_NMI: u64 = 0x4;
const IOAPIC_DM_INIT: u64 = 0x5;
const IOAPIC_DM_EXTINT: u64 = 0x7;

const IOAPIC_REG_REDTBL_BASE: u64 = 0x10;
const IOAPIC_TRIGGER_EDGE: u64 = 0;

/// WHV_INTERRUPT_CONTROL bitfield layout (from WinHvPlatformDefs.h):
///   Bits 0-7:   Type (WHV_INTERRUPT_TYPE)
///   Bits 8-11:  DestinationMode (0=Physical, 1=Logical)
///   Bits 12-15: TriggerMode (0=Edge, 1=Level)
///   Bits 16-23: TargetVtl (0 for default)
///   Bits 24-63: Reserved
const WHV_IC_TYPE_SHIFT: u64 = 0;
const WHV_IC_DEST_MODE_SHIFT: u64 = 8;
const WHV_IC_TRIGGER_MODE_SHIFT: u64 = 12;

/// Parsed redirection table entry info
#[derive(Debug, Default)]
struct IoApicEntryInfo {
    masked: bool,
    trig_mode: u8,   // 0=edge, 1=level
    dest_idx: u8,    // destination APIC ID
    dest_mode: u8,   // 0=physical, 1=logical
    delivery_mode: u8,
    vector: u8,
}

/// 63:56 Destination Field (RW)
/// 55:17 Reserved
/// 16 Interrupt Mask (RW)
/// 15 Trigger Mode (RW)
/// 14 Remote IRR (RO)
/// 13 Interrupt Input Pin Polarity (INTPOL) (RW)
/// 12 Delivery Status (DELIVS) (RO)
/// 11 Destination Mode (DESTMOD) (RW)
/// 10:8 Delivery Mode (DELMOD) (RW)
/// 7:0 Interrupt Vector (INTVEC) (RW)
type RedirectionTableEntry = u64;

/// Mutable IOAPIC state protected by a Mutex for interior mutability.
/// This allows `set_irq` (which takes `&self`) to modify state.
struct IoApicInner {
    id: u8,
    ioregsel: u8,
    irr: u32,
    ioredtbl: [RedirectionTableEntry; IOAPIC_NUM_PINS],
    version: u8,
    irq_eoi: [i32; IOAPIC_NUM_PINS],
}

/// Software IOAPIC implementation for WHP.
///
/// Handles guest MMIO accesses to the IOAPIC register space and injects
/// interrupts into the guest using `WHvRequestInterrupt`.
pub struct IoApic {
    state: Mutex<IoApicInner>,
    partition_handle: WHV_PARTITION_HANDLE,
}

// Safety: WHV_PARTITION_HANDLE is an opaque handle (isize) that is safe to
// use from any thread. The inner state is protected by a Mutex.
unsafe impl Send for IoApic {}

impl IoApic {
    pub fn new(partition_handle: WHV_PARTITION_HANDLE) -> Self {
        Self {
            state: Mutex::new(IoApicInner {
                id: 0,
                ioregsel: 0,
                irr: 0,
                // All entries start masked
                ioredtbl: [1 << IOAPIC_LVT_MASKED_SHIFT; IOAPIC_NUM_PINS],
                version: 0x20,
                irq_eoi: [0; IOAPIC_NUM_PINS],
            }),
            partition_handle,
        }
    }

    /// Parse a redirection table entry into its component fields.
    fn parse_entry(entry: &RedirectionTableEntry) -> IoApicEntryInfo {
        IoApicEntryInfo {
            vector: (entry & IOAPIC_VECTOR_MASK) as u8,
            delivery_mode: ((entry >> IOAPIC_LVT_DELIV_MODE_SHIFT) & IOAPIC_DM_MASK) as u8,
            dest_mode: ((entry >> IOAPIC_LVT_DEST_MODE_SHIFT) & 1) as u8,
            trig_mode: ((entry >> IOAPIC_LVT_TRIGGER_MODE_SHIFT) & 1) as u8,
            masked: ((entry >> IOAPIC_LVT_MASKED_SHIFT) & 1) != 0,
            dest_idx: ((entry >> IOAPIC_LVT_DEST_IDX_SHIFT) & 0xff) as u8,
        }
    }

    /// Inject an interrupt into the guest via WHvRequestInterrupt.
    fn inject_interrupt(partition_handle: WHV_PARTITION_HANDLE, info: &IoApicEntryInfo) {
        // Map IOAPIC delivery mode to WHP interrupt type.
        // The WHP interrupt type values match the x86 APIC delivery mode encoding.
        let interrupt_type = match info.delivery_mode as u64 {
            IOAPIC_DM_FIXED => WHvX64InterruptTypeFixed.0,
            IOAPIC_DM_LOWEST_PRIORITY => WHvX64InterruptTypeLowestPriority.0,
            IOAPIC_DM_NMI => WHvX64InterruptTypeNmi.0,
            IOAPIC_DM_INIT => WHvX64InterruptTypeInit.0,
            IOAPIC_DM_EXTINT => WHvX64InterruptTypeFixed.0, // ExtINT → Fixed
            _ => {
                warn!(
                    "ioapic: unsupported delivery mode {}, treating as fixed",
                    info.delivery_mode
                );
                WHvX64InterruptTypeFixed.0
            }
        };

        // Build the WHV_INTERRUPT_CONTROL bitfield:
        //   Bits 0-7:   Type
        //   Bits 8-11:  DestinationMode
        //   Bits 12-15: TriggerMode
        let bitfield = ((interrupt_type as u64) << WHV_IC_TYPE_SHIFT)
            | ((info.dest_mode as u64) << WHV_IC_DEST_MODE_SHIFT)
            | ((info.trig_mode as u64) << WHV_IC_TRIGGER_MODE_SHIFT);

        let interrupt_control = WHV_INTERRUPT_CONTROL {
            _bitfield: bitfield,
            Destination: info.dest_idx as u32,
            Vector: info.vector as u32,
        };

        unsafe {
            let result = WHvRequestInterrupt(
                partition_handle,
                &interrupt_control as *const _,
                std::mem::size_of::<WHV_INTERRUPT_CONTROL>() as u32,
            );
            if let Err(e) = result {
                error!(
                    "ioapic: failed to inject interrupt vector 0x{:02X} to dest {}: {:?}",
                    info.vector, info.dest_idx, e
                );
            } else {
                debug!(
                    "ioapic: injected vector 0x{:02X} (dm={}, dest={}, trig={})",
                    info.vector, info.delivery_mode, info.dest_idx, info.trig_mode
                );
            }
        }
    }

    /// If the trigger mode is edge, clear the Remote IRR bit.
    fn fix_edge_remote_irr(state: &mut IoApicInner, index: usize) {
        if state.ioredtbl[index] & IOAPIC_LVT_TRIGGER_MODE == IOAPIC_TRIGGER_EDGE {
            state.ioredtbl[index] &= !IOAPIC_LVT_REMOTE_IRR;
        }
    }

    /// Service pending interrupts: scan the IRR and deliver any unmasked interrupts.
    fn service(partition_handle: WHV_PARTITION_HANDLE, state: &mut IoApicInner) {
        for i in 0..IOAPIC_NUM_PINS {
            let mask = 1u32 << i;

            if state.irr & mask != 0 {
                let entry = state.ioredtbl[i];
                let info = Self::parse_entry(&entry);

                if !info.masked {
                    let mut coalesce = false;

                    if info.trig_mode as u64 == IOAPIC_TRIGGER_EDGE {
                        // Edge-triggered: clear IRR immediately
                        state.irr &= !mask;
                    } else {
                        // Level-triggered: set Remote IRR, coalesce if already set
                        coalesce = (state.ioredtbl[i] & IOAPIC_LVT_REMOTE_IRR) != 0;
                        state.ioredtbl[i] |= IOAPIC_LVT_REMOTE_IRR;
                    }

                    if coalesce {
                        // Already have Remote IRR set, coalesce (don't re-deliver)
                        continue;
                    }

                    Self::inject_interrupt(partition_handle, &info);
                }
            }
        }
    }
}

impl IrqChipT for IoApic {
    fn get_mmio_addr(&self) -> u64 {
        layout::IOAPIC_START.0
    }

    fn get_mmio_size(&self) -> u64 {
        layout::IOAPIC_SIZE
    }

    fn set_irq(
        &self,
        irq_line: Option<u32>,
        _interrupt_evt: Option<&WindowsEvent>,
    ) -> Result<(), DeviceError> {
        if let Some(irq) = irq_line {
            if (irq as usize) < IOAPIC_NUM_PINS {
                let mut state = self.state.lock().unwrap();
                state.irr |= 1 << irq;
                Self::service(self.partition_handle, &mut state);
            } else {
                warn!("ioapic: set_irq: irq {} out of range (max {})", irq, IOAPIC_NUM_PINS - 1);
            }
        }
        Ok(())
    }

    fn clear_irq(&self, irq_line: Option<u32>) -> Result<(), DeviceError> {
        if let Some(irq) = irq_line {
            if (irq as usize) < IOAPIC_NUM_PINS {
                let mut state = self.state.lock().unwrap();
                state.irr &= !(1u32 << irq);
            }
        }
        Ok(())
    }
}

impl BusDevice for IoApic {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        let state = self.state.lock().unwrap();

        let val = match offset {
            IO_REG_SEL => {
                debug!("ioapic: read: ioregsel = 0x{:02X}", state.ioregsel);
                state.ioregsel as u32
            }
            IO_WIN => {
                if data.len() != 4 {
                    error!("ioapic: bad read size {} (expected 4)", data.len());
                    return;
                }

                match state.ioregsel {
                    IO_APIC_ID | IO_APIC_ARB => {
                        debug!("ioapic: read: ID = {}", state.id);
                        ((state.id as u64) << IOAPIC_ID_SHIFT) as u32
                    }
                    IO_APIC_VER => {
                        let ver = state.version as u32
                            | ((IOAPIC_NUM_PINS as u32 - 1) << IOAPIC_VER_ENTRIES_SHIFT);
                        debug!("ioapic: read: VERSION = 0x{:08X}", ver);
                        ver
                    }
                    _ => {
                        let index = (state.ioregsel as u64 - IOAPIC_REG_REDTBL_BASE) >> 1;

                        if index < IOAPIC_NUM_PINS as u64 {
                            let val = if state.ioregsel & 1 > 0 {
                                // Upper 32 bits
                                (state.ioredtbl[index as usize] >> 32) as u32
                            } else {
                                // Lower 32 bits
                                (state.ioredtbl[index as usize] & 0xffff_ffff) as u32
                            };
                            debug!(
                                "ioapic: read: REDTBL[{}] {} = 0x{:08X}",
                                index,
                                if state.ioregsel & 1 > 0 { "hi" } else { "lo" },
                                val
                            );
                            val
                        } else {
                            warn!("ioapic: read: register index {} out of range", state.ioregsel);
                            0
                        }
                    }
                }
            }
            _ => {
                warn!("ioapic: read: unknown offset 0x{:X}", offset);
                0
            }
        };

        let out_arr = val.to_ne_bytes();
        for i in 0..data.len().min(4) {
            data[i] = out_arr[i];
        }
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        if data.len() != 4 {
            error!("ioapic: bad write size {} (expected 4)", data.len());
            return;
        }

        let arr = [data[0], data[1], data[2], data[3]];
        let val = u32::from_ne_bytes(arr);
        let partition_handle = self.partition_handle;

        let mut state = self.state.lock().unwrap();
        match offset {
            IO_REG_SEL => {
                debug!("ioapic: write: ioregsel = 0x{:02X}", val);
                state.ioregsel = val as u8;
            }
            IO_WIN => {
                match state.ioregsel {
                    IO_APIC_ID => {
                        state.id =
                            ((val >> IOAPIC_ID_SHIFT) & (IOAPIC_ID_MASK as u32)) as u8;
                        debug!("ioapic: write: ID = {}", state.id);
                    }
                    // VERSION and ARB are read-only
                    IO_APIC_VER | IO_APIC_ARB => {
                        debug!("ioapic: write: ignored (read-only register)");
                    }
                    _ => {
                        if state.ioregsel < (IO_WIN as u8) {
                            debug!("ioapic: write: invalid register 0x{:02X}, ignoring", state.ioregsel);
                            return;
                        }

                        let index = (state.ioregsel as u64 - IOAPIC_REG_REDTBL_BASE) >> 1;
                        if index >= IOAPIC_NUM_PINS as u64 {
                            warn!("ioapic: write: REDTBL index {} out of range", index);
                            return;
                        }

                        debug!(
                            "ioapic: write: REDTBL[{}] {} = 0x{:08X}",
                            index,
                            if state.ioregsel & 1 > 0 { "hi" } else { "lo" },
                            val
                        );

                        // Save read-only bits
                        let ro_bits = state.ioredtbl[index as usize] & IOAPIC_RO_BITS;

                        // Write the appropriate half
                        if state.ioregsel & 1 > 0 {
                            // Upper 32 bits
                            state.ioredtbl[index as usize] &= 0xffff_ffff;
                            state.ioredtbl[index as usize] |= (val as u64) << 32;
                        } else {
                            // Lower 32 bits
                            state.ioredtbl[index as usize] &= !0xffff_ffff_u64;
                            state.ioredtbl[index as usize] |= val as u64;
                        }

                        // Restore read-only bits
                        state.ioredtbl[index as usize] &= IOAPIC_RW_BITS;
                        state.ioredtbl[index as usize] |= ro_bits;
                        state.irq_eoi[index as usize] = 0;

                        // If edge-triggered, clear Remote IRR
                        Self::fix_edge_remote_irr(&mut state, index as usize);

                        // Service any pending interrupts with updated routing
                        Self::service(partition_handle, &mut state);
                    }
                }
            }
            IO_EOI => {
                // EOI register: the guest writes the vector number to acknowledge.
                // Clear Remote IRR for any redirection entry matching that vector.
                let vector = val as u8;
                debug!("ioapic: EOI for vector 0x{:02X}", vector);

                for i in 0..IOAPIC_NUM_PINS {
                    let entry = state.ioredtbl[i];
                    let entry_vector = (entry & IOAPIC_VECTOR_MASK) as u8;

                    if entry_vector == vector && (entry & IOAPIC_LVT_REMOTE_IRR) != 0 {
                        state.ioredtbl[i] &= !IOAPIC_LVT_REMOTE_IRR;

                        // If IRR is still set for this pin, re-service
                        if state.irr & (1 << i) != 0 {
                            Self::service(partition_handle, &mut state);
                        }
                    }
                }
            }
            _ => {
                warn!("ioapic: write: unknown offset 0x{:X}", offset);
            }
        }
    }
}

/// Adapter that wraps an IrqChip (containing IoApic) to implement BusDevice
/// for the partition's MMIO dispatch system.
pub struct IoApicMmioAdapter {
    irqchip: IrqChip,
}

impl IoApicMmioAdapter {
    pub fn new(irqchip: IrqChip) -> Self {
        Self { irqchip }
    }
}

impl BusDevice for IoApicMmioAdapter {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        self.irqchip.lock().unwrap().read(0, offset, data);
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        self.irqchip.lock().unwrap().write(0, offset, data);
    }
}
