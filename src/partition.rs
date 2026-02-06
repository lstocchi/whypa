use windows::Win32::System::{
    Hypervisor::*,
    Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    },
};
use windows::Win32::Foundation::{HANDLE, CloseHandle};
use anyhow::Result;
use std::ptr;

use crate::memory::{MemoryPerms, Memory, MemoryRegion, MemoryAccessViolation};

pub struct Partition {
    handle: WHV_PARTITION_HANDLE,
    is_shared_memory: bool, // Whether memory is from shared mapping
    injected_shared_regions: Vec<InjectedSharedRegion>, // Track injected shared memory regions
    memory: Memory, // Memory debugging and tracking
}

#[derive(Clone)]
struct InjectedSharedRegion {
    gpa: u64,
    size: usize,
    memory_ptr: *mut std::ffi::c_void,
    mapping_handle: Option<HANDLE>, // Handle to file mapping (if created by us)
}

// Partition is safe to send between threads because:
// - WHV_PARTITION_HANDLE is a handle that can be used from any thread
// - The raw pointers point to memory that remains valid and won't be freed from another thread
// - All operations are synchronized through the Windows Hypervisor API
unsafe impl Send for Partition {}

const DEFAULT_VM_MEMORY_SIZE: usize = 2 * 1024 * 1024 * 1024; // 2 GB default for OS boot

impl Partition {
    /// Create a new partition (existing behavior)
    pub fn new() -> Result<Self> {
        unsafe {
            let handle = WHvCreatePartition()?;

            Ok(Self {
                handle,
                is_shared_memory: false,
                injected_shared_regions: Vec::new(),
                memory: Memory::new(),
            })
        }
    }

    pub fn configure(&self, processor_count: u32) -> Result<()> {
        unsafe {

            let processor_count = WHV_PARTITION_PROPERTY {
                ProcessorCount: processor_count,
            };
            WHvSetPartitionProperty(
                self.handle,
                WHvPartitionPropertyCodeProcessorCount, 
                &processor_count as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
            )?;

            Ok(())
        }
    }

    pub fn setup(&self) -> Result<()> {
        unsafe {
            WHvSetupPartition(self.handle)?;
            Ok(())
        }
    }

    pub fn create_vp(&self, vp_id: u32) -> Result<()> {
        unsafe {
            // flags is unused and must be zero - https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvcreatevirtualprocessor
            WHvCreateVirtualProcessor(self.handle, vp_id, 0)?;
            Ok(())
        }
    }

    pub fn allocate_memory(&mut self) -> Result<()> {
        self.allocate_memory_with_size(4 * 1024 * 1024, MemoryPerms::RWX)
    }

    pub fn allocate_memory_with_size(&mut self, size: usize, flags: MemoryPerms) -> Result<()> {
        unsafe {
            let source = VirtualAlloc(Some(ptr::null()), size, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);

            if source.is_null() {
                return Err(anyhow::anyhow!("Failed to allocate memory"));
            }

            WHvMapGpaRange(
                self.handle,
                source,
                0,
                size as u64,
                flags.to_flags()
            )?;

            self.memory.register_region(MemoryRegion {
                gpa: 0,
                size: size as u64,
                perms: MemoryPerms::RWX,
                hpa: Some(source),
                description: "Main VM Memory".to_string(),
            });

            Ok(())
        }
    }

    pub fn write_code(&self, code: &[u8], gpa: u64) -> Result<()> {
        let region = self.memory.find_region(gpa).ok_or(anyhow::anyhow!("Memory not allocated"))?;
        if gpa + code.len() as u64 > region.gpa + region.size {
            return Err(anyhow::anyhow!("Code exceeds allocated memory"));
        }
        if !region.perms.contains(MemoryPerms::WRITE) {
            return Err(anyhow::anyhow!("Memory is read-only"));
        }
        let hpa = region.hpa.ok_or(anyhow::anyhow!("Region has no host physical address"))?;
        let offset = (gpa - region.gpa) as usize;

        unsafe {
            ptr::copy_nonoverlapping(code.as_ptr(), hpa.add(offset) as *mut u8, code.len());
        }
        Ok(())
    }

    pub fn read_memory(&self, gpa: u64, size: usize) -> Result<Vec<u8>> {
        unsafe {
            let region = self.memory.find_region(gpa).ok_or(anyhow::anyhow!("Memory not allocated"))?;
            if gpa + size as u64 > region.gpa + region.size {
                return Err(anyhow::anyhow!("Read exceeds allocated memory"));
            }

            if !region.perms.contains(MemoryPerms::READ) {
                let violation = MemoryAccessViolation {
                    gpa,
                    action: MemoryPerms::READ,
                    access_size: size as u32,
                    instruction_rip: 0,
                };
                return Err(anyhow::anyhow!("Memory access violation: {}", self.memory.analyze_violation(&violation)));
            }

            let hpa = region.hpa.ok_or(anyhow::anyhow!("Region has no host physical address"))?;
            let offset = (gpa - region.gpa) as usize;
            let mut data = vec![0u8; size];
            ptr::copy_nonoverlapping(hpa.add(offset) as *const u8, data.as_mut_ptr(), size);
            Ok(data)
        }
    }

    pub fn setup_registers(&self, vp_id: u32, rip: u64) -> Result<()> {
        unsafe {
            // Set RFLAGS to a safe default value
            // 0x2 = bit 1 (reserved, always set)
            // Note: Do NOT set bit 9 (interrupts enabled) to 0x202 - this causes "Invalid VP register value" error
            let mut rflags_reg = WHV_REGISTER_VALUE {
                Reg64: 0x2,
            };
            WHvSetVirtualProcessorRegisters(
                self.handle,
                vp_id,
                &[WHvX64RegisterRflags] as *const _,
                1,
                &mut rflags_reg as *mut _ as *mut WHV_REGISTER_VALUE,
            )?;
            
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
            )?;

            // Don't set CS - let hypervisor use default
            // Setting CS to 0x8 can cause issues if VM isn't in long mode

            Ok(())
        }
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

    fn advance_rip(vp_context: WHV_VP_EXIT_CONTEXT, handle: WHV_PARTITION_HANDLE, vp_id: u32) -> Result<()> {
        // Access WHV_VP_EXIT_CONTEXT to get RIP and InstructionLength
        // According to Microsoft docs: https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvexitcontextdatatypes
        // InstructionLength is in the lower 4 bits of _bitfield (bits 0-3)
        // Cr8 is in the upper 4 bits of _bitfield (bits 4-7)
        let current_rip = vp_context.Rip;
        
        // Extract InstructionLength from the lower 4 bits of _bitfield
        // InstructionLength : 4 means it uses bits 0-3
        let instruction_length = (vp_context._bitfield & 0x0F) as u64;
        
        println!("Current RIP: 0x{:X}, Instruction length: {} bytes", current_rip, instruction_length);
        
        // Advance RIP past the instruction using the actual instruction length from the exit context
        let new_rip = current_rip + instruction_length;
        
        // Update RIP register
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
        
        
        println!("Advanced RIP to 0x{:X} (skipped {} byte instruction)", new_rip, instruction_length);
        Ok(())
    }

    pub fn handle_exit(&mut self, vp_id: u32, exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<bool> {
        // Returns true if we should continue running, false if we should stop
        // Safely read exit reason - this should always be valid
        // Use a pointer to avoid potential issues with union access
        // Access ExitReason field directly - it's not part of the union, so it's always safe
        let exit_reason = exit_context.ExitReason.0;
        
        // Debug: print exit reason
        println!("VM exit reason: {} (0x{:X})", exit_reason, exit_reason);
        
        // Match on exit reason - only access union fields in the corresponding match arm
        // Each match arm must only access the union member that corresponds to that exit reason
        match exit_reason {
            x if x == WHvRunVpExitReasonNone.0 => {
                println!("Unexpected exit reason: None");
                Ok(false)
            }
            x if x == WHvRunVpExitReasonMemoryAccess.0 => {
                println!("Unsupported feature");
                Ok(false)
            }
            x if x == WHvRunVpExitReasonX64IoPortAccess.0 => {
                unsafe {
                    // Only access IoPortAccess fields when exit reason is IoPortAccess
                    let io_access = &exit_context.Anonymous.IoPortAccess;
                    let port = io_access.PortNumber;
                    println!("I/O port access: port=0x{:X}", port);

                    Self::advance_rip(exit_context.VpContext, self.handle, vp_id)?;
                }
                // Return true to continue execution - now it should execute the 'hlt' instruction
                Ok(true)
            }
            x if x == WHvRunVpExitReasonUnrecoverableException.0 => {
                unsafe {
                    // Only access VpException fields when exit reason is UnrecoverableException
                    let vp_exception = &exit_context.Anonymous.VpException;
                    println!("Unrecoverable exception: {}", vp_exception.ExceptionType);
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
                // Read register values to debug
                
                
                let data = self.read_memory(0x2000, 4)?;
                println!("Memory at 0x2000: {:?}", data);
                let data_32 = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                println!("Memory at 0x2000: 0x{:X} ({})", data_32, data_32);
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

impl Drop for Partition {
    fn drop(&mut self) {
        unsafe {
            // Clean up injected shared memory regions
            for region in &self.injected_shared_regions {
                // Unmap from guest GPA space
                let _ = WHvUnmapGpaRange(self.handle, region.gpa, region.size as u64);
                
                // Unmap view and close handle if we created it
                if let Some(handle) = region.mapping_handle {
                    use windows::Win32::System::Memory::{UnmapViewOfFile, MEMORY_MAPPED_VIEW_ADDRESS};
                    let memory = MEMORY_MAPPED_VIEW_ADDRESS { Value: region.memory_ptr };
                    let _ = UnmapViewOfFile(memory);
                    let _ = CloseHandle(handle);
                }
            }

            WHvDeletePartition(self.handle);
        }
    }
}