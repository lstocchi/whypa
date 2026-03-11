use windows::Win32::System::{
    Hypervisor::*,
    Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    },
};

use anyhow::Result;
use anyhow::Context;
use std::{ffi::c_void, ptr};
use std::fs;

use crate::{cpu::CpuManager, device_manager::DeviceManager, devices::bus::BusDevice, emulator::Emulator, linux_boot::{self, LinuxBootPartition}, memory::{layout, memory::{GuestAddress, MemoryAccessViolation, MemoryManager, MemoryPerms, MemoryRegion, MmioRegion}}};
use std::collections::HashMap;

pub struct Partition {
    pub handle: WHV_PARTITION_HANDLE,
    pub(crate) emulator: Emulator,
    pub(crate) memory: MemoryManager,
    device_manager: DeviceManager,
    cpu_manager: CpuManager,
    pub(crate) mmio_handlers: HashMap<String, Box<dyn BusDevice>>,
    pub pci_address_config: u32,
    // Hyper-V enlightenment state
    pub(crate) vm_start_tsc: u64,
    pub(crate) tsc_reference_gpa: Option<u64>,
    pub(crate) guest_os_id: u64,
}

// Partition is safe to send between threads because:
// - WHV_PARTITION_HANDLE is a handle that can be used from any thread
// - All operations are synchronized through the Windows Hypervisor API
unsafe impl Send for Partition {}

impl Partition {
    pub fn new(memory_size: usize) -> Result<Self> {
        unsafe {
            let handle = WHvCreatePartition()?;
            let emulator = Emulator::new()?;

            let mut device_manager = DeviceManager::new(GuestAddress(memory_size as u64));
            device_manager.init();
            Ok(Self {
                handle,
                emulator,
                memory: MemoryManager::new(),
                device_manager,
                cpu_manager: CpuManager::new(),
                mmio_handlers: HashMap::new(),
                pci_address_config: 0,
                vm_start_tsc: core::arch::x86_64::_rdtsc(),
                tsc_reference_gpa: None,
                guest_os_id: 0,
            })
        }
    }

    pub fn configure(&mut self, processor_count: u32) -> Result<()> {
        unsafe {
            let processor_count_prop = WHV_PARTITION_PROPERTY {
                ProcessorCount: processor_count,
            };
            WHvSetPartitionProperty(
                self.handle,
                WHvPartitionPropertyCodeProcessorCount, 
                &processor_count_prop as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
            )?;

            self.cpu_manager.set_cpu_count(processor_count);
            Ok(())
        }
    }

    /// Get a reference to the device manager
    pub fn device_manager(&self) -> &DeviceManager {
        &self.device_manager
    }

    /// Get a mutable reference to the device manager
    pub fn device_manager_mut(&mut self) -> &mut DeviceManager {
        &mut self.device_manager
    }

    /// Get a reference to the CPU manager
    pub fn cpu_manager(&self) -> &CpuManager {
        &self.cpu_manager
    }

    /// Get a reference to the memory manager
    pub fn memory_manager(&self) -> &MemoryManager {
        &self.memory
    }

    /// Get a mutable reference to MMIO handlers (for emulator callbacks)
    pub(crate) fn mmio_handlers_mut(&mut self) -> &mut HashMap<String, Box<dyn BusDevice>> {
        &mut self.mmio_handlers
    }

    pub fn setup(&self) -> Result<()> {
        unsafe {
            let mut property: WHV_PARTITION_PROPERTY = std::mem::zeroed();
            property.LocalApicEmulationMode = WHV_X64_LOCAL_APIC_EMULATION_MODE(1);

            let result = WHvSetPartitionProperty(
                self.handle,
                WHvPartitionPropertyCodeLocalApicEmulationMode,
                &property as *const _ as *const _,
                std::mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
            );

            if result.is_err() {
                return Err(anyhow::anyhow!("Failed to enable Local APIC emulation: {:?}", result));
            }

            // Configure CPUID exit list for Hyper-V enlightenment leaves.
            let cpuid_exit_list: [u32; 6] = [
                0x40000000, // Hypervisor CPUID leaf range and vendor ID
                0x40000001, // Hypervisor interface identification
                0x40000002, // Hypervisor system identity
                0x40000003, // Hypervisor feature identification
                0x40000004, // Enlightenment recommendations
                0x40000005, // Implementation limits
            ];
            WHvSetPartitionProperty(
                self.handle,
                WHvPartitionPropertyCodeCpuidExitList,
                cpuid_exit_list.as_ptr() as *const _,
                (cpuid_exit_list.len() * std::mem::size_of::<u32>()) as u32,
            )?;

            WHvSetupPartition(self.handle)?;
            Ok(())
        }
    }

    pub fn create_vp(&self, vp_id: u32) -> Result<()> {
        unsafe {
            WHvCreateVirtualProcessor(self.handle, vp_id, 0)?;
            Ok(())
        }
    }

    pub fn allocate_memory_with_size(&mut self, total_memory: u64, flags: MemoryPerms) -> Result<()> {
        unsafe {
            let source = VirtualAlloc(Some(ptr::null()), total_memory as usize, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);

            if source.is_null() {
                return Err(anyhow::anyhow!("Failed to allocate memory"));
            }

            if total_memory <= layout::MEM_32BIT_RESERVED_START.0 {
                WHvMapGpaRange(
                    self.handle,
                    source,
                    0,
                    total_memory,
                    flags.to_flags()
                )?;

                self.memory.register_region(MemoryRegion::new(GuestAddress(0), total_memory, MemoryPerms::RWX, Some(source)));
            } else {
                // Map the lower region (below 32-bit device space)
                let low_size = layout::MEM_32BIT_DEVICES_START.0;

                WHvMapGpaRange(
                    self.handle,
                    source,
                    0,
                    low_size,
                    flags.to_flags()
                )?;

                self.memory.register_region(MemoryRegion::new(GuestAddress(0), low_size, MemoryPerms::RWX, Some(source)));

                let gpa = layout::RAM_64BIT_START.0;
                let size = total_memory - low_size;
                let high_source = (source as *const u8).add(low_size as usize) as *mut c_void;
                WHvMapGpaRange(
                    self.handle,
                    high_source,
                    gpa,
                    size,
                    flags.to_flags()
                )?;

                self.memory.register_region(MemoryRegion::new(GuestAddress(gpa), size, MemoryPerms::RWX, Some(high_source))); 
            }

            Ok(())
        }
    }

    pub fn write_code(&self, code: &[u8], gpa: GuestAddress) -> Result<()> {
        self.memory.write_guest_memory(code, gpa)
    }

    /// Load a file into guest memory at the specified GPA
    pub fn load_file(&self, file_path: &str, gpa: GuestAddress) -> Result<usize> {
        let data = fs::read(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path))?;
        self.write_code(&data, gpa)?;
        Ok(data.len())
    }

    /// Load Linux kernel and set up boot parameters
    pub fn load_linux_kernel(&mut self, kernel_path: &str, initram_path: &str) -> Result<u64> {
        linux_boot::load_linux_kernel(self, kernel_path, initram_path)
    }

    /// Verify current RIP value (for debugging)
    pub fn verify_rip(&self, vp_id: u32) -> Result<u64> {
        unsafe {
            let mut rip_reg = WHV_REGISTER_VALUE::default();
            WHvGetVirtualProcessorRegisters(
                self.handle,
                vp_id,
                &[WHvX64RegisterRip] as *const _,
                1,
                &mut rip_reg as *mut _ as *mut WHV_REGISTER_VALUE,
            )?;
            Ok(rip_reg.Reg64)
        }
    }

    pub fn setup_linux_registers(&self, vp_id: u32, kernel_entry: u64) -> Result<()> {
        let kernel_load_addr = kernel_entry;
        
        // Read init_size from boot_params (offset 0x260 in boot_params at BOOT_PARAMS_BASE)
        let boot_params = self.read_memory(layout::ZERO_PAGE_START, 4096)?;
        let init_size = if boot_params.len() > 0x260 + 4 {
            u32::from_le_bytes([
                boot_params[0x260],
                boot_params[0x261],
                boot_params[0x262],
                boot_params[0x263],
            ])
        } else {
            layout::HIGH_RAM_START.0 as u32
        };
        
        linux_boot::setup_identity_paging(self, kernel_load_addr, init_size)?;
        linux_boot::setup_linux_registers(self, self.handle, vp_id, kernel_entry)?;
        
        Ok(())
    }

    pub fn read_memory(&self, gpa: GuestAddress, size: usize) -> Result<Vec<u8>> {
        unsafe {
            let region = self.memory.find_region(gpa).ok_or(anyhow::anyhow!("Memory not allocated"))?;
            if gpa.unchecked_add(size as u64) > region.last_addr() {
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
            let offset = (gpa.raw_value() - region.start_addr().raw_value()) as usize;
            let mut data = vec![0u8; size];
            ptr::copy_nonoverlapping(hpa.add(offset) as *const u8, data.as_mut_ptr(), size);
            Ok(data)
        }
    }

    /// Register an MMIO region with an optional handler
    pub fn register_mmio_region(&mut self, gpa: u64, size: u64, name: String, handler_name: Option<String>) -> Result<()> {
        let mmio_end = gpa.checked_add(size).ok_or_else(|| anyhow::anyhow!("MMIO region size overflow"))?;
        
        // Check for overlaps with existing memory regions
        for region in &self.memory.regions {
            if gpa < region.last_addr().raw_value() && mmio_end > region.start_addr().raw_value() {
                return Err(anyhow::anyhow!(
                    "MMIO region 0x{:X}-0x{:X} ({}) overlaps with memory region 0x{:X}-0x{:X}",
                    gpa, mmio_end, name,
                    region.start_addr().raw_value(), region.last_addr().raw_value()
                ));
            }
        }
        
        // Check for overlaps with existing MMIO regions
        for existing_mmio in &self.memory.mmio_regions {
            let existing_end = existing_mmio.gpa.0 + existing_mmio.size;
            
            if gpa < existing_end && mmio_end > existing_mmio.gpa.0 {
                return Err(anyhow::anyhow!(
                    "MMIO region 0x{:X}-0x{:X} ({}) overlaps with existing MMIO region 0x{:X}-0x{:X} ({})",
                    gpa, mmio_end, name,
                    existing_mmio.gpa.0, existing_end, existing_mmio.name
                ));
            }
        }
        
        self.memory.register_mmio(MmioRegion {
            gpa: GuestAddress(gpa),
            size,
            name: name.clone(),
            handler: handler_name.clone(),
        });
        
        Ok(())
    }
    
    /// Register an MMIO handler
    pub fn register_mmio_handler(&mut self, name: String, handler: Box<dyn BusDevice>) {
        self.mmio_handlers.insert(name, handler);
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
}

impl LinuxBootPartition for Partition {
    fn load_file(&self, file_path: &str, gpa: u64) -> Result<usize> {
        self.load_file(file_path, GuestAddress(gpa))
    }

    fn write_code(&self, code: &[u8], gpa: u64) -> Result<()> {
        self.write_code(code, GuestAddress(gpa))
    }

    fn get_handle(&self) -> WHV_PARTITION_HANDLE {
        self.handle
    }
    
    fn get_memory_size(&self) -> u64 {
        self.memory.regions.iter()
            .map(|r| r.start_addr().raw_value() + r.len())
            .max()
            .unwrap_or(0)
    }

    fn device_manager(&self) -> &DeviceManager {
        &self.device_manager
    }

    fn cpu_manager(&self) -> &CpuManager {
        &self.cpu_manager
    }

    fn memory_manager(&self) -> &MemoryManager {
        &self.memory
    }
}

impl Drop for Partition {
    fn drop(&mut self) {
        unsafe {
            if !self.emulator.handle.is_null() {
                let _ = WHvEmulatorDestroyEmulator(self.emulator.handle);
            }
            let _ = WHvDeletePartition(self.handle);
        }
    }
}
