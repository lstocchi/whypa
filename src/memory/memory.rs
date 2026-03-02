// Portions Copyright © 2019 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

// Memory management utilities for the hypervisor
// This module provides memory protection, MMIO tracking, and debugging features

use std::ptr;
use std::sync::atomic::{AtomicU8, AtomicU16, AtomicU32, AtomicU64, Ordering};

use anyhow::Result;
use vm_memory::VolatileSlice;
use windows::Win32::System::Hypervisor::*;
use bitflags::bitflags;
use zerocopy::FromBytes;

pub const MEMORY_MANAGER_ACPI_SIZE: usize = 0x18;

/// Represents a guest physical address (GPA).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
pub struct GuestAddress(pub u64);

impl GuestAddress {
    pub fn new(addr: u64) -> Self {
        Self(addr)
    }

    pub fn raw_value(&self) -> u64 {
        self.0
    }

    pub fn checked_offset_from(&self, base: Self) -> Option<u64> {
        self.0.checked_sub(base.0)
    }

    pub fn checked_add(&self, other: u64) -> Option<Self> {
        self.0.checked_add(other).map(Self)
    }

    pub fn overflowing_add(&self, other: u64) -> (Self, bool) {
        let (t, ovf) = self.0.overflowing_add(other);
        (Self(t), ovf)
    }

    pub fn unchecked_add(&self, offset: u64) -> Self {
        Self(self.0 + offset)
    }

    pub fn checked_sub(&self, other: u64) -> Option<Self> {
        self.0.checked_sub(other).map(Self)
    }

    pub fn overflowing_sub(&self, other: u64) -> (Self, bool) {
        let (t, ovf) = self.0.overflowing_sub(other);
        (Self(t), ovf)
    }

    pub fn unchecked_sub(&self, other: u64) -> Self {
        Self(self.0 - other)
    }
}
    
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
    pub gpa: GuestAddress,
    pub size: u64,
    pub name: String,
    pub handler: Option<String>, // Name of the handler function/device
}

/// Memory region information for debugging
#[derive(Debug, Clone)]
pub struct MemoryRegion {
    gpa: GuestAddress,
    size: u64,
    pub perms: MemoryPerms,
    pub hpa: Option<*mut std::ffi::c_void>,
}

impl MemoryRegion {
    pub fn new(gpa: GuestAddress, size: u64, perms: MemoryPerms, hpa: Option<*mut std::ffi::c_void>) -> Self {
        Self { gpa, size, perms, hpa }
    }

    pub fn len(&self) -> u64 {
        self.size
    }

    pub fn start_addr(&self) -> GuestAddress {
        self.gpa
    }

    pub fn last_addr(&self) -> GuestAddress {
        self.gpa.unchecked_add(self.size - 1)
    }

    pub fn check_address(&self, addr: GuestAddress) -> Option<GuestAddress> {
        if addr >= self.start_addr() && addr <= self.last_addr() {
            Some(addr)
        } else {
            None
        }
    }

    /// Returns the address plus the offset if it is in this region.
    pub fn checked_offset(
        &self,
        base: GuestAddress,
        offset: usize,
    ) -> Option<GuestAddress> {
        base.checked_add(offset as u64)
            .and_then(|addr| self.check_address(addr))
    }

    /// Returns a [`VolatileSlice`](struct.VoalatileSlice.html) of `count` bytes starting at
    /// `addr`.
    pub fn get_slice(&self, addr: GuestAddress, count: usize) -> Result<VolatileSlice<'static>, vm_memory::GuestMemoryError> {
        let offset_value = addr.raw_value() as usize;
        let end = offset_value.checked_add(count)
            .ok_or(vm_memory::GuestMemoryError::InvalidBackendAddress)?;

        if end > self.len() as usize {
            return Err(vm_memory::GuestMemoryError::InvalidBackendAddress);
        }

        // Get the host address
        let hpa = self.hpa.ok_or(vm_memory::GuestMemoryError::HostAddressNotAvailable)?;
        
        // Convert offset to byte offset and add to host address
        let host_ptr = unsafe {
            (hpa as *mut u8).add(offset_value)
        };

        // Create VolatileSlice from the host pointer
        // SAFETY: We've checked that offset + count is within bounds
        // The slice is created from a raw pointer, so it doesn't need to borrow from self
        unsafe {
            Ok(VolatileSlice::with_bitmap(
                host_ptr,
                count,
                (),  // Empty bitmap slice
                None,
            ))
        }
    }
}

/// Memory access violation information
#[derive(Debug)]
pub struct MemoryAccessViolation {
    pub gpa: GuestAddress,
    pub action: MemoryPerms,
    pub access_size: u32,
    pub instruction_rip: u64,
}

impl MemoryAccessViolation {
    pub fn from_exit_context(exit_context: &WHV_RUN_VP_EXIT_CONTEXT) -> Option<Self> {
        unsafe {
            if exit_context.ExitReason.0 == WHvRunVpExitReasonMemoryAccess.0 {
                let mem_access = &exit_context.Anonymous.MemoryAccess;
                let access_info = mem_access.AccessInfo.AsUINT32;
                
                // Determine access type from AccessInfo bits
                // Bit 0: Write (1) or Read (0)
                // Bit 1: Execute (1) or Data (0)
                let is_write = (access_info & 0x1) != 0;
                let is_execute = (access_info & 0x2) != 0;
                
                let action = match (is_write, is_execute) {
                    (false, false) => MemoryPerms::READ,
                    (true, false) => MemoryPerms::WRITE,
                    (false, true) => MemoryPerms::EXECUTE,
                    (true, true) => MemoryPerms::WRITE | MemoryPerms::EXECUTE,
                };
                
                Some(Self {
                    gpa: GuestAddress(mem_access.Gpa),
                    action,
                    access_size: ((access_info >> 2) & 0x7) as u32, // Bits 2-4
                    instruction_rip: exit_context.VpContext.Rip,
                })
            } else {
                None
            }
        }
    }
}

/// Memory translation debugger
pub struct MemoryManager {
    pub regions: Vec<MemoryRegion>,
    pub mmio_regions: Vec<MmioRegion>,
    access_log: Vec<(u64, bool, u64)>, // (GPA, is_write, timestamp-ish)
    pub acpi_address: Option<GuestAddress>,
}

// MemoryManager is safe to send between threads because:
// - The raw pointers (hpa) point to memory that remains valid and won't be freed from another thread
// - All operations are synchronized through the Windows Hypervisor API
// - The pointers are only used for reading/writing guest memory, which is thread-safe via WHV
unsafe impl Send for MemoryManager {}

impl MemoryManager {
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
            mmio_regions: Vec::new(),
            access_log: Vec::new(),
            acpi_address: None,
        }
    }

    pub fn get_regions(&self) -> &Vec<MemoryRegion> {
        &self.regions
    }

    pub fn get_mmio_regions(&self) -> &Vec<MmioRegion> {
        &self.mmio_regions
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

    pub fn address_in_range(&self, addr: GuestAddress) -> bool {
        self.find_region(addr).is_some()
    }

    /// Find which region contains a GPA
    pub fn find_region(&self, gpa: GuestAddress) -> Option<&MemoryRegion> {
        self.regions.iter()
            .find(|r| gpa >= r.start_addr() && gpa <= r.last_addr())
    }

    /// Find MMIO region for a GPA
    pub fn find_mmio(&self, gpa: u64) -> Option<&MmioRegion> {
        self.mmio_regions.iter()
            .find(|r| gpa >= r.gpa.0 && gpa < r.gpa.0 + r.size)
    }

    /// Print memory map
    pub fn print_memory_map(&self) {
        //eprintln!("\n=== Memory Map ===");
        for region in &self.regions {
            //eprintln!("GPA: 0x{:016X} - 0x{:016X} - {:?}",
                /* region.gpa.0,
                region.gpa.0 + region.size,
                region.perms
            ); */
        }
        
        if !self.mmio_regions.is_empty() {
            //eprintln!("\n=== MMIO Regions ===");
            for mmio in &self.mmio_regions {
                //eprintln!("GPA: 0x{:016X} - 0x{:016X} ({})",
                   /*  mmio.gpa.0,
                    mmio.gpa.0 + mmio.size,
                    mmio.name
                ); */
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
        
        //eprintln!("\n=== Recent Memory Accesses (last {}) ===", count);
        for (gpa, is_write, _) in &self.access_log[start..] {
            //eprintln!("GPA: 0x{:016X} - {}", gpa, if *is_write { "WRITE" } else { "READ" });
        }
    }

    /// Analyze memory access violation
    pub fn analyze_violation(&self, violation: &MemoryAccessViolation) -> String {
        let mut analysis = format!(
            "Memory Access Violation:\n  GPA: 0x{:016X}\n  Type: {}\n  Size: {} bytes\n  RIP: 0x{:016X}\n",
            violation.gpa.0,
            violation.action.to_string(),
            violation.access_size,
            violation.instruction_rip
        );

        if let Some(region) = self.find_region(violation.gpa) {
            analysis.push_str(&format!(
                "  Region: (0x{:016X} - 0x{:016X})\n  Protection: {:?}\n",
                region.gpa.0,
                region.gpa.0 + region.size,
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
        } else if let Some(mmio) = self.find_mmio(violation.gpa.0) {
            analysis.push_str(&format!(
                "  MMIO Region: {} (0x{:016X} - 0x{:016X})\n",
                mmio.name,
                mmio.gpa.0,
                mmio.gpa.0 + mmio.size
            ));
        } else {
            analysis.push_str("  ❌ GPA not mapped to any region!\n");
        }

        analysis
    }

    pub fn write_guest_memory(&self, code: &[u8], gpa: GuestAddress) -> Result<()> {
        let region = self.find_region(gpa).ok_or(anyhow::anyhow!("Memory not allocated"))?;
        if gpa.unchecked_add(code.len() as u64) > region.last_addr() {
            return Err(anyhow::anyhow!("Code exceeds allocated memory"));
        }
        if !region.perms.contains(MemoryPerms::WRITE) {
            return Err(anyhow::anyhow!("Memory is read-only"));
        }
        let hpa = region.hpa.ok_or(anyhow::anyhow!("Region has no host physical address"))?;
        let offset = (gpa.raw_value() - region.gpa.0) as usize;

        unsafe {
            ptr::copy_nonoverlapping(code.as_ptr(), hpa.add(offset) as *mut u8, code.len());
        }
        Ok(())
    }

    /// Read guest memory as raw bytes
    pub fn read_guest_memory(&self, gpa: GuestAddress, size: usize) -> Result<Vec<u8>> {
        let region = self.find_region(gpa).ok_or(anyhow::anyhow!("Memory not allocated"))?;
        if gpa.unchecked_add(size as u64) > region.last_addr() {
            return Err(anyhow::anyhow!("Read exceeds allocated memory"));
        }
        if !region.perms.contains(MemoryPerms::READ) {
            return Err(anyhow::anyhow!("Memory is not readable"));
        }
        let hpa = region.hpa.ok_or(anyhow::anyhow!("Region has no host physical address"))?;
        let offset = (gpa.raw_value() - region.gpa.0) as usize;
        let mut data = vec![0u8; size];
        
        unsafe {
            ptr::copy_nonoverlapping(hpa.add(offset) as *const u8, data.as_mut_ptr(), size);
        }
        Ok(data)
    }

    /// Read a typed object from guest memory
    ///
    /// This method reads bytes from guest memory and converts them to a typed object.
    /// The type `T` must implement `FromBytes` from the `zerocopy` crate
    /// to ensure safe byte-to-struct conversion.
    ///
    /// # Safety
    /// This method is safe because it uses `zerocopy`'s safe conversion traits.
    /// However, the guest memory must contain valid data for the type `T`.
    ///
    /// # Example
    /// ```rust,no_run
    /// use zerocopy::FromBytes;
    ///
    /// #[repr(C)]
    /// #[derive(FromBytes)]
    /// struct MyStruct {
    ///     field1: u32,
    ///     field2: u64,
    /// }
    ///
    /// let mem = MemoryManager::new();
    /// let addr = GuestAddress(0x1000);
    /// let obj: MyStruct = mem.read_obj(addr)?;
    /// ```
    pub fn read_obj<T>(&self, gpa: GuestAddress) -> Result<T>
    where
        T: FromBytes,
    {
        let size = std::mem::size_of::<T>();
        let bytes = self.read_guest_memory(gpa, size)?;
        
        // Use zerocopy to safely convert bytes to the type
        // FromBytes::read_from_bytes ensures the bytes are valid for the type
        // read_from_bytes returns Result<T, LayoutError>
        T::read_from_bytes(&bytes[..size])
            .map_err(|e| anyhow::anyhow!("Failed to convert bytes to type: {:?}", e))
    }

    /// Write a typed object to guest memory
    ///
    /// This method converts a typed object to bytes and writes them to guest memory.
    /// The type `T` must be a plain old data (POD) type that can be safely converted to bytes.
    ///
    /// # Safety
    /// This method uses `std::mem::transmute` to convert the object to bytes, which is safe
    /// for POD types (types that are `Copy` and have no padding). The object is written
    /// as raw bytes to the specified guest address.
    ///
    /// # Example
    /// ```rust,no_run
    /// #[repr(C)]
    /// #[derive(Copy, Clone)]
    /// struct MyStruct {
    ///     field1: u32,
    ///     field2: u64,
    /// }
    ///
    /// let mem = MemoryManager::new();
    /// let addr = GuestAddress(0x1000);
    /// let obj = MyStruct { field1: 42, field2: 100 };
    /// mem.write_obj(obj, addr)?;
    /// ```
    pub fn write_obj<T>(&self, obj: T, gpa: GuestAddress) -> Result<()>
    where
        T: Copy,
    {
        let size = std::mem::size_of::<T>();
        
        // Convert the object to bytes
        // SAFETY: This is safe for POD types (Copy types with no padding)
        // The object is copied, not moved, and we're just viewing its bytes
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &obj as *const T as *const u8,
                size
            )
        };
        
        self.write_guest_memory(bytes, gpa)
    }

    /// Atomically store a value at the specified guest address.
    ///
    /// This method performs an atomic store operation using the specified memory ordering.
    /// Supported types are: u8, u16, u32, u64, i8, i16, i32, i64, usize, isize.
    ///
    /// # Arguments
    /// * `val` - The value to store (must be one of the supported atomic types)
    /// * `addr` - The guest physical address where to store the value
    /// * `order` - The memory ordering to use (e.g., `Ordering::Release`, `Ordering::Relaxed`)
    ///
    /// # Example
    /// ```rust,no_run
    /// use std::sync::atomic::Ordering;
    ///
    /// let mem = MemoryManager::new();
    /// let addr = GuestAddress(0x1000);
    /// mem.store(42u16, addr, Ordering::Release)?;
    /// ```
    pub fn store<T>(&self, val: T, addr: GuestAddress, order: Ordering) -> Result<()>
    where
        T: AtomicStore,
    {
        let region = self.find_region(addr).ok_or(anyhow::anyhow!("Memory not allocated"))?;
        if !region.perms.contains(MemoryPerms::WRITE) {
            return Err(anyhow::anyhow!("Memory is read-only"));
        }
        let hpa = region.hpa.ok_or(anyhow::anyhow!("Region has no host physical address"))?;
        let offset = (addr.raw_value() - region.gpa.0) as usize;
        let host_ptr = unsafe { (hpa as *mut u8).add(offset) };

        // Perform atomic store based on type size
        // SAFETY: The pointer is valid and aligned (atomic types require proper alignment)
        unsafe {
            T::atomic_store(host_ptr, val, order);
        }
        
        Ok(())
    }

    /// Atomically load a value from the specified guest address.
    ///
    /// This method performs an atomic load operation using the specified memory ordering.
    /// Supported types are: u8, u16, u32, u64, i8, i16, i32, i64, usize, isize.
    ///
    /// # Arguments
    /// * `addr` - The guest physical address from which to load the value
    /// * `order` - The memory ordering to use (e.g., `Ordering::Acquire`, `Ordering::Relaxed`)
    ///
    /// # Example
    /// ```rust,no_run
    /// use std::sync::atomic::Ordering;
    ///
    /// let mem = MemoryManager::new();
    /// let addr = GuestAddress(0x1000);
    /// let value: u16 = mem.load(addr, Ordering::Acquire)?;
    /// ```
    pub fn load<T>(&self, addr: GuestAddress, order: Ordering) -> Result<T>
    where
        T: AtomicLoad,
    {
        let region = self.find_region(addr).ok_or(anyhow::anyhow!("Memory not allocated"))?;
        if !region.perms.contains(MemoryPerms::READ) {
            return Err(anyhow::anyhow!("Memory is not readable"));
        }
        let hpa = region.hpa.ok_or(anyhow::anyhow!("Region has no host physical address"))?;
        let offset = (addr.raw_value() - region.gpa.0) as usize;
        let host_ptr = unsafe { (hpa as *const u8).add(offset) };

        // Perform atomic load based on type size
        // SAFETY: The pointer is valid and aligned (atomic types require proper alignment)
        Ok(unsafe { T::atomic_load(host_ptr, order) })
    }
}

// Helper traits for atomic operations
/// Trait for types that can be atomically stored
trait AtomicStore: Copy {
    unsafe fn atomic_store(ptr: *mut u8, val: Self, order: Ordering);
}

/// Trait for types that can be atomically loaded
trait AtomicLoad: Copy {
    unsafe fn atomic_load(ptr: *const u8, order: Ordering) -> Self;
}

macro_rules! impl_atomic_ops {
    ($T:ty, $Atomic:ty) => {
        impl AtomicStore for $T {
            unsafe fn atomic_store(ptr: *mut u8, val: Self, order: Ordering) {
                let atomic_ptr = ptr as *mut $Atomic;
                // SAFETY: Caller ensures pointer is valid and properly aligned
                unsafe { (*atomic_ptr).store(val, order) };
            }
        }

        impl AtomicLoad for $T {
            unsafe fn atomic_load(ptr: *const u8, order: Ordering) -> Self {
                let atomic_ptr = ptr as *const $Atomic;
                // SAFETY: Caller ensures pointer is valid and properly aligned
                unsafe { (*atomic_ptr).load(order) }
            }
        }
    };
}

impl_atomic_ops!(u8, AtomicU8);
impl_atomic_ops!(u16, AtomicU16);
impl_atomic_ops!(u32, AtomicU32);
impl_atomic_ops!(u64, AtomicU64);
impl_atomic_ops!(i8, std::sync::atomic::AtomicI8);
impl_atomic_ops!(i16, std::sync::atomic::AtomicI16);
impl_atomic_ops!(i32, std::sync::atomic::AtomicI32);
impl_atomic_ops!(i64, std::sync::atomic::AtomicI64);
impl_atomic_ops!(usize, std::sync::atomic::AtomicUsize);
impl_atomic_ops!(isize, std::sync::atomic::AtomicIsize);

impl Clone for MemoryManager {
    fn clone(&self) -> Self {
        Self {
            regions: self.regions.clone(),
            mmio_regions: self.mmio_regions.clone(),
            access_log: self.access_log.clone(),
            acpi_address: self.acpi_address,
        }
    }
}

impl Default for MemoryManager {
    fn default() -> Self {
        Self::new()
    }
}
