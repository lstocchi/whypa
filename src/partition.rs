use windows::Win32::System::{
    Hypervisor::*,
    Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc,
    },
};
use windows::Win32::Foundation::{HANDLE, CloseHandle};

use anyhow::Result;
use anyhow::Context;
use std::{ffi::c_void, ptr};
use std::fs;
use tracing::{debug};

use crate::{cpu::CpuManager, device_manager::DeviceManager, devices::bus::BusDevice, emulator::{self, Emulator}, linux_boot::{self, LinuxBootPartition}, memory::{layout, memory::{GuestAddress, MemoryAccessViolation, MemoryManager, MemoryPerms, MemoryRegion, MmioRegion}}};
use std::collections::HashMap;

pub struct Partition {
    pub handle: WHV_PARTITION_HANDLE,
    emulator: Emulator, // Emulator handle for instruction emulation
    is_shared_memory: bool, // Whether memory is from shared mapping
    injected_shared_regions: Vec<InjectedSharedRegion>, // Track injected shared memory regions
    memory: MemoryManager, // Memory debugging and tracking
    device_manager: DeviceManager, // Device management (PCI, serial, ACPI platform addresses)
    cpu_manager: CpuManager, // CPU management (ACPI MADT, CPU topology)
    mmio_handlers: HashMap<String, Box<dyn BusDevice>>, // MMIO device handlers by name
    pub pci_address_config: u32,
    // Hyper-V enlightenment state
    vm_start_tsc: u64,              // Host TSC value captured at VM creation
    tsc_reference_gpa: Option<u64>, // GPA of the TSC reference page (set by guest via MSR 0x40000021)
    guest_os_id: u64,               // Value written by guest to HV_X64_MSR_GUEST_OS_ID
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
    pub fn new(memory_size: usize) -> Result<Self> {
        unsafe {
            let handle = WHvCreatePartition()?;

            let emulator = emulator::Emulator::new()?;
                        
            let mut device_manager = DeviceManager::new(GuestAddress(memory_size as u64));
            device_manager.init();
            Ok(Self {
                handle,
                emulator,
                is_shared_memory: false,
                injected_shared_regions: Vec::new(),
                memory: MemoryManager::new(),
                device_manager: device_manager,
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

            // Update CPU manager with processor count
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

    /// Get a mutable reference to the CPU manager
    pub fn cpu_manager_mut(&mut self) -> &mut CpuManager {
        &mut self.cpu_manager
    }

    /// Get a reference to the memory manager
    pub fn memory_manager(&self) -> &MemoryManager {
        &self.memory
    }

    /// Get a mutable reference to the memory manager
    pub fn memory_manager_mut(&mut self) -> &mut MemoryManager {
        &mut self.memory
    }

    /// Get a mutable reference to MMIO handlers (for emulator callbacks)
    pub(crate) fn mmio_handlers_mut(&mut self) -> &mut HashMap<String, Box<dyn BusDevice>> {
        &mut self.mmio_handlers
    }

    pub fn setup(&self) -> Result<()> {
        unsafe {
            // 1. Create the property union
            let mut property: WHV_PARTITION_PROPERTY = std::mem::zeroed();
            
            // 2. Enable Local APIC Emulation
            // This tells WHP to handle memory at 0xFEE00000 automatically
            property.LocalApicEmulationMode = WHV_X64_LOCAL_APIC_EMULATION_MODE(1);

            // 3. Apply the property to the partition
            let result = WHvSetPartitionProperty(
                self.handle,
                WHvPartitionPropertyCodeLocalApicEmulationMode,
                &property as *const _ as *const _,
                std::mem::size_of::<WHV_PARTITION_PROPERTY>() as u32,
            );

            if result.is_err() {
                return Err(anyhow::anyhow!("Failed to enable Local APIC emulation: {:?}", result));
            }

            // 4. Configure CPUID exit list for Hyper-V enlightenment leaves.
            // This ensures we intercept CPUID for the 0x4000000x range so we can
            // advertise Hyper-V features (including the Reference TSC Page).
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
            // flags is unused and must be zero - https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvcreatevirtualprocessor
            WHvCreateVirtualProcessor(self.handle, vp_id, 0)?;
            Ok(())
        }
    }

    pub fn allocate_memory(&mut self) -> Result<()> {
        self.allocate_memory_with_size(4 * 1024 * 1024, MemoryPerms::RWX)
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
                // Map the first 3GB
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

            // Note: We don't register the 32-bit reserved area as an MMIO region.
            // It's just unmapped memory - individual MMIO devices within this area
            // (like virtio, PCI, etc.) should be registered separately.
            // The reserved area will still be included in the E820 table as reserved memory.

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

    /// Load Linux kernel and set up boot parameters using linux_loader
    pub fn load_linux_kernel(&mut self, kernel_path: &str, initram_path: &str, memory_size: u64) -> Result<u64> {
        linux_boot::load_linux_kernel(self, kernel_path, initram_path)
    }

    /// Set up Linux boot parameters structure
    /// This is a minimal implementation - real boot params are more complex
    fn setup_linux_boot_params(&self, gpa: GuestAddress) -> Result<()> {
        // Linux boot_params structure (simplified)
        // We'll set up minimal fields needed for boot
        let mut boot_params = vec![0u8; 4096]; // 4KB should be enough for basic params
        
        // Set signature "HdrS" (0x53726448) at offset 0x1f1
        // This identifies it as a valid boot_params structure
        let hdr_signature: u32 = 0x53726448; // "HdrS"
        boot_params[0x1f1..0x1f5].copy_from_slice(&hdr_signature.to_le_bytes());
        
        // Set version (offset 0x1f6) - u16 value
        let version: u16 = 0x0208; // Version 2.08
        boot_params[0x1f6..0x1f8].copy_from_slice(&version.to_le_bytes());
        
        // Set kernel_alignment (offset 0x1f7) - typically 0x200000 (2MB)
        let kernel_align: u32 = 0x200000;
        boot_params[0x1f7..0x1fb].copy_from_slice(&kernel_align.to_le_bytes());
        
        // Set cmd_line_ptr (offset 0x228) - pointer to command line
        // For now, we'll set it to 0 (no command line)
        // In a full implementation, you'd put the command line somewhere and point to it
        
        // Load type (offset 0x210) - 0x01 = loaded by boot loader
        boot_params[0x210] = 0x01;
        
        // Write boot params to memory
        self.write_code(&boot_params, gpa)?;
        //eprintln!("  Boot parameters set up at 0x{:X}", gpa.raw_value());
        
        Ok(())
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
        // Calculate kernel_load_addr from kernel_entry (kernel_entry = kernel_load_addr + 0x200)
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
            layout::HIGH_RAM_START.0 as u32 // Default to 1MB if not available
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
        // Check for overlaps with existing memory regions
        let mmio_end = gpa.checked_add(size).ok_or_else(|| anyhow::anyhow!("MMIO region size overflow"))?;
        
        for region in &self.memory.regions {
            
            
            // Check if MMIO overlaps with this memory region
            // Overlap exists if: mmio_start < region_end && mmio_end > region_gpa
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
        
        //eprintln!("  ✓ MMIO region 0x{:X}-0x{:X} ({}) registered (no overlaps)", gpa, mmio_end, name);
        
        // Register the MMIO region in memory tracking
        self.memory.register_mmio(MmioRegion {
            gpa: GuestAddress(gpa),
            size,
            name: name.clone(),
            handler: handler_name.clone(),
        });
        
        // Don't map this region - MMIO regions should not be mapped to physical memory
        // WHV will trap unmapped memory accesses and generate memory access exits
        
        Ok(())
    }
    
    /// Register an MMIO handler
    pub fn register_mmio_handler(&mut self, name: String, handler: Box<dyn BusDevice>) {
        self.mmio_handlers.insert(name, handler);
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
            // Setting CS incorrectly can cause "Invalid VP register value" errors
            // The hypervisor will set appropriate defaults for the VM mode
            Ok(())
        }
    }

    /// Get the host TSC frequency in Hz.
    /// Tries CPUID leaf 0x15 (TSC/crystal ratio), then 0x16 (processor base frequency).
    fn get_tsc_frequency_hz() -> u64 {
        unsafe {
            // Try CPUID 0x15: TSC frequency = ECX * EBX / EAX
            let cpuid15 = core::arch::x86_64::__cpuid(0x15);
            if cpuid15.eax != 0 && cpuid15.ebx != 0 && cpuid15.ecx != 0 {
                return (cpuid15.ecx as u64 * cpuid15.ebx as u64) / cpuid15.eax as u64;
            }

            // Fall back to CPUID 0x16: EAX = processor base frequency in MHz
            let cpuid16 = core::arch::x86_64::__cpuid(0x16);
            if cpuid16.eax != 0 {
                return cpuid16.eax as u64 * 1_000_000;
            }

            // Last resort fallback
            2_712_000_000
        }
    }

    /// Write the Hyper-V Reference TSC Page into guest memory at the given GPA.
    ///
    /// The page layout (HV_REFERENCE_TSC_PAGE):
    ///   +0x00: u32 TscSequence  (non-zero = valid)
    ///   +0x04: u32 Reserved
    ///   +0x08: u64 TscScale
    ///   +0x10: i64 TscOffset
    ///   +0x18: ... (rest of page zeroed)
    ///
    /// Guest formula: ReferenceTime = ((RDTSC() * TscScale) >> 64) + TscOffset
    /// Result is in 100-nanosecond units (10 MHz).
    fn write_tsc_reference_page(&self, gpa: u64) -> Result<()> {
        let tsc_freq = Self::get_tsc_frequency_hz();

        // TscScale: fixed-point multiplier that converts TSC ticks → 100ns ticks.
        // (10_000_000 << 64) / tsc_freq, computed via 128-bit math.
        let tsc_scale: u64 = ((10_000_000u128 << 64) / tsc_freq as u128) as u64;

        // TscOffset: calibrate so ReferenceTime ≈ 0 at VM start.
        // offset = -((vm_start_tsc * scale) >> 64)
        let tsc_offset: i64 = -(((self.vm_start_tsc as u128 * tsc_scale as u128) >> 64) as i64);

        let mut page = [0u8; 4096];
        page[0..4].copy_from_slice(&1u32.to_le_bytes());       // TscSequence = 1 (valid)
        // page[4..8] is reserved (already zero)
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
       
        // Extract InstructionLength from the lower 4 bits of _bitfield
        // InstructionLength : 4 means it uses bits 0-3
        let instruction_length = (vp_context._bitfield & 0x0F) as u64;
        
        Self::advance_rip_new(vp_context, instruction_length, handle, vp_id)
    }

    fn advance_rip_new(vp_context: WHV_VP_EXIT_CONTEXT, instruction_length: u64, handle: WHV_PARTITION_HANDLE, vp_id: u32) -> Result<()> {
        // Access WHV_VP_EXIT_CONTEXT to get RIP and InstructionLength
        // According to Microsoft docs: https://learn.microsoft.com/en-us/virtualization/api/hypervisor-platform/funcs/whvexitcontextdatatypes
        // InstructionLength is in the lower 4 bits of _bitfield (bits 0-3)
        // Cr8 is in the upper 4 bits of _bitfield (bits 4-7)
        let current_rip = vp_context.Rip;
        
        //eprintln!("Current RIP: 0x{:X}, Instruction length: {} bytes", current_rip, instruction_length);
        
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
        
        // Verify RIP was actually set by reading it back
        let mut verify_rip = WHV_REGISTER_VALUE::default();
        unsafe {
            WHvGetVirtualProcessorRegisters(
                handle,
                vp_id,
                &[WHvX64RegisterRip] as *const _,
                1,
                &mut verify_rip as *mut _ as *mut WHV_REGISTER_VALUE,
            ).map_err(|e| anyhow::anyhow!("Failed to verify RIP: {}", e))?;
            
            if verify_rip.Reg64 != new_rip {
                //eprintln!("  ⚠️  WARNING: RIP update failed! Expected 0x{:X}, got 0x{:X}", new_rip, verify_rip.Reg64);
            } else {
                //eprintln!("Advanced RIP to 0x{:X} (skipped {} byte instruction) ✓", new_rip, instruction_length);
            }
        }
        Ok(())
    }

    pub fn dump_memory(&self, gpa: GuestAddress, size: usize) -> Result<()> {
        let data = self.read_memory(gpa, size)?;
        //eprintln!("--- Memory Dump at 0x{:X} ---", gpa.raw_value());
        for chunk in data.chunks(16).enumerate() {
            let offset = chunk.0 * 16;
            let hex: Vec<String> = chunk.1.iter().map(|b| format!("{:02X}", b)).collect();
            let ascii: String = chunk.1.iter()
                .map(|&b| if b >= 32 && b <= 126 { b as char } else { '.' })
                .collect();
            //eprintln!("0x{:08X}: {:48} | {} |", gpa.raw_value() + offset as u64, hex.join(" "), ascii);
        }
        Ok(())
    }

    /// Handle I/O port access using emulator (if available) or manual handling
    fn handle_io_port_with_emulator(
        &mut self,
        exit_context: &WHV_RUN_VP_EXIT_CONTEXT,
    ) -> Result<bool> {
        let io_port_access_ctx = unsafe { &exit_context.Anonymous.IoPortAccess };
        let vp_context = &exit_context.VpContext;
        let result = self.emulator.try_io_emulation(self as *const _ as *const std::ffi::c_void, vp_context, io_port_access_ctx)?;
        
        
        unsafe {
            let status = result.Anonymous._bitfield as u64;
            if status & 0x1 == 1 {
                ////eprintln!("Emulator status: EMULATED");
                return Ok(true);
            } else {
                ////eprintln!("Emulator status: NOT EMULATED");
                return Ok(false);
            }
        }
    }

    pub fn handle_exit(&mut self, vp_id: u32, exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Result<bool> {
        // Returns true if we should continue running, false if we should stop
        // Safely read exit reason - this should always be valid
        // Use a pointer to avoid potential issues with union access
        // Access ExitReason field directly - it's not part of the union, so it's always safe
        let exit_reason = exit_context.ExitReason.0;
        
        // Get current RIP to verify it's actually updated
        let exit_rip = exit_context.VpContext.Rip;
        
        // Track exit reasons to see what's happening (using atomics for thread safety)
        use std::sync::atomic::{AtomicU64, Ordering};
        static EXIT_COUNT: AtomicU64 = AtomicU64::new(0);
        static MEMORY_ACCESS_COUNT: AtomicU64 = AtomicU64::new(0);
        static OTHER_EXIT_COUNT: AtomicU64 = AtomicU64::new(0);
        
        let exit_count = EXIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        if exit_reason == WHvRunVpExitReasonMemoryAccess.0 {
            MEMORY_ACCESS_COUNT.fetch_add(1, Ordering::Relaxed);
        } else {
            let other_count = OTHER_EXIT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            // Log non-MMIO exits occasionally to see what's happening
            
        }
        
        // Log summary every 100k exits
        if exit_count % 100000 == 0 {
            let mmio_count = MEMORY_ACCESS_COUNT.load(Ordering::Relaxed);
            let other_count = OTHER_EXIT_COUNT.load(Ordering::Relaxed);
            //eprintln!("Exit summary: total={}, MMIO={}, other={}", 
              //  exit_count, mmio_count, other_count);
        }
        
        
        
        // Match on exit reason - only access union fields in the corresponding match arm
        // Each match arm must only access the union member that corresponds to that exit reason
        match exit_reason {
            x if x == WHvRunVpExitReasonNone.0 => {
                //eprintln!("Unexpected exit reason: None");
                Ok(false)
            }
            x if x == WHvRunVpExitReasonMemoryAccess.0 => {
                // Extract memory access violation details
                let violation = MemoryAccessViolation::from_exit_context(exit_context)
                    .ok_or_else(|| anyhow::anyhow!("Failed to extract memory access violation"))?;
                
                // Check if this is an MMIO region - if so, use the emulator
                if let Some(mmio_region) = self.memory.find_mmio(violation.gpa.0) {
                    // Only log virtio and IOAPIC accesses to reduce noise
                    if mmio_region.name.contains("Virtio") || mmio_region.name.contains("IOAPIC") {
                        //eprintln!("MMIO {}: {} at 0x{:X}, RIP=0x{:X}", 
                           // if violation.action.contains(MemoryPerms::WRITE) { "WRITE" } else { "READ" },
                           // mmio_region.name, violation.gpa.0, violation.instruction_rip);
                    }
                    
                    // Use the WHP emulator to handle MMIO properly
                    // The emulator will decode the instruction, determine which register is used,
                    // call our memory_callback to get/set the value, and update the correct register
                    let memory_access_ctx = unsafe { &exit_context.Anonymous.MemoryAccess };
                    let vp_context = &exit_context.VpContext;
                    
                    match self.emulator.try_mmio_emulation(
                        self as *const _ as *const std::ffi::c_void,
                        vp_context,
                        memory_access_ctx,
                    ) {
                        Ok(emulator_status) => {
                            unsafe {
                                let status = emulator_status.Anonymous._bitfield as u64;
                                if status & 0x1 == 1 {
                                    // Emulator successfully handled the MMIO access
                                    // It has already updated the correct register and advanced RIP
                                    Ok(true)
                                } else {
                                    // Emulator couldn't handle it - fall back to error
                                    //eprintln!("  ⚠️ MMIO emulation failed for {} at 0x{:X}", mmio_region.name, violation.gpa.0);
                                    Ok(false)
                                }
                            }
                        }
                        Err(e) => {
                            //eprintln!("  ⚠️ MMIO emulation error for {} at 0x{:X}: {:?}", mmio_region.name, violation.gpa.0, e);
                            Ok(false)
                        }
                    }
                } else {
                    // Not an MMIO region - check if it's a valid memory region
                    if let Some(region) = self.memory.find_region(violation.gpa) {
                        // Check if access violates protection
                        if !region.perms.contains(violation.action) {
                            //eprintln!("Memory protection violation: {}", self.memory.analyze_violation(&violation));
                            return Ok(false);
                        }
                                                
                        // Valid access to mapped memory - this shouldn't normally cause an exit
                        // unless the region was unmapped. For now, treat as error.
                        //eprintln!("Unexpected memory access exit for mapped region");
                        return Ok(false);
                    } else {
                        // Unmapped memory access - page fault
                        //eprintln!("Page fault: {}", self.memory.analyze_violation(&violation));
                        return Ok(false);
                    }
                }
            }
            x if x == WHvRunVpExitReasonX64IoPortAccess.0 => {
                // Use emulator if available, otherwise fall back to manual handling
                self.handle_io_port_with_emulator(exit_context)
            }
            x if x == WHvRunVpExitReasonException.0 || x == 4 => {
                unsafe {
                    // Access VpException fields when exit reason is Exception
                    let vp_exception = &exit_context.Anonymous.VpException;
                    //eprintln!("Exception occurred:");
                    //eprintln!("  Exception Type: {} (0x{:X})", vp_exception.ExceptionType, vp_exception.ExceptionType);
                    //eprintln!("  Error Code: 0x{:X}", vp_exception.ErrorCode);
                    //eprintln!("  Exception Parameter: 0x{:X}", vp_exception.ExceptionParameter);
                    //eprintln!("  RIP: 0x{:X}", exit_context.VpContext.Rip);
                    
                    // Common exception types:
                    // 0x0E = Page Fault (PF)
                    // 0x0D = General Protection Fault (GPF)
                    // 0x06 = Invalid Opcode
                    // 0x00 = Divide Error
                    match vp_exception.ExceptionType {
                        0x0E => {
                            //eprintln!("  → Page Fault detected!");
                            //eprintln!("  → Error Code: 0x{:X}", vp_exception.ErrorCode);
                            //eprintln!("  → Faulting Address: 0x{:X}", vp_exception.ExceptionParameter);
                            //eprintln!("  → This likely means memory at 0x{:X} is not mapped", vp_exception.ExceptionParameter);
                            //eprintln!("  → Check page tables and memory mapping");
                        }
                        0x0D => {
                            //eprintln!("  → General Protection Fault!");
                            //eprintln!("  → Error Code: 0x{:X}", vp_exception.ErrorCode);
                            //eprintln!("  → This might indicate segment register issues or invalid memory access");
                        }
                        0x06 => {
                            //eprintln!("  → Invalid Opcode!");
                            //eprintln!("  → The instruction at RIP might not be valid for the current CPU mode");
                        }
                        0x00 => {
                            //eprintln!("  → Divide Error!");
                            //eprintln!("  → Division by zero or overflow");
                        }
                        _ => {
                            //eprintln!("  → Unknown exception type - check x86 exception documentation");
                        }
                    }
                }
                Ok(false)
            }
            x if x == WHvRunVpExitReasonUnrecoverableException.0 => {
                unsafe {
                    // Only access VpException fields when exit reason is UnrecoverableException
                    let vp_exception = &exit_context.Anonymous.VpException;
                    //eprintln!("Unrecoverable exception: {}", vp_exception.ExceptionType);
                    //eprintln!("  RIP: 0x{:X}", exit_context.VpContext.Rip);
                    //eprintln!("  ⚠️  This might be how WHV handles unmapped memory access!");
                    //eprintln!("  ⚠️  Check if this exception type indicates a memory access violation");
                }
                Ok(false)
            }
            x if x == WHvRunVpExitReasonInvalidVpRegisterValue.0 => {
                //eprintln!("Invalid VP register value");
                Ok(false)
            }
            x if x == WHvRunVpExitReasonUnsupportedFeature.0 => {
                //eprintln!("Unsupported feature");
                Ok(false)
            }
            x if x == WHvRunVpExitReasonX64InterruptWindow.0 => {
                // Interrupt window - kernel is ready to receive interrupts
                // Inject a timer interrupt (IRQ 0) periodically to allow kernel scheduling
                use std::sync::atomic::{AtomicU64, Ordering};
                static LAST_INTERRUPT_TIME: AtomicU64 = AtomicU64::new(0);
                static INTERRUPT_COUNT: AtomicU64 = AtomicU64::new(0);
                
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;
                
                // Inject timer interrupt on every interrupt window to keep kernel progressing
                // The kernel is stuck waiting for interrupts, so we need to inject them frequently
                let count = INTERRUPT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
                
                // Inject interrupt every time (or at least very frequently)
                // The 10ms throttling might be preventing the kernel from making progress
                let last_time = LAST_INTERRUPT_TIME.load(Ordering::Relaxed);
                let should_inject = now.saturating_sub(last_time) >= 1 || last_time == 0; // Every 1ms or first time
                
                if should_inject {
                    LAST_INTERRUPT_TIME.store(now, Ordering::Relaxed);
                    
                    // Log first few interrupts to verify they're being injected
                    if count <= 10 {
                        //eprintln!("Injecting timer interrupt #{} at RIP 0x{:X}", count, exit_rip);
                    }
                    
                    // Inject Local APIC timer interrupt using WHvRequestInterrupt
                    // Vector 0x20 is the timer interrupt (IRQ 0 mapped to vector 0x20)
                    unsafe {
                        // WHV_INTERRUPT_CONTROL structure:
                        // _bitfield: contains interrupt type (bits 0-3) and other flags
                        // Destination: destination CPU (0 = broadcast to all)
                        // Vector: interrupt vector
                        let interrupt_control = WHV_INTERRUPT_CONTROL {
                            _bitfield: WHvX64InterruptTypeFixed.0 as u64, // Set interrupt type in bitfield
                            Destination: 0, // Broadcast to all CPUs
                            Vector: 0x20, // Timer interrupt vector
                        };
                        
                        let result = WHvRequestInterrupt(
                            self.handle,
                            &interrupt_control as *const _,
                            std::mem::size_of::<WHV_INTERRUPT_CONTROL>() as u32,
                        );
                        
                        if result.is_err() {
                            // Log errors for first few attempts
                            if count <= 10 {
                                //eprintln!("Failed to inject timer interrupt #{}: {:?}", count, result);
                            }
                        } else if count <= 10 {
                            //eprintln!("Successfully injected timer interrupt #{}", count);
                        }
                    }
                }
                Ok(true)
            }
            x if x == WHvRunVpExitReasonX64Halt.0 => {
                // VM executed HLT instruction - kernel is waiting for an interrupt
                // For now, we'll just continue (in a real implementation, we'd inject a timer interrupt)
                // Log occasionally to avoid spam
                static mut HALT_COUNT: u64 = 0;
                unsafe {
                    HALT_COUNT += 1;
                    if HALT_COUNT % 10000 == 0 {
                        
                            //eprintln!("VM halted times at RIP 0x{:X} (waiting for interrupt?)", 
                            //exit_context.VpContext.Rip);    
                        
                        
                    }
                }
                Ok(true) // Continue execution - kernel will wake up when interrupt is injected
            }
            x if x == WHvRunVpExitReasonX64ApicEoi.0 => {
                // Forward EOI to the IOAPIC so it can clear Remote IRR for
                // level-triggered interrupts.  Without this the IOAPIC thinks
                // the interrupt is still being serviced and coalesces (drops)
                // all subsequent assertions on the same pin.
                let vector = unsafe { exit_context.Anonymous.ApicEoi.InterruptVector };
                if let Some(handler) = self.mmio_handlers.get_mut("ioapic") {
                    // Write to the IOAPIC EOI register (offset 0x40) with the vector
                    handler.write(0, 0x40, &(vector as u32).to_le_bytes());
                }
                Ok(true)
            }
            x if x == WHvRunVpExitReasonX64Cpuid.0 => {
                let cpuid_access = unsafe { &exit_context.Anonymous.CpuidAccess };
                let leaf = cpuid_access.Rax;

                let mut rax = cpuid_access.DefaultResultRax;
                let mut rbx = cpuid_access.DefaultResultRbx;
                let mut rcx = cpuid_access.DefaultResultRcx;
                let mut rdx = cpuid_access.DefaultResultRdx;

                match leaf {
                    0x40000000 => {
                        // Hyper-V hypervisor present: max leaf + "Microsoft Hv" vendor signature.
                        // Linux checks EBX:ECX:EDX == "Microsoft Hv" to detect Hyper-V.
                        rax = 0x40000005; // max hypervisor CPUID leaf
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
                        // Hypervisor system identity (version info — informational only)
                        rax = 0;               // Build number
                        rbx = (10 << 16) | 0;  // Major.Minor version
                        rcx = 0;               // Service pack
                        rdx = 0;               // Service branch
                    }
                    0x40000003 => {
                        // Hypervisor feature identification.
                        // EAX privilege bits the guest may use:
                        //   Bit  1: AccessPartitionReferenceCounter (MSR 0x40000020)
                        //   Bit  9: AccessReferenceTsc (MSR 0x40000021 — the TSC page)
                        //   Bit 15: AccessTscInvariantControls (MSR 0x40000118)
                        //           Linux writes to this MSR and then calls
                        //           setup_force_cpu_cap(X86_FEATURE_TSC_RELIABLE),
                        //           which tells the clocksource watchdog to trust TSC.
                        rax = (1 << 1) | (1 << 9) | (1 << 15);
                        rbx = 0;
                        rcx = 0;
                        rdx = 0;
                    }
                    0x40000004 => {
                        // Enlightenment recommendations / hints
                        rax = 0;
                        rbx = 0;
                        rcx = 0;
                        rdx = 0;
                    }
                    0x40000005 => {
                        // Implementation limits
                        rax = 0;
                        rbx = 0;
                        rcx = 0;
                        rdx = 0;
                    }
                    _ => {
                        // For all other CPUID requests, pass through the default results.
                    }
                }

                let next_rip = exit_context.VpContext.Rip + (exit_context.VpContext._bitfield & 0x0F) as u64;

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
            x if x == WHvRunVpExitReasonX64MsrAccess.0 => {
                let msr_access = unsafe { &exit_context.Anonymous.MsrAccess };
                let msr_number = msr_access.MsrNumber;
                let is_write = unsafe { msr_access.AccessInfo.Anonymous._bitfield } & 0x1 != 0; // Bit 0 is Write

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
                        // Linux expects to see 'Enabled' bit (bit 0) if it previously wrote to it.
                        if !is_write {
                            rax = 1; // Mark as enabled
                        }
                    }
                    0x40000020 => { // HV_X64_MSR_TIME_REF_COUNT (read-only)
                        // Return elapsed time since VM start in 100ns increments.
                        // This is the slow (MSR-exit) fallback; once the TSC reference page
                        // is active, the guest won't use this MSR for timekeeping.
                        use std::sync::OnceLock;
                        static VM_START_TIME: OnceLock<std::time::Instant> = OnceLock::new();
                        let start = VM_START_TIME.get_or_init(|| std::time::Instant::now());
                        let ticks = start.elapsed().as_nanos() / 100; // 100ns increments
                        rax = (ticks & 0xFFFFFFFF) as u64;
                        rdx = ((ticks >> 32) & 0xFFFFFFFF) as u64;
                    }
                    0x40000021 => { // HV_X64_MSR_REFERENCE_TSC
                        // The guest writes a GPA (page-aligned) with bit 0 = enable.
                        // We populate a shared page at that GPA with TSC scale/offset so the
                        // guest can compute time via RDTSC without any further VM exits.
                        if is_write {
                            let msr_value = (msr_access.Rdx << 32) | (msr_access.Rax & 0xFFFFFFFF);
                            let enabled = msr_value & 1;
                            if enabled != 0 {
                                let gpa = msr_value & !0xFFF; // bits 63:12 are the PFN, page-aligned
                                self.write_tsc_reference_page(gpa)?;
                                self.tsc_reference_gpa = Some(gpa);
                            } else {
                                self.tsc_reference_gpa = None;
                            }
                        } else {
                            // Read back: return stored GPA | enabled bit
                            if let Some(gpa) = self.tsc_reference_gpa {
                                let value = gpa | 1; // set enabled bit
                                rax = value & 0xFFFFFFFF;
                                rdx = value >> 32;
                            }
                        }
                    }
                    0x40000118 => { // HV_X64_MSR_TSC_INVARIANT_CONTROL
                        // Linux writes HV_EXPOSE_INVARIANT_TSC (bit 0) here when it sees
                        // AccessTscInvariantControls (CPUID 0x40000003 bit 15).
                        // After writing, Linux calls setup_force_cpu_cap(X86_FEATURE_TSC_RELIABLE)
                        // which makes the clocksource watchdog trust TSC unconditionally.
                        // We just accept the write (sink) and return it on read.
                        static TSC_INVARIANT_CONTROL: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        if is_write {
                            let val = (msr_access.Rdx << 32) | (msr_access.Rax & 0xFFFFFFFF);
                            TSC_INVARIANT_CONTROL.store(val, std::sync::atomic::Ordering::Relaxed);
                        } else {
                            let val = TSC_INVARIANT_CONTROL.load(std::sync::atomic::Ordering::Relaxed);
                            rax = val & 0xFFFFFFFF;
                            rdx = val >> 32;
                        }
                    }
                    _ => {}
                }

                if !is_write {
                    // For reads (RDMSR), we must put the result in RAX and RDX
                    let names = [WHvX64RegisterRax, WHvX64RegisterRdx];
                    let values = [
                        WHV_REGISTER_VALUE { Reg64: rax },
                        WHV_REGISTER_VALUE { Reg64: rdx },
                    ];
                    unsafe {
                        WHvSetVirtualProcessorRegisters(self.handle, 0, &names as *const _, 2, &values as *const _)?;
                    }
                }

                // Advance RIP
                Self::advance_rip_new(exit_context.VpContext, (exit_context.VpContext._bitfield & 0x0F) as u64, self.handle, 0)?;
                Ok(true)
            }
            _ => {
                //eprintln!("Unknown exit reason: {} (0x{:X})", exit_reason, exit_reason);
                //eprintln!("  RIP: 0x{:X}", exit_context.VpContext.Rip);
                //eprintln!("  ⚠️  This might be how WHV reports unmapped memory access!");
                //eprintln!("  ⚠️  Check WHV documentation for exit reason {}", exit_reason);
                Ok(false)
            }
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
        // Find the largest memory region to determine total memory size
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
            // Destroy emulator handle
            if !self.emulator.handle.is_null() {
                let _ = WHvEmulatorDestroyEmulator(self.emulator.handle);
            }
            
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

            let _ = WHvDeletePartition(self.handle);
        }
    }
}