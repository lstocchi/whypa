use windows::Win32::System::{Hypervisor::*, Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc}};
use std::result::Result;
use std::ptr;

pub struct Partition {
    handle: WHV_PARTITION_HANDLE,
}

const VM_MEMORY_SIZE: usize = 16 * 1024 * 1024; // 16 MB

impl Partition {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let handle = WHvCreatePartition()
                .map_err(|e| e.to_string())?;

            Ok(Self {
                handle,
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

    pub fn allocate_memory(&self) -> Result<(), String> {
        unsafe {

            let memory = VirtualAlloc(Some(ptr::null()), VM_MEMORY_SIZE, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);

            if memory.is_null() {
                return Err("Failed to allocate memory".to_string());
            }

            WHvMapGpaRange(self.handle, memory, 0, VM_MEMORY_SIZE as u64, WHvMapGpaRangeFlagExecute | WHvMapGpaRangeFlagRead | WHvMapGpaRangeFlagWrite).map_err(|e| e.to_string())?;

            Ok(())
        }
    }
}