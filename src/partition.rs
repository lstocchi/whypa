use windows::Win32::System::{
    Hypervisor::*,
    Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc},
};
use std::result::Result;
use std::ptr;
use std::fs::File;
use std::io::Read;

use crate::virtio::VirtioBlockDevice;

pub struct Partition {
    handle: WHV_PARTITION_HANDLE,
    memory: Option<*mut std::ffi::c_void>,
    memory_size: usize,
    disk_image: Option<Vec<u8>>,
    disk_gpa: Option<u64>, // Guest Physical Address where disk is mapped
    virtio_device: Option<VirtioBlockDevice>,
}

const DEFAULT_VM_MEMORY_SIZE: usize = 2 * 1024 * 1024 * 1024; // 2 GB default for OS boot
const DISK_BASE_GPA: u64 = 0x100000000; // 4GB - typical location for disk in memory-mapped I/O

impl Partition {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let handle = WHvCreatePartition()
                .map_err(|e| e.to_string())?;

            Ok(Self {
                handle,
                memory: None,
                memory_size: DEFAULT_VM_MEMORY_SIZE,
                disk_image: None,
                disk_gpa: None,
                virtio_device: None,
            })
        }
    }

    pub fn delete(self) -> Result<(), String> {
        unsafe {
            WHvDeletePartition(self.handle).map_err(|e| e.to_string())?;

            Ok(())
        }
    }

    pub fn configure(&self, processor_count: u32) -> Result<(), String> {
        unsafe {

            let processor_count = WHV_PARTITION_PROPERTY {
                ProcessorCount: processor_count,
            };
            WHvSetPartitionProperty(
                self.handle,
                WHvPartitionPropertyCodeProcessorCount, 
                &processor_count as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
            ).map_err(|e| e.to_string())?;

            Ok(())
        }
    }

    pub fn setup(&self) -> Result<(), String> {
        unsafe {
            WHvSetupPartition(self.handle).map_err(|e| e.to_string())?;
            Ok(())
        }
    }

    pub fn create_vp(&self, vp_id: u32) -> Result<(), String> {
        unsafe {
            // flags is unused and must be zero - https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvcreatevirtualprocessor
            WHvCreateVirtualProcessor(self.handle, vp_id, 0).map_err(|e| e.to_string())?;
            Ok(())
        }
    }

    pub fn allocate_memory(&mut self) -> Result<(), String> {
        self.allocate_memory_with_size(self.memory_size)
    }

    pub fn allocate_memory_with_size(&mut self, size: usize) -> Result<(), String> {
        unsafe {
            let memory = VirtualAlloc(Some(ptr::null()), size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);

            if memory.is_null() {
                return Err("Failed to allocate memory".to_string());
            }

            WHvMapGpaRange(self.handle, memory, 0, size as u64, WHvMapGpaRangeFlagExecute | WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite).map_err(|e| e.to_string())?;

            self.memory = Some(memory);
            self.memory_size = size;

            Ok(())
        }
    }

    pub fn load_disk_image(&mut self, image_path: &str) -> Result<(), String> {
        println!("Loading disk image from: {}", image_path);
        
        let mut file = File::open(image_path)
            .map_err(|e| format!("Failed to open disk image: {}", e))?;
        
        let mut image_data = Vec::new();
        file.read_to_end(&mut image_data)
            .map_err(|e| format!("Failed to read disk image: {}", e))?;
        
        println!("Loaded {} bytes from disk image", image_data.len());
        
        self.disk_image = Some(image_data.clone());
        
        // Initialize VirtIO device with disk image
        let disk_size = image_data.len() as u64;
        let mut virtio = VirtioBlockDevice::new(disk_size);
        virtio.set_disk_image(image_data);
        self.virtio_device = Some(virtio);
        
        Ok(())
    }

    pub fn load_uefi_firmware(&self, firmware_path: &str) -> Result<(), String> {
        println!("Loading UEFI firmware from: {}", firmware_path);
        
        let mut file = File::open(firmware_path)
            .map_err(|e| format!("Failed to open UEFI firmware: {}", e))?;
        
        let mut firmware_data = Vec::new();
        file.read_to_end(&mut firmware_data)
            .map_err(|e| format!("Failed to read UEFI firmware: {}", e))?;
        
        println!("Loaded {} bytes of UEFI firmware", firmware_data.len());
        
        // UEFI firmware for 64-bit systems typically starts at 0x100000 (1MB)
        // This is the standard entry point for OVMF (Open Virtual Machine Firmware)
        const UEFI_BASE_ADDRESS: usize = 0x100000;
        
        if UEFI_BASE_ADDRESS + firmware_data.len() > self.memory_size {
            return Err(format!(
                "UEFI firmware ({} bytes) exceeds available memory at address 0x{:X}",
                firmware_data.len(),
                UEFI_BASE_ADDRESS
            ));
        }
        
        // Write the firmware to memory at the UEFI base address
        self.write_code(&firmware_data, UEFI_BASE_ADDRESS)?;
        
        println!("UEFI firmware loaded at address 0x{:X}", UEFI_BASE_ADDRESS);
        
        Ok(())
    }

    pub fn map_disk_image(&mut self) -> Result<(), String> {
        let disk_data = self.disk_image.as_ref()
            .ok_or("No disk image loaded".to_string())?;
        
        unsafe {
            // Allocate memory for the disk image
            let disk_size = disk_data.len();
            let disk_memory = VirtualAlloc(
                Some(ptr::null()),
                disk_size,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE
            );

            if disk_memory.is_null() {
                return Err("Failed to allocate memory for disk image".to_string());
            }

            // Copy disk data to allocated memory
            ptr::copy_nonoverlapping(
                disk_data.as_ptr(),
                disk_memory as *mut u8,
                disk_size
            );

            // Map the disk to guest physical address space
            // Using DISK_BASE_GPA as the base address
            WHvMapGpaRange(
                self.handle,
                disk_memory,
                DISK_BASE_GPA,
                disk_size as u64,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite
            ).map_err(|e| format!("Failed to map disk image: {}", e))?;

            self.disk_gpa = Some(DISK_BASE_GPA);
            println!("Mapped disk image to GPA: 0x{:X} ({} bytes)", DISK_BASE_GPA, disk_size);
            
            Ok(())
        }
    }

    pub fn map_virtio_mmio(&mut self) -> Result<(), String> {
        unsafe {
            // Allocate memory for VirtIO MMIO region
            let virtio_memory = VirtualAlloc(
                Some(ptr::null()),
                crate::virtio::VIRTIO_MMIO_SIZE as usize,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE
            );

            if virtio_memory.is_null() {
                return Err("Failed to allocate memory for VirtIO MMIO".to_string());
            }

            // Initialize to zero
            ptr::write_bytes(virtio_memory, 0, crate::virtio::VIRTIO_MMIO_SIZE as usize);

            // Map the VirtIO MMIO region to guest physical address space
            WHvMapGpaRange(
                self.handle,
                virtio_memory,
                crate::virtio::VIRTIO_MMIO_BASE,
                crate::virtio::VIRTIO_MMIO_SIZE,
                WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite
            ).map_err(|e| format!("Failed to map VirtIO MMIO: {}", e))?;

            println!("Mapped VirtIO MMIO region to GPA: 0x{:X} ({} bytes)", 
                     crate::virtio::VIRTIO_MMIO_BASE, crate::virtio::VIRTIO_MMIO_SIZE);
            
            Ok(())
        }
    }

    pub fn set_memory_size(&mut self, size: usize) {
        self.memory_size = size;
    }

    pub fn write_code(&self, code: &[u8], offset: usize) -> Result<(), String> {
        unsafe {
            let memory = self.memory.ok_or("Memory not allocated".to_string())?;
            if offset + code.len() > self.memory_size {
                return Err("Code exceeds allocated memory".to_string());
            }
            ptr::copy_nonoverlapping(code.as_ptr(), memory.add(offset) as *mut u8, code.len());
            Ok(())
        }
    }

    pub fn read_memory(&self, offset: usize, size: usize) -> Result<Vec<u8>, String> {
        unsafe {
            let memory = self.memory.ok_or("Memory not allocated".to_string())?;
            if offset + size > self.memory_size {
                return Err("Read exceeds allocated memory".to_string());
            }
            let mut data = vec![0u8; size];
            ptr::copy_nonoverlapping(memory.add(offset) as *const u8, data.as_mut_ptr(), size);
            Ok(data)
        }
    }

    pub fn read_memory_gpa(&self, gpa: u64, size: usize) -> Result<Vec<u8>, String> {
        // For now, assume GPA maps directly to host memory offset
        // In a full implementation, you'd need proper GPA to HVA translation
        if gpa as usize + size > self.memory_size {
            return Err(format!("GPA read exceeds memory: GPA=0x{:X}, size={}", gpa, size));
        }
        self.read_memory(gpa as usize, size)
    }

    pub fn write_memory_gpa(&self, gpa: u64, data: &[u8]) -> Result<(), String> {
        // For now, assume GPA maps directly to host memory offset
        if gpa as usize + data.len() > self.memory_size {
            return Err(format!("GPA write exceeds memory: GPA=0x{:X}, size={}", gpa, data.len()));
        }
        unsafe {
            let memory = self.memory.ok_or("Memory not allocated".to_string())?;
            ptr::copy_nonoverlapping(data.as_ptr(), memory.add(gpa as usize) as *mut u8, data.len());
            Ok(())
        }
    }

    pub fn read_memory_u16_gpa(&self, gpa: u64) -> Result<u16, String> {
        let data = self.read_memory_gpa(gpa, 2)?;
        Ok(u16::from_le_bytes([data[0], data[1]]))
    }

    pub fn write_memory_u16_gpa(&self, gpa: u64, value: u16) -> Result<(), String> {
        let bytes = value.to_le_bytes();
        self.write_memory_gpa(gpa, &bytes)
    }

    pub fn setup_registers(&self, vp_id: u32, rip: u64) -> Result<(), String> {
        unsafe {
            // Set RIP (Instruction Pointer) to point to the entry point
            let mut rip_reg = WHV_REGISTER_VALUE {
                Reg64: rip,
            };
            WHvSetVirtualProcessorRegisters(
                self.handle,
                vp_id,
                &[WHvX64RegisterRip] as *const _,
                1,
                &mut rip_reg as *mut _ as *mut WHV_REGISTER_VALUE,
            ).map_err(|e| e.to_string())?;

            // Set RFLAGS (default value)
            let mut rflags_reg = WHV_REGISTER_VALUE {
                Reg64: 0x2, // Bit 1 (reserved) is always set
            };
            WHvSetVirtualProcessorRegisters(
                self.handle,
                vp_id,
                &[WHvX64RegisterRflags] as *const _,
                1,
                &mut rflags_reg as *mut _ as *mut WHV_REGISTER_VALUE,
            ).map_err(|e| e.to_string())?;

            // Set up segment registers for proper boot
            let mut cs_reg = WHV_REGISTER_VALUE {
                Reg16: 0x8, // Code segment selector
            };
            WHvSetVirtualProcessorRegisters(
                self.handle,
                vp_id,
                &[WHvX64RegisterCs] as *const _,
                1,
                &mut cs_reg as *mut _ as *mut WHV_REGISTER_VALUE,
            ).map_err(|e| e.to_string())?;

            Ok(())
        }
    }

    pub fn setup_registers_for_boot(&self, vp_id: u32, boot_address: u64) -> Result<(), String> {
        // For UEFI boot, typically starts at 0x100000 (1MB)
        // For legacy BIOS boot, starts at 0x7C00
        self.setup_registers(vp_id, boot_address)
    }

    pub fn run_vp(&self, vp_id: u32) -> Result<WHV_RUN_VP_EXIT_CONTEXT, String> {
        unsafe {
            let mut exit_context = WHV_RUN_VP_EXIT_CONTEXT::default();
            WHvRunVirtualProcessor(
                self.handle,
                vp_id,
                &mut exit_context as *mut _ as *mut std::ffi::c_void,
                std::mem::size_of::<WHV_RUN_VP_EXIT_CONTEXT>() as u32,
            ).map_err(|e| e.to_string())?;

            Ok(exit_context)
        }
    }

    pub fn get_memory_size(&self) -> usize {
        self.memory_size
    }

    pub fn get_handle(&self) -> WHV_PARTITION_HANDLE {
        self.handle
    }

    pub fn handle_exit(&mut self, _vp_id: u32, exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<bool, String> {
        // Returns true if we should continue running, false if we should stop
        let exit_reason = exit_context.ExitReason.0;
        
        match exit_reason {
            x if x == WHvRunVpExitReasonNone.0 => {
                println!("Unexpected exit reason: None");
                Ok(false)
            }
            x if x == WHvRunVpExitReasonMemoryAccess.0 => {
                unsafe {
                    let gpa = exit_context.Anonymous.MemoryAccess.Gpa;
                    let access_info = exit_context.Anonymous.MemoryAccess.AccessInfo;
                    
                    // Check if this is a VirtIO MMIO access
                    if gpa >= crate::virtio::VIRTIO_MMIO_BASE && gpa < crate::virtio::VIRTIO_MMIO_BASE + crate::virtio::VIRTIO_MMIO_SIZE {
                        let offset = gpa - crate::virtio::VIRTIO_MMIO_BASE;
                        
                        // Determine if this is a read or write
                        // AccessInfo: bit 0 indicates write (1) or read (0)
                        let access_value = access_info.AsUINT32;
                        let is_write = (access_value & 0x1) != 0;
                        
                        if is_write {
                            // For MMIO writes, we need to get the value from RAX register
                            // Read RAX to get the value being written
                            let mut rax_reg = WHV_REGISTER_VALUE::default();
                            WHvGetVirtualProcessorRegisters(
                                self.handle,
                                0,
                                &[WHvX64RegisterRax] as *const _,
                                1,
                                &mut rax_reg as *mut _ as *mut WHV_REGISTER_VALUE,
                            ).map_err(|e| format!("Failed to read RAX: {}", e))?;
                            
                            let value = rax_reg.Reg32;
                            // Temporarily take ownership to avoid double borrow
                            if let Some(mut virtio) = self.virtio_device.take() {
                                virtio.handle_mmio_write(offset, value, self)?;
                                self.virtio_device = Some(virtio);
                            }
                        } else {
                            // MMIO read - get value and write to RAX
                            let value = if let Some(virtio) = self.virtio_device.as_ref() {
                                virtio.handle_mmio_read(offset)?
                            } else {
                                0
                            };
                            
                            // Write value back to RAX
                            let mut rax_reg = WHV_REGISTER_VALUE::default();
                            rax_reg.Reg32 = value;
                            WHvSetVirtualProcessorRegisters(
                                self.handle,
                                0,
                                &[WHvX64RegisterRax] as *const _,
                                1,
                                &mut rax_reg as *mut _ as *mut WHV_REGISTER_VALUE,
                            ).map_err(|e| format!("Failed to write RAX: {}", e))?;
                        }
                    } else {
                        // Not a VirtIO MMIO access - might be other MMIO or unmapped memory
                        // For now, just log it
                        if gpa < self.memory_size as u64 {
                            // It's in our mapped memory, so allow the access
                            // The hypervisor should handle this automatically
                        } else {
                            println!("Memory access exit at unmapped GPA: 0x{:X}", gpa);
                        }
                    }
                }
                Ok(true)
            }
            x if x == WHvRunVpExitReasonX64IoPortAccess.0 => {
                unsafe {
                    let port = exit_context.Anonymous.IoPortAccess.PortNumber;
                    println!("I/O port access: port=0x{:X}", port);
                }
                // For disk I/O, you'd need to implement IDE/AHCI controller emulation here
                Ok(true)
            }
            x if x == WHvRunVpExitReasonUnrecoverableException.0 => {
                unsafe {
                    println!("Unrecoverable exception: {}", exit_context.Anonymous.VpException.ExceptionType);
                }
                Ok(false)
            }
            x if x == WHvRunVpExitReasonInvalidVpRegisterValue.0 => {
                println!("Invalid VP register value");
                Ok(false)
            }
            x if x == WHvRunVpExitReasonUnsupportedFeature.0 => {
                println!("Unsupported feature");
                Ok(false)
            }
            x if x == WHvRunVpExitReasonX64InterruptWindow.0 => {
                println!("Interrupt window");
                Ok(true)
            }
            x if x == WHvRunVpExitReasonX64Halt.0 => {
                println!("VM halted");
                Ok(false)
            }
            x if x == WHvRunVpExitReasonX64ApicEoi.0 => {
                println!("APIC EOI");
                Ok(true)
            }
            _ => {
                println!("Unknown exit reason: {}", exit_reason);
                Ok(false)
            }
        }
    }
}