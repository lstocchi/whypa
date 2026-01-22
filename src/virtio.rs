use std::result::Result;

// VirtIO MMIO base address - typically at 0x0A000000 (160MB)
pub const VIRTIO_MMIO_BASE: u64 = 0x0A000000;
pub const VIRTIO_MMIO_SIZE: u64 = 0x200; // 512 bytes per device

// VirtIO constants
const VIRTIO_MAGIC: u32 = 0x74726976; // "virt"
const VIRTIO_VERSION: u32 = 0x2; // VirtIO 1.0
const VIRTIO_DEVICE_ID_BLOCK: u32 = 0x2;
const VIRTIO_VENDOR_ID: u32 = 0x554D4551; // "QEMU"

// VirtIO MMIO register offsets
const VIRTIO_MMIO_MAGIC_VALUE: u64 = 0x000;
const VIRTIO_MMIO_VERSION: u64 = 0x004;
const VIRTIO_MMIO_DEVICE_ID: u64 = 0x008;
const VIRTIO_MMIO_VENDOR_ID: u64 = 0x00C;
const VIRTIO_MMIO_DEVICE_FEATURES: u64 = 0x010;
const VIRTIO_MMIO_DEVICE_FEATURES_SEL: u64 = 0x014;
const VIRTIO_MMIO_DRIVER_FEATURES: u64 = 0x020;
const VIRTIO_MMIO_DRIVER_FEATURES_SEL: u64 = 0x024;
const VIRTIO_MMIO_QUEUE_SEL: u64 = 0x030;
const VIRTIO_MMIO_QUEUE_NUM_MAX: u64 = 0x034;
const VIRTIO_MMIO_QUEUE_NUM: u64 = 0x038;
const VIRTIO_MMIO_QUEUE_READY: u64 = 0x044;
const VIRTIO_MMIO_QUEUE_NOTIFY: u64 = 0x050;
const VIRTIO_MMIO_INTERRUPT_STATUS: u64 = 0x060;
const VIRTIO_MMIO_INTERRUPT_ACK: u64 = 0x064;
const VIRTIO_MMIO_STATUS: u64 = 0x070;
const VIRTIO_MMIO_QUEUE_DESC_LOW: u64 = 0x080;
const VIRTIO_MMIO_QUEUE_DESC_HIGH: u64 = 0x084;
const VIRTIO_MMIO_QUEUE_AVAIL_LOW: u64 = 0x090;
const VIRTIO_MMIO_QUEUE_AVAIL_HIGH: u64 = 0x094;
const VIRTIO_MMIO_QUEUE_USED_LOW: u64 = 0x0A0;
const VIRTIO_MMIO_QUEUE_USED_HIGH: u64 = 0x0A4;
const VIRTIO_MMIO_CONFIG_GENERATION: u64 = 0x0FC;
const VIRTIO_MMIO_CONFIG: u64 = 0x100;

// VirtIO device status bits
const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_FAILED: u8 = 128;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;

// VirtIO block device features
const VIRTIO_BLK_F_SIZE_MAX: u64 = 1 << 1;
const VIRTIO_BLK_F_SEG_MAX: u64 = 1 << 2;
const VIRTIO_BLK_F_GEOMETRY: u64 = 1 << 4;
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const VIRTIO_BLK_F_TOPOLOGY: u64 = 1 << 10;
const VIRTIO_BLK_F_CONFIG_WCE: u64 = 1 << 11;
const VIRTIO_BLK_F_MQ: u64 = 1 << 12;
const VIRTIO_BLK_F_DISCARD: u64 = 1 << 13;
const VIRTIO_BLK_F_WRITE_ZEROES: u64 = 1 << 14;

// VirtIO block request types
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_T_GET_ID: u32 = 8;
const VIRTIO_BLK_T_DISCARD: u32 = 11;
const VIRTIO_BLK_T_WRITE_ZEROES: u32 = 13;

// VirtIO block request status
const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_IOERR: u8 = 1;
const VIRTIO_BLK_S_UNSUPP: u8 = 2;

// VirtIO queue structure
#[derive(Clone)]
struct VirtQueue {
    desc_addr: u64,      // Descriptor table address
    avail_addr: u64,     // Available ring address
    used_addr: u64,      // Used ring address
    size: u16,           // Queue size
    ready: bool,         // Queue is ready
}

// VirtIO descriptor
#[repr(C, packed)]
struct VirtioDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

// VirtIO available ring
#[repr(C, packed)]
struct VirtioAvail {
    flags: u16,
    idx: u16,
    ring: [u16; 0], // Variable length
}

// VirtIO used ring element
#[repr(C, packed)]
struct VirtioUsedElem {
    id: u32,
    len: u32,
}

// VirtIO used ring
#[repr(C, packed)]
struct VirtioUsed {
    flags: u16,
    idx: u16,
    ring: [VirtioUsedElem; 0], // Variable length
}

// VirtIO block request header
#[repr(C, packed)]
struct VirtioBlkReq {
    req_type: u32,
    reserved: u32,
    sector: u64,
}

pub struct VirtioBlockDevice {
    device_features: u64,
    driver_features: u64,
    device_features_sel: u32,
    driver_features_sel: u32,
    status: u8,
    queue_sel: u16,
    queue: VirtQueue,
    disk_image: Option<Vec<u8>>,
    disk_size: u64,
}

impl VirtioBlockDevice {
    pub fn new(disk_size: u64) -> Self {
        Self {
            device_features: VIRTIO_BLK_F_FLUSH | VIRTIO_BLK_F_BLK_SIZE,
            driver_features: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            status: 0,
            queue_sel: 0,
            queue: VirtQueue {
                desc_addr: 0,
                avail_addr: 0,
                used_addr: 0,
                size: 0,
                ready: false,
            },
            disk_image: None,
            disk_size,
        }
    }

    pub fn set_disk_image(&mut self, image: Vec<u8>) {
        self.disk_image = Some(image);
        self.disk_size = self.disk_image.as_ref().map(|img| img.len() as u64).unwrap_or(0);
    }

    pub fn handle_mmio_read(&self, offset: u64) -> Result<u32, String> {
        let reg = offset & 0xFFF;
        
        match reg {
            VIRTIO_MMIO_MAGIC_VALUE => Ok(VIRTIO_MAGIC),
            VIRTIO_MMIO_VERSION => Ok(VIRTIO_VERSION),
            VIRTIO_MMIO_DEVICE_ID => Ok(VIRTIO_DEVICE_ID_BLOCK),
            VIRTIO_MMIO_VENDOR_ID => Ok(VIRTIO_VENDOR_ID),
            VIRTIO_MMIO_DEVICE_FEATURES => {
                let features = if self.device_features_sel == 0 {
                    self.device_features as u32
                } else {
                    (self.device_features >> 32) as u32
                };
                Ok(features)
            }
            VIRTIO_MMIO_QUEUE_NUM_MAX => Ok(256), // Maximum queue size
            VIRTIO_MMIO_QUEUE_READY => Ok(if self.queue.ready { 1 } else { 0 }),
            VIRTIO_MMIO_INTERRUPT_STATUS => Ok(0), // No interrupts pending
            VIRTIO_MMIO_STATUS => Ok(self.status as u32),
            VIRTIO_MMIO_CONFIG_GENERATION => Ok(0),
            VIRTIO_MMIO_CONFIG => {
                // Block device config: capacity (in 512-byte sectors)
                let config_offset = (offset - VIRTIO_MMIO_CONFIG) as usize;
                match config_offset {
                    0 => Ok((self.disk_size / 512) as u32), // Capacity low
                    4 => Ok((self.disk_size / 512 >> 32) as u32), // Capacity high
                    _ => Ok(0),
                }
            }
            _ => {
                // Unknown register
                Ok(0)
            }
        }
    }

    pub fn handle_mmio_write(&mut self, offset: u64, value: u32, partition: &mut crate::partition::Partition) -> Result<(), String> {
        let reg = offset & 0xFFF;
        
        match reg {
            VIRTIO_MMIO_DEVICE_FEATURES_SEL => {
                self.device_features_sel = value;
            }
            VIRTIO_MMIO_DRIVER_FEATURES => {
                if self.driver_features_sel == 0 {
                    self.driver_features = (self.driver_features & 0xFFFFFFFF00000000) | (value as u64);
                } else {
                    self.driver_features = (self.driver_features & 0xFFFFFFFF) | ((value as u64) << 32);
                }
            }
            VIRTIO_MMIO_DRIVER_FEATURES_SEL => {
                self.driver_features_sel = value;
            }
            VIRTIO_MMIO_QUEUE_SEL => {
                self.queue_sel = value as u16;
            }
            VIRTIO_MMIO_QUEUE_NUM => {
                self.queue.size = value as u16;
            }
            VIRTIO_MMIO_QUEUE_READY => {
                self.queue.ready = value != 0;
            }
            VIRTIO_MMIO_QUEUE_DESC_LOW => {
                self.queue.desc_addr = (self.queue.desc_addr & 0xFFFFFFFF00000000) | (value as u64);
            }
            VIRTIO_MMIO_QUEUE_DESC_HIGH => {
                self.queue.desc_addr = (self.queue.desc_addr & 0xFFFFFFFF) | ((value as u64) << 32);
            }
            VIRTIO_MMIO_QUEUE_AVAIL_LOW => {
                self.queue.avail_addr = (self.queue.avail_addr & 0xFFFFFFFF00000000) | (value as u64);
            }
            VIRTIO_MMIO_QUEUE_AVAIL_HIGH => {
                self.queue.avail_addr = (self.queue.avail_addr & 0xFFFFFFFF) | ((value as u64) << 32);
            }
            VIRTIO_MMIO_QUEUE_USED_LOW => {
                self.queue.used_addr = (self.queue.used_addr & 0xFFFFFFFF00000000) | (value as u64);
            }
            VIRTIO_MMIO_QUEUE_USED_HIGH => {
                self.queue.used_addr = (self.queue.used_addr & 0xFFFFFFFF) | ((value as u64) << 32);
            }
            VIRTIO_MMIO_QUEUE_NOTIFY => {
                // Process the queue when notified
                if (value as u16) < self.queue.size && self.queue.ready {
                    self.process_queue(partition)?;
                }
            }
            VIRTIO_MMIO_STATUS => {
                self.status = value as u8;
            }
            VIRTIO_MMIO_INTERRUPT_ACK => {
                // Acknowledge interrupt (we don't generate interrupts yet)
            }
            _ => {
                // Unknown register write - ignore
            }
        }
        
        Ok(())
    }

    fn process_queue(&mut self, partition: &mut crate::partition::Partition) -> Result<(), String> {
        if !self.queue.ready || self.queue.desc_addr == 0 || self.queue.avail_addr == 0 || self.queue.used_addr == 0 {
            return Ok(());
        }

        let disk_image = self.disk_image.as_ref().ok_or("No disk image loaded")?;

        // Read the available ring to get the index of the next descriptor
        let avail_idx_addr = self.queue.avail_addr + 2; // Skip flags
        let avail_idx = partition.read_memory_u16_gpa(avail_idx_addr)?;
        
        // Read the ring array at avail_idx
        let ring_idx_addr = avail_idx_addr + 2 + (avail_idx as u64 % self.queue.size as u64) * 2;
        let desc_idx = partition.read_memory_u16_gpa(ring_idx_addr)?;

        // Read the descriptor chain
        let desc_addr = self.queue.desc_addr + (desc_idx as u64 * 16); // Each descriptor is 16 bytes
        
        // Read descriptor
        let desc = partition.read_memory_gpa(desc_addr, 16)?;
        let req_addr = u64::from_le_bytes([
            desc[0], desc[1], desc[2], desc[3],
            desc[4], desc[5], desc[6], desc[7],
        ]);
        let _req_len = u32::from_le_bytes([desc[8], desc[9], desc[10], desc[11]]) as usize;
        let _flags = u16::from_le_bytes([desc[12], desc[13]]);
        let next = u16::from_le_bytes([desc[14], desc[15]]);

        // Read the request header (first descriptor should be the request)
        let req_data = partition.read_memory_gpa(req_addr, std::mem::size_of::<VirtioBlkReq>())?;
        let req_type = u32::from_le_bytes([req_data[0], req_data[4], req_data[8], req_data[12]]);
        let sector = u64::from_le_bytes([
            req_data[16], req_data[17], req_data[18], req_data[19],
            req_data[20], req_data[21], req_data[22], req_data[23],
        ]);

        // Find the data descriptor (next in chain)
        let data_desc_addr = self.queue.desc_addr + (next as u64 * 16);
        let data_desc = partition.read_memory_gpa(data_desc_addr, 16)?;
        let data_addr = u64::from_le_bytes([
            data_desc[0], data_desc[1], data_desc[2], data_desc[3],
            data_desc[4], data_desc[5], data_desc[6], data_desc[7],
        ]);
        let data_len = u32::from_le_bytes([data_desc[8], data_desc[9], data_desc[10], data_desc[11]]) as usize;

        // Find the status descriptor (last in chain)
        let status_desc_idx = u16::from_le_bytes([data_desc[14], data_desc[15]]);
        let status_desc_addr = self.queue.desc_addr + (status_desc_idx as u64 * 16);
        let status_desc = partition.read_memory_gpa(status_desc_addr, 16)?;
        let status_addr = u64::from_le_bytes([
            status_desc[0], status_desc[1], status_desc[2], status_desc[3],
            status_desc[4], status_desc[5], status_desc[6], status_desc[7],
        ]);

        // Process the request
        let status = match req_type {
            VIRTIO_BLK_T_IN => {
                // Read from disk
                let offset = (sector * 512) as usize;
                if offset + data_len <= disk_image.len() {
                    let data = &disk_image[offset..offset + data_len];
                    partition.write_memory_gpa(data_addr, data)?;
                    VIRTIO_BLK_S_OK
                } else {
                    VIRTIO_BLK_S_IOERR
                }
            }
            VIRTIO_BLK_T_OUT => {
                // Write to disk
                let offset = (sector * 512) as usize;
                if offset + data_len <= disk_image.len() {
                    let _data = partition.read_memory_gpa(data_addr, data_len)?;
                    // Note: We'd need mutable access to disk_image to write, but for now we'll just return OK
                    // In a full implementation, you'd update the disk image here
                    VIRTIO_BLK_S_OK
                } else {
                    VIRTIO_BLK_S_IOERR
                }
            }
            VIRTIO_BLK_T_FLUSH => {
                // Flush - no-op for now
                VIRTIO_BLK_S_OK
            }
            _ => {
                VIRTIO_BLK_S_UNSUPP
            }
        };

        // Write status to status descriptor
        partition.write_memory_gpa(status_addr, &[status])?;

        // Update used ring
        let used_idx_addr = self.queue.used_addr + 2; // Skip flags
        let used_idx = partition.read_memory_u16_gpa(used_idx_addr)?;
        let used_elem_addr = used_idx_addr + 2 + (used_idx as u64 % self.queue.size as u64) * 8;
        
        // Write used element: id and len
        let used_elem = [
            (desc_idx as u32).to_le_bytes(),
            (1u32).to_le_bytes(), // len = 1 (status byte)
        ].concat();
        partition.write_memory_gpa(used_elem_addr, &used_elem)?;
        
        // Update used ring index
        let new_used_idx = (used_idx + 1) % (self.queue.size as u16 * 2);
        partition.write_memory_u16_gpa(used_idx_addr, new_used_idx)?;

        Ok(())
    }
}

// Helper methods use Partition's public GPA methods
