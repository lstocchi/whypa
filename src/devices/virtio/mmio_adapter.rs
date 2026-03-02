// Adapter to bridge MmioTransport (BusDevice) to Partition's MmioHandler trait

use std::sync::{Arc, Mutex};
use anyhow::Result;
use crate::partition::MmioHandler;
use crate::devices::virtio::mmio::MmioTransport;
use crate::devices::bus::BusDevice;

/// Adapter that wraps MmioTransport to implement MmioHandler for the partition
pub struct MmioTransportAdapter {
    transport: Arc<Mutex<MmioTransport>>,
}

impl MmioTransportAdapter {
    pub fn new(transport: MmioTransport) -> Self {
        Self {
            transport: Arc::new(Mutex::new(transport)),
        }
    }
}

impl MmioHandler for MmioTransportAdapter {
    fn handle_read(&self, offset: u64, size: u32) -> Result<u64> {
        let mut transport = self.transport.lock().unwrap();
        
        // Create a buffer for the read
        let mut data = vec![0u8; size as usize];
        
        // Call the BusDevice read method (vcpuid 0 for now)
        transport.read(0, offset, &mut data);
        
        // Convert the data back to u64 based on size
        let value = match size {
            1 => data[0] as u64,
            2 => {
                let v = u16::from_le_bytes([data[0], data[1]]);
                v as u64
            }
            4 => {
                let v = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                v as u64
            }
            8 => {
                let v = u64::from_le_bytes([
                    data[0], data[1], data[2], data[3],
                    data[4], data[5], data[6], data[7],
                ]);
                v
            }
            _ => return Err(anyhow::anyhow!("Unsupported read size: {}", size)),
        };
        
        // Log important virtio register accesses
        match offset {
            0x00 => eprintln!("  [Virtio] Read MagicValue: 0x{:X}", value),
            0x04 => eprintln!("  [Virtio] Read Version: 0x{:X}", value),
            0x08 => eprintln!("  [Virtio] Read DeviceID: 0x{:X}", value),
            0x0C => eprintln!("  [Virtio] Read VendorID: 0x{:X}", value),
            0x10 => eprintln!("  [Virtio] Read DeviceFeatures: 0x{:X}", value),
            0x14 => eprintln!("  [Virtio] Read DeviceFeaturesSel: 0x{:X}", value),
            0x20 => eprintln!("  [Virtio] Read QueueSel: 0x{:X}", value),
            0x24 => eprintln!("  [Virtio] Read QueueNumMax: 0x{:X}", value),
            0x28 => eprintln!("  [Virtio] Read QueueNum: 0x{:X}", value),
            0x30 => eprintln!("  [Virtio] Read QueueReady: 0x{:X}", value),
            0x40 => eprintln!("  [Virtio] Read QueueNotify: 0x{:X}", value),
            0x50 => eprintln!("  [Virtio] Read InterruptStatus: 0x{:X}", value),
            0x60 => eprintln!("  [Virtio] Read Status: 0x{:X}", value),
            _ => {
                // Only log other reads if they're not common polling addresses
                if offset >= 0x70 {
                    eprintln!("  [Virtio] Read offset 0x{:X}: 0x{:X}", offset, value);
                }
            }
        }
        
        Ok(value)
    }
    
    fn handle_write(&mut self, offset: u64, size: u32, value: u64) -> Result<()> {
        let mut transport = self.transport.lock().unwrap();
        
        // Log important virtio register writes
        match offset {
            0x14 => eprintln!("  [Virtio] Write DeviceFeaturesSel: 0x{:X}", value),
            0x20 => eprintln!("  [Virtio] Write QueueSel: 0x{:X}", value),
            0x24 => eprintln!("  [Virtio] Write QueueNum: 0x{:X}", value),
            0x28 => eprintln!("  [Virtio] Write QueueAlign: 0x{:X}", value),
            0x30 => eprintln!("  [Virtio] Write QueueReady: 0x{:X}", value),
            0x38 => eprintln!("  [Virtio] Write QueueDescLow: 0x{:X}", value),
            0x3C => eprintln!("  [Virtio] Write QueueDescHigh: 0x{:X}", value),
            0x40 => eprintln!("  [Virtio] Write QueueAvailLow: 0x{:X}", value),
            0x44 => eprintln!("  [Virtio] Write QueueAvailHigh: 0x{:X}", value),
            0x48 => eprintln!("  [Virtio] Write QueueUsedLow: 0x{:X}", value),
            0x4C => eprintln!("  [Virtio] Write QueueUsedHigh: 0x{:X}", value),
            0x50 => eprintln!("  [Virtio] Write InterruptAck: 0x{:X}", value),
            0x64 => eprintln!("  [Virtio] Write Status: 0x{:X}", value),
            _ => {
                if offset >= 0x70 {
                    eprintln!("  [Virtio] Write offset 0x{:X}: 0x{:X}", offset, value);
                }
            }
        }
        
        // Convert value to bytes based on size
        let data = match size {
            1 => vec![value as u8],
            2 => {
                let v = value as u16;
                v.to_le_bytes().to_vec()
            }
            4 => {
                let v = value as u32;
                v.to_le_bytes().to_vec()
            }
            8 => value.to_le_bytes().to_vec(),
            _ => return Err(anyhow::anyhow!("Unsupported write size: {}", size)),
        };
        
        // Call the BusDevice write method (vcpuid 0 for now)
        transport.write(0, offset, &data);
        
        Ok(())
    }
}
