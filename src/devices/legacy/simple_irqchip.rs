// Simple IrqChip implementation for virtio devices

use std::sync::{Arc, Mutex};
use anyhow::Error as DeviceError;
use crate::devices::{bus::BusDevice, event::WindowsEvent};
use crate::devices::legacy::irqchip::IrqChipT;

/// Simple IrqChip implementation that stores IRQ state
/// This is a minimal implementation for virtio devices
pub struct SimpleIrqChip {
    mmio_addr: u64,
    mmio_size: u64,
    irq_state: Arc<Mutex<IrqState>>,
}

struct IrqState {
    irq_line: Option<u32>,
    interrupt_evt: Option<Arc<WindowsEvent>>,
}

impl SimpleIrqChip {
    pub fn new(mmio_addr: u64, mmio_size: u64) -> Self {
        Self {
            mmio_addr,
            mmio_size,
            irq_state: Arc::new(Mutex::new(IrqState {
                irq_line: None,
                interrupt_evt: None,
            })),
        }
    }
}

impl BusDevice for SimpleIrqChip {
    fn read(&mut self, _vcpuid: u64, _offset: u64, _data: &mut [u8]) {
        // Simple IRQ chip doesn't need MMIO reads
    }

    fn write(&mut self, _vcpuid: u64, _offset: u64, _data: &[u8]) {
        // Simple IRQ chip doesn't need MMIO writes
    }
}

impl IrqChipT for SimpleIrqChip {
    fn get_mmio_addr(&self) -> u64 {
        self.mmio_addr
    }

    fn get_mmio_size(&self) -> u64 {
        self.mmio_size
    }

    fn set_irq(
        &self,
        irq_line: Option<u32>,
        interrupt_evt: Option<&WindowsEvent>,
    ) -> Result<(), DeviceError> {
        let mut state = self.irq_state.lock().unwrap();
        state.irq_line = irq_line;
        if let Some(evt) = interrupt_evt {
            // Signal the event to notify that an interrupt should be injected
            // In a full implementation, this would actually inject interrupts into the guest
            evt.signal().map_err(|e| DeviceError::msg(format!("Failed to signal interrupt event: {}", e)))?;
        }
        Ok(())
    }

    fn clear_irq(&self, _irq_line: Option<u32>) -> Result<(), DeviceError> {
        // Simple IRQ chip doesn't track line level
        Ok(())
    }
}
