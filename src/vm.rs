//! vCPU execution loop and VM-exit handling.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Result;
use tracing::{debug, error, info, warn};
use windows::Win32::System::Hypervisor::*;

use crate::memory::memory::{GuestAddress, MemoryAccessViolation};
use crate::partition::Partition;

/// Run the vCPU execution loop on the calling thread.
///
/// Keeps running until the partition signals a stop, an unrecoverable error
/// occurs, or `running` is set to `false` (e.g. by the Ctrl+C handler).
pub fn run(partition: &mut Partition, kernel_entry: u64, running: &AtomicBool) {
    match partition.verify_rip(0) {
        Ok(rip) => {
            if rip != kernel_entry {
                warn!(rip = format_args!("0x{:X}", rip),
                      expected = format_args!("0x{:X}", kernel_entry),
                      "RIP mismatch before first run");
            }
        }
        Err(e) => {
            warn!(error = %e, "Could not verify initial RIP");
        }
    }

    let mut iteration: u64 = 0;
    let mut last_rip: u64 = 0;
    let mut rip_repeat_count: u64 = 0;

    while running.load(Ordering::Relaxed) {
        iteration += 1;

        let exit_context = match partition.run_vp(0) {
            Ok(ctx) => ctx,
            Err(e) => {
                error!(error = %e, iteration, "VP run failed");
                break;
            }
        };

        let current_rip = exit_context.VpContext.Rip;

        if current_rip == last_rip {
            rip_repeat_count += 1;
            if rip_repeat_count > 1000 && rip_repeat_count % 1000 == 0 {
                warn!(rip = format_args!("0x{:X}", current_rip),
                      stuck_for = rip_repeat_count, iteration,
                      "VM appears stuck at same RIP");
            }
        } else {
            if rip_repeat_count > 100 {
                debug!(from = format_args!("0x{:X}", last_rip),
                       to = format_args!("0x{:X}", current_rip),
                       after = rip_repeat_count,
                       "Resumed from stuck RIP");
            }
            last_rip = current_rip;
            rip_repeat_count = 0;
        }

        match partition.handle_exit(0, &exit_context) {
            Ok(true) => {}
            Ok(false) => {
                info!(iteration, "VM execution stopped");
                break;
            }
            Err(e) => {
                error!(error = %e, iteration,
                       rip = format_args!("0x{:X}", current_rip),
                       "Error handling VM exit");
                break;
            }
        }

        if iteration % 100_000 == 0 {
            debug!(iteration, rip = format_args!("0x{:X}", current_rip), "VM running");
        }
    }

    if !running.load(Ordering::Relaxed) {
        info!(iteration, "VM stopped by Ctrl+C");
    }
}

// ---------------------------------------------------------------------------
// VM-exit handling – logically part of the run-loop, implemented on Partition
// so it can access internal state directly.
// ---------------------------------------------------------------------------

impl Partition {
    /// Handle a VM exit. Returns `true` to continue running, `false` to stop.
    pub fn handle_exit(&mut self, _vp_id: u32, exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<bool> {
        let exit_reason = exit_context.ExitReason.0;

        match exit_reason {
            x if x == WHvRunVpExitReasonNone.0 => Ok(false),

            x if x == WHvRunVpExitReasonMemoryAccess.0 => {
                let violation = MemoryAccessViolation::from_exit_context(exit_context)
                    .ok_or_else(|| anyhow::anyhow!("Failed to extract memory access violation"))?;

                if self.memory.find_mmio(violation.gpa.0).is_some() {
                    // Use the WHP emulator to handle MMIO – it decodes the instruction,
                    // calls our memory_callback, and updates the correct register.
                    let memory_access_ctx = unsafe { &exit_context.Anonymous.MemoryAccess };
                    let vp_context = &exit_context.VpContext;

                    match self.emulator.try_mmio_emulation(
                        self as *const _ as *const std::ffi::c_void,
                        vp_context,
                        memory_access_ctx,
                    ) {
                        Ok(status) => {
                            let emulated = unsafe { status.Anonymous._bitfield as u64 } & 0x1 == 1;
                            Ok(emulated)
                        }
                        Err(_) => Ok(false),
                    }
                } else if let Some(region) = self.memory.find_region(violation.gpa) {
                    if !region.perms.contains(violation.action) {
                        return Ok(false);
                    }
                    // Valid access to mapped memory shouldn't normally cause an exit.
                    Ok(false)
                } else {
                    // Unmapped memory access (page fault).
                    Ok(false)
                }
            }

            x if x == WHvRunVpExitReasonX64IoPortAccess.0 => {
                self.handle_io_port(exit_context)
            }

            x if x == WHvRunVpExitReasonException.0 || x == 4 => Ok(false),
            x if x == WHvRunVpExitReasonUnrecoverableException.0 => Ok(false),
            x if x == WHvRunVpExitReasonInvalidVpRegisterValue.0 => Ok(false),
            x if x == WHvRunVpExitReasonUnsupportedFeature.0 => Ok(false),

            x if x == WHvRunVpExitReasonX64InterruptWindow.0 => {
                self.handle_interrupt_window()
            }

            x if x == WHvRunVpExitReasonX64Halt.0 => Ok(true),

            x if x == WHvRunVpExitReasonX64ApicEoi.0 => {
                // Forward EOI to the IOAPIC so it can clear Remote IRR for
                // level-triggered interrupts.
                let vector = unsafe { exit_context.Anonymous.ApicEoi.InterruptVector };
                if let Some(handler) = self.mmio_handlers.get_mut("ioapic") {
                    handler.write(0, 0x40, &(vector as u32).to_le_bytes());
                }
                Ok(true)
            }

            x if x == WHvRunVpExitReasonX64Cpuid.0 => {
                self.handle_cpuid(exit_context)
            }

            x if x == WHvRunVpExitReasonX64MsrAccess.0 => {
                self.handle_msr_access(exit_context)
            }

            _ => Ok(false),
        }
    }

    /// Handle I/O port access via the WHP emulator.
    fn handle_io_port(&mut self, exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<bool> {
        let io_port_access_ctx = unsafe { &exit_context.Anonymous.IoPortAccess };
        let vp_context = &exit_context.VpContext;
        let result = self.emulator.try_io_emulation(
            self as *const _ as *const std::ffi::c_void,
            vp_context,
            io_port_access_ctx,
        )?;

        let emulated = unsafe { result.Anonymous._bitfield as u64 } & 0x1 == 1;
        Ok(emulated)
    }

    /// Inject a timer interrupt when the guest opens an interrupt window.
    fn handle_interrupt_window(&self) -> Result<bool> {
        static LAST_INTERRUPT_TIME: AtomicU64 = AtomicU64::new(0);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        let last_time = LAST_INTERRUPT_TIME.load(Ordering::Relaxed);
        if now.saturating_sub(last_time) >= 1 || last_time == 0 {
            LAST_INTERRUPT_TIME.store(now, Ordering::Relaxed);

            unsafe {
                let interrupt_control = WHV_INTERRUPT_CONTROL {
                    _bitfield: WHvX64InterruptTypeFixed.0 as u64,
                    Destination: 0,
                    Vector: 0x20,
                };
                let _ = WHvRequestInterrupt(
                    self.handle,
                    &interrupt_control as *const _,
                    std::mem::size_of::<WHV_INTERRUPT_CONTROL>() as u32,
                );
            }
        }

        Ok(true)
    }

    /// Handle a CPUID exit by returning Hyper-V enlightenment leaves.
    fn handle_cpuid(&self, exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<bool> {
        let cpuid_access = unsafe { &exit_context.Anonymous.CpuidAccess };
        let leaf = cpuid_access.Rax;

        let mut rax = cpuid_access.DefaultResultRax;
        let mut rbx = cpuid_access.DefaultResultRbx;
        let mut rcx = cpuid_access.DefaultResultRcx;
        let mut rdx = cpuid_access.DefaultResultRdx;

        match leaf {
            0x40000000 => {
                // Hyper-V hypervisor present: max leaf + "Microsoft Hv" vendor signature.
                rax = 0x40000005;
                rbx = u32::from_le_bytes(*b"Micr") as u64;
                rcx = u32::from_le_bytes(*b"osof") as u64;
                rdx = u32::from_le_bytes(*b"t Hv") as u64;
            }
            0x40000001 => {
                // Hypervisor interface identification: "Hv#1"
                rax = u32::from_le_bytes(*b"Hv#1") as u64;
                rbx = 0;
                rcx = 0;
                rdx = 0;
            }
            0x40000002 => {
                // Hypervisor system identity (version info)
                rax = 0;
                rbx = (10 << 16) | 0;
                rcx = 0;
                rdx = 0;
            }
            0x40000003 => {
                // Hypervisor feature identification.
                // EAX privilege bits:
                //   Bit  1: AccessPartitionReferenceCounter (MSR 0x40000020)
                //   Bit  9: AccessReferenceTsc (MSR 0x40000021)
                //   Bit 15: AccessTscInvariantControls (MSR 0x40000118)
                rax = (1 << 1) | (1 << 9) | (1 << 15);
                rbx = 0;
                rcx = 0;
                rdx = 0;
            }
            0x40000004 | 0x40000005 => {
                rax = 0;
                rbx = 0;
                rcx = 0;
                rdx = 0;
            }
            _ => { /* pass through default results */ }
        }

        let next_rip = exit_context.VpContext.Rip
            + (exit_context.VpContext._bitfield & 0x0F) as u64;

        let register_names = [
            WHvX64RegisterRax,
            WHvX64RegisterRbx,
            WHvX64RegisterRcx,
            WHvX64RegisterRdx,
            WHvX64RegisterRip,
        ];

        let register_values = [
            WHV_REGISTER_VALUE { Reg64: rax },
            WHV_REGISTER_VALUE { Reg64: rbx },
            WHV_REGISTER_VALUE { Reg64: rcx },
            WHV_REGISTER_VALUE { Reg64: rdx },
            WHV_REGISTER_VALUE { Reg64: next_rip },
        ];

        unsafe {
            WHvSetVirtualProcessorRegisters(
                self.handle,
                0,
                register_names.as_ptr(),
                register_names.len() as u32,
                register_values.as_ptr(),
            )?;
        }

        Ok(true)
    }

    /// Handle MSR read/write (Hyper-V enlightenments).
    fn handle_msr_access(&mut self, exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<bool> {
        let msr_access = unsafe { &exit_context.Anonymous.MsrAccess };
        let msr_number = msr_access.MsrNumber;
        let is_write = unsafe { msr_access.AccessInfo.Anonymous._bitfield } & 0x1 != 0;

        let mut rax = 0u64;
        let mut rdx = 0u64;

        match msr_number {
            0x40000000 => { // HV_X64_MSR_GUEST_OS_ID
                if is_write {
                    self.guest_os_id = (msr_access.Rdx << 32) | (msr_access.Rax & 0xFFFFFFFF);
                } else {
                    rax = self.guest_os_id & 0xFFFFFFFF;
                    rdx = self.guest_os_id >> 32;
                }
            }
            0x40000001 => { // HV_X64_MSR_HYPERCALL
                if !is_write {
                    rax = 1; // Mark as enabled
                }
            }
            0x40000020 => { // HV_X64_MSR_TIME_REF_COUNT (read-only)
                // Return elapsed time since VM start in 100ns increments.
                use std::sync::OnceLock;
                static VM_START_TIME: OnceLock<std::time::Instant> = OnceLock::new();
                let start = VM_START_TIME.get_or_init(|| std::time::Instant::now());
                let ticks = start.elapsed().as_nanos() / 100;
                rax = (ticks & 0xFFFFFFFF) as u64;
                rdx = ((ticks >> 32) & 0xFFFFFFFF) as u64;
            }
            0x40000021 => { // HV_X64_MSR_REFERENCE_TSC
                if is_write {
                    let msr_value = (msr_access.Rdx << 32) | (msr_access.Rax & 0xFFFFFFFF);
                    if msr_value & 1 != 0 {
                        let gpa = msr_value & !0xFFF;
                        self.write_tsc_reference_page(gpa)?;
                        self.tsc_reference_gpa = Some(gpa);
                    } else {
                        self.tsc_reference_gpa = None;
                    }
                } else if let Some(gpa) = self.tsc_reference_gpa {
                    let value = gpa | 1;
                    rax = value & 0xFFFFFFFF;
                    rdx = value >> 32;
                }
            }
            0x40000118 => { // HV_X64_MSR_TSC_INVARIANT_CONTROL
                static TSC_INVARIANT_CONTROL: AtomicU64 = AtomicU64::new(0);
                if is_write {
                    let val = (msr_access.Rdx << 32) | (msr_access.Rax & 0xFFFFFFFF);
                    TSC_INVARIANT_CONTROL.store(val, Ordering::Relaxed);
                } else {
                    let val = TSC_INVARIANT_CONTROL.load(Ordering::Relaxed);
                    rax = val & 0xFFFFFFFF;
                    rdx = val >> 32;
                }
            }
            _ => {}
        }

        if !is_write {
            let names = [WHvX64RegisterRax, WHvX64RegisterRdx];
            let values = [
                WHV_REGISTER_VALUE { Reg64: rax },
                WHV_REGISTER_VALUE { Reg64: rdx },
            ];
            unsafe {
                WHvSetVirtualProcessorRegisters(self.handle, 0, &names as *const _, 2, &values as *const _)?;
            }
        }

        Self::advance_rip(
            exit_context.VpContext,
            (exit_context.VpContext._bitfield & 0x0F) as u64,
            self.handle,
            0,
        )?;
        Ok(true)
    }

    /// Advance RIP past the current instruction.
    fn advance_rip(
        vp_context: WHV_VP_EXIT_CONTEXT,
        instruction_length: u64,
        handle: WHV_PARTITION_HANDLE,
        vp_id: u32,
    ) -> Result<()> {
        let new_rip = vp_context.Rip + instruction_length;
        let mut rip_reg = WHV_REGISTER_VALUE::default();
        rip_reg.Reg64 = new_rip;
        unsafe {
            WHvSetVirtualProcessorRegisters(
                handle,
                vp_id,
                &[WHvX64RegisterRip] as *const _,
                1,
                &mut rip_reg as *mut _ as *mut WHV_REGISTER_VALUE,
            ).map_err(|e| anyhow::anyhow!("Failed to advance RIP: {}", e))?;
        }
        Ok(())
    }

    /// Get the host TSC frequency in Hz.
    /// Tries CPUID leaf 0x15 (TSC/crystal ratio), then 0x16 (processor base frequency).
    fn get_tsc_frequency_hz() -> u64 {
        unsafe {
            let cpuid15 = core::arch::x86_64::__cpuid(0x15);
            if cpuid15.eax != 0 && cpuid15.ebx != 0 && cpuid15.ecx != 0 {
                return (cpuid15.ecx as u64 * cpuid15.ebx as u64) / cpuid15.eax as u64;
            }

            let cpuid16 = core::arch::x86_64::__cpuid(0x16);
            if cpuid16.eax != 0 {
                return cpuid16.eax as u64 * 1_000_000;
            }

            // Last resort fallback
            2_712_000_000
        }
    }

    /// Write the Hyper-V Reference TSC Page into guest memory.
    ///
    /// Guest formula: `ReferenceTime = ((RDTSC() * TscScale) >> 64) + TscOffset`
    /// Result is in 100-nanosecond units (10 MHz).
    fn write_tsc_reference_page(&self, gpa: u64) -> Result<()> {
        let tsc_freq = Self::get_tsc_frequency_hz();

        // TscScale: fixed-point multiplier converting TSC ticks → 100ns ticks.
        let tsc_scale: u64 = ((10_000_000u128 << 64) / tsc_freq as u128) as u64;

        // TscOffset: calibrate so ReferenceTime ≈ 0 at VM start.
        let tsc_offset: i64 = -(((self.vm_start_tsc as u128 * tsc_scale as u128) >> 64) as i64);

        let mut page = [0u8; 4096];
        page[0..4].copy_from_slice(&1u32.to_le_bytes());       // TscSequence = 1 (valid)
        page[8..16].copy_from_slice(&tsc_scale.to_le_bytes()); // TscScale
        page[16..24].copy_from_slice(&tsc_offset.to_le_bytes()); // TscOffset

        self.memory.write_guest_memory(&page, GuestAddress(gpa))?;

        debug!(
            gpa = format_args!("0x{:X}", gpa),
            tsc_freq_mhz = tsc_freq / 1_000_000,
            tsc_scale = format_args!("0x{:016X}", tsc_scale),
            tsc_offset = tsc_offset,
            "Hyper-V Reference TSC Page written"
        );

        Ok(())
    }
}
