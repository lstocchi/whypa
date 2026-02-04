# Memory Management Implementation Guide

This guide explains how to use the new memory management features you've implemented.

## Features Implemented

### 1. Memory Protection

You can now map memory regions with different protection levels:

- **ReadWriteExecute** - Full access (default)
- **ReadWrite** - No execution (data only)
- **ReadExecute** - No writes (code/ROM)
- **ReadOnly** - Read-only data
- **ExecuteOnly** - Execute-only code (W^X security)

### 2. Memory-Mapped I/O (MMIO) Tracking

MMIO regions are automatically tracked and can be queried for debugging.

### 3. Page Fault Handling

The hypervisor now detects and reports:
- Unmapped memory accesses
- Protection violations
- MMIO accesses

### 4. Memory Translation Debugging

Tools for analyzing memory layout and access patterns.

## Usage Examples

### Example 1: Map Memory with Protection

```rust
use partition::Partition;
use memory::MemoryProtection;

let mut partition = Partition::new()?;
partition.configure(1)?;
partition.setup()?;
partition.create_vp(0)?;

// Allocate main memory (read-write-execute)
partition.allocate_memory()?;

// Map a read-only region (e.g., ROM)
let rom_data = vec![0x90; 1024]; // NOP instructions
let rom_memory = unsafe { VirtualAlloc(...) };
// ... copy rom_data to rom_memory ...
partition.map_gpa_range_with_protection(
    rom_memory,
    0xE0000,  // Typical ROM location
    1024,
    MemoryProtection::ReadOnly,
    "BIOS ROM"
)?;
```

### Example 2: Register MMIO Region

```rust
// Register VirtIO MMIO (already done in map_virtio_mmio)
partition.map_virtio_mmio()?; // This automatically registers it

// Or register a custom MMIO region
partition.register_mmio_region(
    0xF0000000,  // Custom MMIO base
    0x1000,      // 4KB
    "Custom Device",
    Some("custom_handler")
)?;
```

### Example 3: Change Memory Protection

```rust
// Make code section execute-only (W^X)
partition.change_gpa_protection(
    0x1000,     // Code start
    0x10000,    // 64KB
    MemoryProtection::ReadExecute
)?;
```

### Example 4: Debug Memory Layout

```rust
// Print complete memory map
partition.print_memory_map();

// Print recent memory accesses
partition.print_memory_access_log(50); // Last 50 accesses
```

### Example 5: Handle Page Faults

The exit handler now automatically:
- Detects unmapped memory accesses
- Reports protection violations
- Logs all memory accesses

You'll see output like:
```
⚠️  Page Fault Detected!
Memory Access Violation:
  GPA: 0x0000000000003000
  Type: WRITE
  Size: 4 bytes
  RIP: 0x0000000000000100
  Region: Main VM Memory (0x0000000000000000 - 0x000000007FFFFFFF)
  Protection: ReadExecute
  ❌ Access violates region protection!
```

## Implementation Details

### Memory Protection Flags

The `MemoryProtection` enum maps to Windows Hypervisor Platform flags:

| Protection | Flags |
|------------|-------|
| ReadWriteExecute | READ \| WRITE \| EXECUTE |
| ReadWrite | READ \| WRITE |
| ReadExecute | READ \| EXECUTE |
| ReadOnly | READ |
| ExecuteOnly | EXECUTE |

### MMIO Handling

MMIO regions are tracked separately from regular memory. When a memory access exit occurs:

1. Check if GPA is in an MMIO region
2. If yes, handle via device emulation
3. If no, check if it's a valid memory region
4. If unmapped, report as page fault

### Page Fault Detection

Page faults are detected when:
- Access to unmapped GPA
- Access violates protection (e.g., write to read-only)
- Execute access to non-executable region

## Next Steps

1. **Test with different protection levels**
   - Try writing to a read-only region
   - Try executing non-executable code

2. **Add more MMIO devices**
   - Serial port (COM1 at 0x3F8)
   - Timer/PIT
   - Keyboard controller

3. **Implement proper page fault handling**
   - Inject page fault exception to guest
   - Handle guest page fault handler

4. **Add memory statistics**
   - Track access counts per region
   - Identify hot spots

## Common Issues

### "GPA not mapped to any region!"
- The guest accessed unmapped memory
- Either map the region or inject a page fault

### "Access violates region protection!"
- Guest tried to write to read-only memory
- Or execute non-executable code
- This is expected behavior - handle accordingly

### Type errors with WHV_MAP_GPA_RANGE_FLAGS
- The Windows API uses a specific flag type
- Use `MemoryProtection::to_flags()` to convert

## API Reference

### Partition Methods

- `map_gpa_range_with_protection()` - Map with specific protection
- `change_gpa_protection()` - Change existing region protection
- `register_mmio_region()` - Register MMIO region for tracking
- `print_memory_map()` - Print all mapped regions
- `print_memory_access_log()` - Print recent accesses

### MemoryDebugger

- `register_region()` - Register memory region
- `register_mmio()` - Register MMIO region
- `find_region()` - Find region containing GPA
- `find_mmio()` - Find MMIO region for GPA
- `analyze_violation()` - Analyze access violation
