// Memory management utilities for the hypervisor
// This module provides memory protection, MMIO tracking, and debugging features

use windows::Win32::System::Hypervisor::*;
use bitflags::bitflags;
    
bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct MemoryPerms: u32 {
        const READ    = 0b001;
        const WRITE   = 0b010;
        const EXECUTE = 0b100;
        const RW      = Self::READ.bits() | Self::WRITE.bits();
        const RX      = Self::READ.bits() | Self::EXECUTE.bits();
        const RWX     = Self::READ.bits() | Self::WRITE.bits() | Self::EXECUTE.bits();
    }
}

impl MemoryPerms {
    pub fn to_flags(&self) -> WHV_MAP_GPA_RANGE_FLAGS {
        let mut flags = WHV_MAP_GPA_RANGE_FLAGS::default();
        if self.contains(Self::READ)    { flags |= WHvMapGpaRangeFlagRead; }
        if self.contains(Self::WRITE)   { flags |= WHvMapGpaRangeFlagWrite; }
        if self.contains(Self::EXECUTE) { flags |= WHvMapGpaRangeFlagExecute; }
        flags
    }
}

impl std::fmt::Display for MemoryPerms {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// Represents a memory-mapped I/O region
#[derive(Debug, Clone)]
pub struct MmioRegion {
    pub gpa: u64,
    pub size: u64,
    pub name: String,
    pub handler: Option<String>, // Name of the handler function/device
}

/// Memory region information for debugging
#[derive(Debug, Clone)]
pub struct MemoryRegion {
    pub gpa: u64,
    pub size: u64,
    pub perms: MemoryPerms,
    pub hpa: Option<*mut std::ffi::c_void>,
    pub description: String,
}

/// Memory access violation information
#[derive(Debug)]
pub struct MemoryAccessViolation {
    pub gpa: u64,
    pub action: MemoryPerms,
    pub access_size: u32,
    pub instruction_rip: u64,
}

/* impl MemoryAccessViolation {
    pub fn from_exit_context(exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Option<Self> {
        unsafe {
            if exit_context.ExitReason.0 == WHvRunVpExitReasonMemoryAccess.0 {
                let mem_access = &exit_context.Anonymous.MemoryAccess;
                let access_info = mem_access.AccessInfo.AsUINT32;
                
                Some(Self {
                    gpa: mem_access.Gpa,
                    is_write: (access_info & 0x1) != 0,
                    is_execute: (access_info & 0x2) != 0,
                    access_size: (access_info >> 2) & 0x7, // Bits 2-4
                    instruction_rip: exit_context.VpContext.Rip,
                })
            } else {
                None
            }
        }
    }
} */

/// Memory translation debugger
pub struct Memory {
    pub regions: Vec<MemoryRegion>,
    pub mmio_regions: Vec<MmioRegion>,
    access_log: Vec<(u64, bool, u64)>, // (GPA, is_write, timestamp-ish)
}

impl Memory {
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
            mmio_regions: Vec::new(),
            access_log: Vec::new(),
        }
    }

    pub fn register_region(&mut self, region: MemoryRegion) {
        self.regions.push(region);
    }

    pub fn register_mmio(&mut self, mmio: MmioRegion) {
        self.mmio_regions.push(mmio);
    }

    pub fn log_access(&mut self, gpa: u64, is_write: bool) {
        // Simple access logging (could be enhanced with actual timestamps)
        self.access_log.push((gpa, is_write, self.access_log.len() as u64));
        
        // Keep only last 1000 accesses to avoid memory bloat
        if self.access_log.len() > 1000 {
            self.access_log.remove(0);
        }
    }

    /// Find which region contains a GPA
    pub fn find_region(&self, gpa: u64) -> Option<&MemoryRegion> {
        self.regions.iter()
            .find(|r| gpa >= r.gpa && gpa < r.gpa + r.size)
    }

    /// Find MMIO region for a GPA
    pub fn find_mmio(&self, gpa: u64) -> Option<&MmioRegion> {
        self.mmio_regions.iter()
            .find(|r| gpa >= r.gpa && gpa < r.gpa + r.size)
    }

    /// Print memory map
    pub fn print_memory_map(&self) {
        println!("\n=== Memory Map ===");
        for region in &self.regions {
            println!("GPA: 0x{:016X} - 0x{:016X} ({}) - {:?}",
                region.gpa,
                region.gpa + region.size,
                region.description,
                region.perms
            );
        }
        
        if !self.mmio_regions.is_empty() {
            println!("\n=== MMIO Regions ===");
            for mmio in &self.mmio_regions {
                println!("GPA: 0x{:016X} - 0x{:016X} ({})",
                    mmio.gpa,
                    mmio.gpa + mmio.size,
                    mmio.name
                );
            }
        }
    }

    /// Print recent memory accesses
    pub fn print_access_log(&self, count: usize) {
        let start = if self.access_log.len() > count {
            self.access_log.len() - count
        } else {
            0
        };
        
        println!("\n=== Recent Memory Accesses (last {}) ===", count);
        for (gpa, is_write, _) in &self.access_log[start..] {
            println!("GPA: 0x{:016X} - {}", gpa, if *is_write { "WRITE" } else { "READ" });
        }
    }

    /// Analyze memory access violation
    pub fn analyze_violation(&self, violation: &MemoryAccessViolation) -> String {
        let mut analysis = format!(
            "Memory Access Violation:\n  GPA: 0x{:016X}\n  Type: {}\n  Size: {} bytes\n  RIP: 0x{:016X}\n",
            violation.gpa,
            violation.action.to_string(),
            violation.access_size,
            violation.instruction_rip
        );

        if let Some(region) = self.find_region(violation.gpa) {
            analysis.push_str(&format!(
                "  Region: {} (0x{:016X} - 0x{:016X})\n  Protection: {:?}\n",
                region.description,
                region.gpa,
                region.gpa + region.size,
                region.perms
            ));

            match violation.action {
                MemoryPerms::READ => {
                    analysis.push_str("  ❌ Read access violation!\n");
                }
                MemoryPerms::WRITE => {
                    analysis.push_str("  ❌ Write access violation!\n");
                }
                MemoryPerms::EXECUTE => {
                    analysis.push_str("  ❌ Execute access violation!\n");
                }
                _ => {
                    
                }
            }
        } else if let Some(mmio) = self.find_mmio(violation.gpa) {
            analysis.push_str(&format!(
                "  MMIO Region: {} (0x{:016X} - 0x{:016X})\n",
                mmio.name,
                mmio.gpa,
                mmio.gpa + mmio.size
            ));
        } else {
            analysis.push_str("  ❌ GPA not mapped to any region!\n");
        }

        analysis
    }
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}
