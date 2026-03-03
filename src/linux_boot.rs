use anyhow::{Result, Context};
use windows::Win32::System::Hypervisor::*;
use std::fs::File;

use crate::acpi::create_acpi_tables;
use crate::memory::memory::GuestAddress;
use crate::bootparam::setup_header;

/// Linux boot constants
const KERNEL_BASE: u64 = 0x100000; // 1MB - standard Linux kernel load address
pub const BOOT_PARAMS_BASE: GuestAddress = GuestAddress(0x10000); // Zero page - boot parameters location
const CMD_LINE_OFFSET: u64 = 0x1000; // Command line buffer location

use crate::device_manager::DeviceManager;
use crate::cpu::CpuManager;
use crate::memory::memory::MemoryManager;

/// Trait for partition operations needed for Linux boot
pub trait LinuxBootPartition {
    fn load_file(&self, file_path: &str, gpa: u64) -> Result<usize>;
    fn write_code(&self, code: &[u8], gpa: u64) -> Result<()>;
    fn get_handle(&self) -> WHV_PARTITION_HANDLE;
    fn get_memory_size(&self) -> u64;
    fn device_manager(&self) -> &DeviceManager;
    fn cpu_manager(&self) -> &CpuManager;
    fn memory_manager(&self) -> &MemoryManager;
}

/// Load Linux kernel and set up boot parameters
/// Kernel is loaded at 0x100000 (1MB), boot params at 0x10000 (64KB)
pub fn load_linux_kernel<P: LinuxBootPartition>(
    partition: &P,
    kernel_path: &str,
    initram_path: &str,
) -> Result<u64> {
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    
    //eprintln!("Loading Linux kernel from: {}", kernel_path);

    let mut initram_data = Vec::new();
    if !initram_path.is_empty() {
        let mut initramfd = File::open(initram_path)
            .with_context(|| format!("Failed to open initramfs file: {}", initram_path))?;
        initramfd.read_to_end(&mut initram_data)?;
    }
    
    let mut kernel_file = File::open(kernel_path)
        .map_err(|e| anyhow::anyhow!("Failed to open kernel file: {}", e))?;
    
    // Get total file size
    let mut total_size = kernel_file
        .seek(SeekFrom::End(0))
        .map_err(|e| anyhow::anyhow!("Failed to seek to end: {}", e))? as usize;

    // Read boot header (setup_header structure) -> https://www.kernel.org/doc/Documentation/x86/boot.rst
    // The setup_header starts at offset 0x1f1 in the kernel image
    kernel_file.seek(SeekFrom::Start(0x1f1))?;
    
    // Read the struct as bytes (size is 123 bytes according to bindgen test)
    let mut boot_header = setup_header::default();
    unsafe {
        let header_bytes = std::slice::from_raw_parts_mut(
            &mut boot_header as *mut setup_header as *mut u8,
            std::mem::size_of::<setup_header>()
        );
        kernel_file.read_exact(header_bytes)?;
    }
    // Key fields we need:
    // - offset 0x1F1: setup_sects (u8) - number of setup sectors
    // - offset 0x202: header (magic "HdrS" = 0x53726448)
    // - offset 0x206: version (u16) 
    // - offset 0x211: loadflags (u8)
    // - offset 0x214: code32_start (u32) - kernel load address

    // Copy the field to avoid unaligned access (packed struct)
    let header_magic = boot_header.header;
    if header_magic != 0x5372_6448 {
        return Err(anyhow::anyhow!("Invalid bzImage: missing HdrS magic (got 0x{:X})", header_magic));
    }

    if (boot_header.version < 0x0200) || ((boot_header.loadflags & 0x1) == 0x0) {
        let version = boot_header.version as u16;
        return Err(anyhow::anyhow!("Unsupported bzImage version: 0x{:X}", version));
    }

    let mut setup_size = boot_header.setup_sects as usize;
    if setup_size == 0 {
        setup_size = 4;
    }
    setup_size = (setup_size + 1) * 512;

    let kernel_size = total_size - setup_size;

    let code32_start = boot_header.code32_start as u64;
    let kernel_load_addr = if code32_start >= KERNEL_BASE {
        code32_start
    } else {
        KERNEL_BASE
    };
    
    //eprintln!("  bzImage header parsed:");
    //eprintln!("    Kernel size: {} bytes", kernel_size);
    //eprintln!("    Kernel load address: 0x{:X}", kernel_load_addr);

    kernel_file.seek(SeekFrom::Start(setup_size as u64))?;
    let mut payload = Vec::new();
    kernel_file.read_to_end(&mut payload)?;
    partition.write_code(&payload, KERNEL_BASE)?;

    // Step 1: Place initramfs in guest physical memory
    // Read initrd_addr_max from the kernel header (offset 0x22C in bzImage = field in setup_header)
    // This is the highest physical address where the initramfs end may be placed.
    let initrd_addr_max = boot_header.initrd_addr_max;
    
    let mut initram_address: u64 = 0;
    let initram_size = initram_data.len();
    
    if !initram_data.is_empty() {
        // Pick a page-aligned address as high as possible below initrd_addr_max,
        // but also within the guest's physical memory.
        let max_addr = (initrd_addr_max as u64 + 1).min(partition.get_memory_size());
        let kernel_end = KERNEL_BASE + kernel_size as u64;
        
        // Align initramfs start down to page boundary (4KB)
        initram_address = (max_addr - initram_size as u64) & !0xFFF;
        
        // Sanity check: must not overlap with the kernel
        if initram_address < kernel_end {
            return Err(anyhow::anyhow!(
                "Not enough memory to place initramfs: need 0x{:X} but kernel ends at 0x{:X}, initrd_addr_max=0x{:X}",
                initram_size, kernel_end, initrd_addr_max
            ));
        }
        
        partition.write_code(&initram_data, initram_address)?;
    }
    
    // Step 2: Set up boot parameters structure
    // init_size is stored in boot_params and will be read later when setting up paging
    let _init_size = setup_linux_boot_params(partition, BOOT_PARAMS_BASE, kernel_path, code32_start, initram_address, initram_size)?;
    
    // Kernel entry point is at kernel_load_addr + 0x200 (standard offset)
    let kernel_entry = KERNEL_BASE + 0x200;
    
    //eprintln!("  Kernel entry point: 0x{:X}", kernel_entry);
    Ok(kernel_entry as u64)
}

fn write_e820(boot_params: &mut [u8], offset: usize, mem_addr: u64, mem_size: u64, mem_type: u32) {
    boot_params[offset..offset+8].copy_from_slice(&mem_addr.to_le_bytes());
    boot_params[offset+8..offset+16].copy_from_slice(&mem_size.to_le_bytes());
    boot_params[offset+16..offset+20].copy_from_slice(&mem_type.to_le_bytes());
}

/// Set up Linux boot parameters structure according to 64-bit boot protocol
/// 
/// In 64-bit boot protocol:
/// 1. Allocate memory for struct boot_params (zero page) and initialize to all zero
/// 2. Load the setup header at offset 0x01f1 of kernel image into struct boot_params
/// 3. The end of setup header is calculated as: 0x0202 + byte value at offset 0x0201
/// 4. Fill additional fields of struct boot_params as described in Zero Page chapter
/// 
/// Returns the init_size value from the setup header
fn setup_linux_boot_params<P: LinuxBootPartition>(partition: &P, gpa: GuestAddress, kernel_path: &str, code32_start: u64, initram_address: u64, initram_size: usize) -> Result<u32> {
    use std::io::{Read, Seek, SeekFrom};

    //eprintln!("  Setting up Linux boot parameters at 0x{:X} with code32_start 0x{:X}", gpa.raw_value(), code32_start);
    
    // 1. Allocate and initialize boot_params to all zero (as per 64-bit boot protocol)
    let mut boot_params = vec![0u8; 4096];
    let mut f = std::fs::File::open(kernel_path)?;

    // 2. Load the setup header at offset 0x01f1 of kernel image into struct boot_params
    // First, read the byte at offset 0x0201 to calculate setup header end
    f.seek(SeekFrom::Start(0x0201))?;
    let mut setup_header_size_byte = [0u8; 1];
    f.read_exact(&mut setup_header_size_byte)?;

    let setup_header_end = 0x0202 + (setup_header_size_byte[0] as usize);
    let setup_header_size = setup_header_end - 0x1f1;

    // 2. Load the header into the Zero Page
    f.seek(SeekFrom::Start(0x1f1))?;
    f.read_exact(&mut boot_params[0x1f1..0x1f1 + setup_header_size])?;
       
    // 0x210: type_of_loader. MUST be non-zero (0xFF = custom)
    boot_params[0x210] = 0xFF;

    // 0x211: loadflags. bit 0 (Loaded High) + bit 7 (Heap)
    boot_params[0x211] |= 0x81; 
    
    // 0x214: code32_start (u32) - kernel load address
    boot_params[0x214..0x218].copy_from_slice(&(code32_start as u32).to_le_bytes());

    // Step 2: Tell the kernel about the initramfs via ramdisk_image / ramdisk_size
    // 0x218: ramdisk_image (u32) - GPA where the initramfs is loaded
    boot_params[0x218..0x21C].copy_from_slice(&(initram_address as u32).to_le_bytes());
    // 0x21C: ramdisk_size (u32) - size of the initramfs in bytes
    boot_params[0x21C..0x220].copy_from_slice(&(initram_size as u32).to_le_bytes());

    // 0x224: heap_end_ptr (u16)
    let heap_end: u16 = 0xFE00;
    boot_params[0x224..0x226].copy_from_slice(&heap_end.to_le_bytes());

    // 0x228: cmd_line_ptr (u32). Point to a null terminator at the end of the page.
    // lpj stands for "Loops Per Jiffy" - it is needed bc Fast TSC calibration failed
    // 2000000 is a safe, standard value for modern virtualized CPUs
    // virtio_mmio.device tells Linux to probe for a virtio-mmio device
    // Format: virtio_mmio.device=<size>@<baseaddr>:<irq>
    // Use the layout-defined address in the 32-bit reserved area (0xF8000000)
    use crate::memory::layout::VIRTIO_MMIO_START;
    let virtio_base = VIRTIO_MMIO_START.0;
    let cmd_line = format!(
        "console=ttyS0 earlycon=uart8250,io,0x3f8 no_timer_check clocksource=tsc tsc=reliable noreplace-smp lpj=2000000 root=/dev/vda1 rw virtio_mmio.device=4K@0x{:08x}:20 virtio_mmio.device=4K@0xc0001000:21 rootwait rootdelay=1 raid=noautodetect systemd.mask=systemd-vconsole-setup.service", 
        virtio_base
    );//eprintln!("  Kernel command line: {}", cmd_line);
    let cmd_line_bytes = cmd_line.as_bytes();

    let mut cmd_line_vec = cmd_line_bytes.to_vec();
    cmd_line_vec.push(0);

    let cmd_ptr = gpa.unchecked_add(CMD_LINE_OFFSET);

    partition.write_code(&cmd_line_vec, cmd_ptr.0)?;
    boot_params[0x228..0x22C].copy_from_slice(&(cmd_ptr.0 as u32).to_le_bytes());
    
    for i in 0x1E0..0x1EF {
        boot_params[i] = 0;
    }
    // 0x1E8: e820_entries (u8) - Number of entries in e820_table
    // NOTE: This is at offset 0x1E8, NOT 0x1E0 (which is alt_mem_k)
    
    boot_params[0x202..0x206].copy_from_slice(b"HdrS");

    // 0x2D0: e820_table - E820 memory map table (array of struct e820_entry)
    // Each entry is 20 bytes: addr (u64), size (u64), type (u32)
    // Type 1 = RAM, Type 2 = Reserved
    let memory_size = partition.get_memory_size() - 0x100000;
    let mut offset = 0x2D0;
    let regions = partition.memory_manager().get_regions();
    for region in regions {
        write_e820(&mut boot_params, offset, region.start_addr().raw_value(), region.len(), 1);
        offset += 20;
    }
    
    // Add the 32-bit reserved area as reserved memory in E820 table
    // This is the hole between low RAM (3GB) and high RAM (4GB)
    use crate::memory::layout::{MEM_32BIT_RESERVED_START, MEM_32BIT_RESERVED_SIZE};
    if partition.get_memory_size() > MEM_32BIT_RESERVED_START.0 {
        write_e820(&mut boot_params, offset, MEM_32BIT_RESERVED_START.0, MEM_32BIT_RESERVED_SIZE, 2);
        offset += 20;
    }
    
    // Add individual MMIO regions as reserved
    let mmio_regions = partition.memory_manager().get_mmio_regions();
    for region in mmio_regions {
        write_e820(&mut boot_params, offset, region.gpa.0, region.size, 2);
        offset += 20;
    }

    let e820_count = regions.len() + 
        (if partition.get_memory_size() > MEM_32BIT_RESERVED_START.0 { 1 } else { 0 }) +
        mmio_regions.len();
    boot_params[0x1E8] = e820_count as u8;
    /* write_e820(&mut boot_params, offset, 0x0, 0x400, 1);
    write_e820(&mut boot_params, offset + 20, 0x400, 0x9FC00, 1);
    write_e820(&mut boot_params, offset + 40, 0x100000, memory_size, 1);*/
    


    // Read init_size from setup header (offset 0x260 in boot_params)
    // init_size is a u32 that specifies the size of the kernel initialization code
    let init_size = u32::from_le_bytes([
        boot_params[0x260], boot_params[0x261], 
        boot_params[0x262], boot_params[0x263]
    ]);

    partition.write_code(&boot_params, gpa.0)?;
    //eprintln!("  ✓ Boot parameters finalized at 0x{:X} (setup header size: {} bytes, init_size: 0x{:X})", gpa.raw_value(), setup_header_size, init_size);


    create_acpi_tables(partition.device_manager(), partition.cpu_manager(), partition.memory_manager());
    Ok(init_size)
}
const PAGE_TABLE_BASE: u64 = 0x9000; // Place tables starting at 36KB

/// Set up identity paging for 64-bit boot protocol
/// 
/// Maps the following ranges with identity mapping:
/// - Kernel + init_size: from kernel_load_addr to kernel_load_addr + init_size
/// - Zero page (boot_params): BOOT_PARAMS_BASE (typically 0x0)
/// - Command line buffer: at the end of the zero page
pub fn setup_identity_paging<P: LinuxBootPartition>(
    partition: &P,
    kernel_load_addr: u64,
    init_size: u32,
) -> Result<()> {
    // We'll use 1 PML4 table and 1 PDP table.
    // Each table is 4096 bytes (512 entries each table - each entry is 8 bytes). Total = 8KB.
    let mut tables = vec![0u8; 4096 * 2];

    // PML4[0] points to PDP (at PAGE_TABLE_BASE + 0x1000)
    let pml4_entry = (PAGE_TABLE_BASE + 0x1000) | 0x3; // Present + Writable
    tables[0..8].copy_from_slice(&pml4_entry.to_le_bytes());

    // Calculate the maximum address we need to map
    // We need to map: kernel (kernel_load_addr + init_size), zero page (0x0), and command line buffer
    let kernel_end = kernel_load_addr + (init_size as u64);
    let max_addr = kernel_end.max(0x10000); // At least map up to 64KB for zero page + command line
    
    // Calculate how many 1GB PDP entries we need
    let pdp_entries_needed = ((max_addr + 0x3FFFFFFF) >> 30) as usize + 1; // Round up to next 1GB boundary
    //let pdp_entries = pdp_entries_needed.min(4); // Limit to 4GB for now

    // PDP: Map entries to cover required range (each PDP entry covers 1GB)
    for i in 0..4 {
        let pdp_entry = ((i as u64) << 30) | 0x83; // Present + Writable + Page Size (1GB pages)
        let offset = 0x1000 + (i * 8);
        tables[offset..offset + 8].copy_from_slice(&pdp_entry.to_le_bytes());
    }

    partition.write_code(&tables, PAGE_TABLE_BASE)?;
    //eprintln!("  ✓ Identity paging written to 0x{:X} (mapping up to 0x{:X})", PAGE_TABLE_BASE, max_addr);
    Ok(())
}

/// Set up registers for 64-bit boot protocol
/// 
/// Requirements:
/// - CPU must be in 64-bit mode with paging enabled
/// - GDT must be loaded with descriptors for selectors __BOOT_CS(0x10) and __BOOT_DS(0x18)
/// - Both descriptors must be 4G flat segment
/// - __BOOT_CS must have execute/read permission
/// - __BOOT_DS must have read/write permission
/// - CS must be __BOOT_CS and DS, ES, SS must be __BOOT_DS
/// - Interrupt must be disabled
/// - %rsi must hold the base address of the struct boot_params
pub fn setup_linux_registers<P: LinuxBootPartition>(partition: &P, handle: WHV_PARTITION_HANDLE, vp_id: u32, kernel_entry: u64) -> Result<()> {
    unsafe {
        // 1. GDT for 64-bit mode
        // GDT structure: [null, __BOOT_CS(0x10), __BOOT_DS(0x18)]
        // Each entry is 8 bytes
        // 
        // __BOOT_CS (selector 0x10 = index 2): 64-bit code segment, execute/read
        //   Base: 0, Limit: 0xffffffff (4G flat)
        //   P=1, S=1, Type=1010 (execute/read), L=1 (64-bit), D=0
        //   GDT entry format: [limit_low(16) | base_low(16) | base_mid(8) | type(4) flags(4) | limit_high(4) flags2(4) | base_high(8)]
        //   64-bit code: 0x00AF9A000000FFFF
        //   Breaking it down:
        //   - limit_low: 0xFFFF
        //   - base_low: 0x0000
        //   - base_mid: 0x00
        //   - type/flags: 0x9A (P=1, S=1, Type=1010)
        //   - limit_high/flags2: 0xAF (G=1, D=0, L=1, limit_high=0xF)
        //   - base_high: 0x00
        //
        // __BOOT_DS (selector 0x18 = index 3): 64-bit data segment, read/write
        //   Base: 0, Limit: 0xffffffff (4G flat)
        //   P=1, S=1, Type=0010 (read/write), L=0, D=0
        //   64-bit data: 0x00CF93000000FFFF
        //   Breaking it down:
        //   - limit_low: 0xFFFF
        //   - base_low: 0x0000
        //   - base_mid: 0x00
        //   - type/flags: 0x93 (P=1, S=1, Type=0010)
        //   - limit_high/flags2: 0xCF (G=1, D=0, L=0, limit_high=0xF)
        //   - base_high: 0x00
        
        let gdt: [u64; 4] = [
            0x0000000000000000,                    // Null descriptor (index 0)
            0x0000000000000000,                    // Unused (index 1)
            0x00AF9A000000FFFF,                    // __BOOT_CS (index 2, selector 0x10): 64-bit code, execute/read
            0x00CF93000000FFFF,                    // __BOOT_DS (index 3, selector 0x18): 64-bit data, read/write
        ];
        partition.write_code(std::slice::from_raw_parts(gdt.as_ptr() as *const u8, 32), 0x500)?;

        let mut names = Vec::new();
        let mut values = Vec::new();

        // --- GDTR ---
        names.push(WHvX64RegisterGdtr);
        values.push(WHV_REGISTER_VALUE {
            Table: WHV_X64_TABLE_REGISTER { Base: 0x500, Limit: 31, ..Default::default() }, // 4 entries * 8 bytes - 1
        });

        // --- Control Registers: 64-bit mode with paging enabled ---
        names.push(WHvX64RegisterCr3);  
        values.push(WHV_REGISTER_VALUE { Reg64: PAGE_TABLE_BASE });
        
        names.push(WHvX64RegisterCr4);  
        values.push(WHV_REGISTER_VALUE { Reg64: 0x20 }); // PAE (required for Long Mode)
        
        names.push(WHvX64RegisterEfer); 
        values.push(WHV_REGISTER_VALUE { Reg64: 0x100 }); // LME (Long Mode Enable) + LMA (Long Mode Active)
        
        names.push(WHvX64RegisterCr0);  
        values.push(WHV_REGISTER_VALUE { Reg64: 0x80050033 }); // PG (Paging) + PE (Protected Mode) + NE + ET + MP

        // --- Segment Registers (64-bit mode) ---
        // __BOOT_CS (selector 0x10): 64-bit code segment, execute/read, 4G flat
        let mut cs_desc = WHV_X64_SEGMENT_REGISTER::default();
        cs_desc.Selector = 0x10; // __BOOT_CS
        cs_desc.Base = 0;
        cs_desc.Limit = 0xffffffff;
        // Attributes: P=1, S=1, Type=1010 (execute/read), L=1 (64-bit), G=1, D=0
        // 0xA09B = 1010 0000 1001 1011
        // Breaking down: G=1, D=0, L=1, AVL=0, P=1, DPL=00, S=1, Type=1010
        cs_desc.Anonymous.Attributes = 0xA09B;

        // __BOOT_DS (selector 0x18): 64-bit data segment, read/write, 4G flat
        let mut ds_desc = WHV_X64_SEGMENT_REGISTER::default();
        ds_desc.Selector = 0x18; // __BOOT_DS
        ds_desc.Base = 0;
        ds_desc.Limit = 0xffffffff;
        // Attributes: P=1, S=1, Type=0010 (read/write), L=0, G=1, D=0
        // 0xC093 = 1100 0000 1001 0011
        // Breaking down: G=1, D=0, L=0, AVL=0, P=1, DPL=00, S=1, Type=0010
        ds_desc.Anonymous.Attributes = 0xC093;

        names.push(WHvX64RegisterCs); 
        values.push(WHV_REGISTER_VALUE { Segment: cs_desc });
        
        // DS, ES, SS must be __BOOT_DS
        for reg in &[WHvX64RegisterDs, WHvX64RegisterEs, WHvX64RegisterSs] {
            names.push(*reg);
            values.push(WHV_REGISTER_VALUE { Segment: ds_desc });
        }

        // --- Execution Registers ---
        names.push(WHvX64RegisterRip); 
        values.push(WHV_REGISTER_VALUE { Reg64: kernel_entry });
        
        // %rsi must hold the base address of the struct boot_params
        names.push(WHvX64RegisterRsi); 
        values.push(WHV_REGISTER_VALUE { Reg64: BOOT_PARAMS_BASE.0 });
        
        // RSP: Set stack pointer
        names.push(WHvX64RegisterRsp); 
        values.push(WHV_REGISTER_VALUE { Reg64: 0x80000 });
        
        // RFLAGS: Interrupt must be disabled (IF flag = 0)
        // RFLAGS = 0x2 (bit 1 = reserved, always 1; IF bit 9 = 0)
        names.push(WHvX64RegisterRflags); 
        values.push(WHV_REGISTER_VALUE { Reg64: 0x2 });

        WHvSetVirtualProcessorRegisters(handle, vp_id, names.as_ptr(), names.len() as u32, values.as_ptr())?;
        Ok(())
    }
}