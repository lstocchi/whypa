use anyhow::Result;
use windows::Win32::System::Hypervisor::*;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use tracing::info;

use crate::acpi::create_acpi_tables;
use crate::memory::memory::GuestAddress;
use crate::memory::layout;
use crate::bootparam::setup_header;


use crate::device_manager::DeviceManager;
use crate::cpu::CpuManager;
use crate::memory::memory::MemoryManager;

/// ELF64 magic: \x7fELF
const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// ELF constants
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1; // Little-endian
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;

/// Minimal ELF64 file header (64 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Elf64Ehdr {
    e_ident: [u8; 16],
    e_type: u16,
    e_machine: u16,
    e_version: u32,
    e_entry: u64,
    e_phoff: u64,
    e_shoff: u64,
    e_flags: u32,
    e_ehsize: u16,
    e_phentsize: u16,
    e_phnum: u16,
    e_shentsize: u16,
    e_shnum: u16,
    e_shstrndx: u16,
}

/// Minimal ELF64 program header (56 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct Elf64Phdr {
    p_type: u32,
    p_flags: u32,
    p_offset: u64,
    p_vaddr: u64,
    p_paddr: u64,
    p_filesz: u64,
    p_memsz: u64,
    p_align: u64,
}

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

/// Detect kernel format and load accordingly.
pub fn load_linux_kernel<P: LinuxBootPartition>(
    partition: &P,
    kernel_path: &str,
    initram_path: &str,
) -> Result<u64> {
    let mut kernel_file = File::open(kernel_path)
        .map_err(|e| anyhow::anyhow!("Failed to open kernel file: {}", e))?;

    // Read first 4 bytes to detect format
    let mut magic = [0u8; 4];
    kernel_file.read_exact(&mut magic)?;
    kernel_file.seek(SeekFrom::Start(0))?;

    if magic == ELF_MAGIC {
        info!("Detected ELF kernel image");
        load_elf_kernel(partition, kernel_path, &mut kernel_file, initram_path)
    } else {
        info!("Detected bzImage kernel");
        load_bzimage_kernel(partition, kernel_path, &mut kernel_file, initram_path)
    }
}

/// Load a bzImage format kernel (original path).
fn load_bzimage_kernel<P: LinuxBootPartition>(
    partition: &P,
    kernel_path: &str,
    kernel_file: &mut File,
    _initram_path: &str,
) -> Result<u64> {
    // Get total file size
    let total_size = kernel_file
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

    let _kernel_size = total_size - setup_size;

    let code32_start = boot_header.code32_start as u64;

    kernel_file.seek(SeekFrom::Start(setup_size as u64))?;
    let mut payload = Vec::new();
    kernel_file.read_to_end(&mut payload)?;
    partition.write_code(&payload, layout::HIGH_RAM_START.0)?;

    let initram_address: u64 = 0;
    let initram_size = 0;

    // Set up boot parameters structure (reads setup_header from file)
    let _init_size = setup_bzimage_boot_params(partition, layout::ZERO_PAGE_START, kernel_path, code32_start, initram_address, initram_size)?;

    // Kernel entry point is at HIGH_RAM_START + 0x200 (standard offset for bzImage)
    let kernel_entry = layout::HIGH_RAM_START.0 + 0x200;
    Ok(kernel_entry as u64)
}

/// Load an ELF64 format kernel.
fn load_elf_kernel<P: LinuxBootPartition>(
    partition: &P,
    kernel_path: &str,
    kernel_file: &mut File,
    _initram_path: &str,
) -> Result<u64> {
    // Read ELF header
    let mut ehdr = Elf64Ehdr::default();
    unsafe {
        let buf = std::slice::from_raw_parts_mut(
            &mut ehdr as *mut Elf64Ehdr as *mut u8,
            std::mem::size_of::<Elf64Ehdr>(),
        );
        kernel_file.read_exact(buf)?;
    }

    // Validate ELF header
    if ehdr.e_ident[0..4] != ELF_MAGIC {
        return Err(anyhow::anyhow!("Not a valid ELF file"));
    }
    if ehdr.e_ident[4] != ELFCLASS64 {
        return Err(anyhow::anyhow!("Not a 64-bit ELF (class={})", ehdr.e_ident[4]));
    }
    if ehdr.e_ident[5] != ELFDATA2LSB {
        return Err(anyhow::anyhow!("Not a little-endian ELF"));
    }
    if ehdr.e_machine != EM_X86_64 {
        return Err(anyhow::anyhow!("Not an x86_64 ELF (machine={})", ehdr.e_machine));
    }

    let entry = ehdr.e_entry;
    info!(entry = format_args!("0x{:X}", entry), phnum = ehdr.e_phnum, "Parsed ELF header");

    // Read program headers
    let phdr_size = std::mem::size_of::<Elf64Phdr>();
    if ehdr.e_phentsize as usize != phdr_size {
        return Err(anyhow::anyhow!(
            "Unexpected phentsize: {} (expected {})", ehdr.e_phentsize, phdr_size
        ));
    }

    kernel_file.seek(SeekFrom::Start(ehdr.e_phoff))?;
    let mut phdrs = vec![Elf64Phdr::default(); ehdr.e_phnum as usize];
    unsafe {
        let buf = std::slice::from_raw_parts_mut(
            phdrs.as_mut_ptr() as *mut u8,
            phdr_size * ehdr.e_phnum as usize,
        );
        kernel_file.read_exact(buf)?;
    }

    // Load each PT_LOAD segment into guest physical memory.
    // Track the vaddr→paddr delta from the first PT_LOAD so we can convert
    // e_entry (a virtual address) into a guest physical address.
    let mut lowest_paddr: u64 = u64::MAX;
    let mut highest_end: u64 = 0;
    let mut vaddr_to_paddr_offset: Option<i64> = None; // vaddr - paddr

    for (i, phdr) in phdrs.iter().enumerate() {
        if phdr.p_type != PT_LOAD {
            continue;
        }

        let load_addr = phdr.p_paddr;
        let mem_size = phdr.p_memsz;
        let file_size = phdr.p_filesz;
        let file_offset = phdr.p_offset;

        // Capture the vaddr/paddr relationship from the first PT_LOAD segment
        if vaddr_to_paddr_offset.is_none() {
            vaddr_to_paddr_offset = Some(phdr.p_vaddr.wrapping_sub(phdr.p_paddr) as i64);
        }

        info!(
            segment = i,
            vaddr = format_args!("0x{:X}", phdr.p_vaddr),
            paddr = format_args!("0x{:X}", load_addr),
            filesz = format_args!("0x{:X}", file_size),
            memsz = format_args!("0x{:X}", mem_size),
            "Loading ELF PT_LOAD segment"
        );

        // Read segment data from file
        if file_size > 0 {
            kernel_file.seek(SeekFrom::Start(file_offset))?;
            let mut data = vec![0u8; file_size as usize];
            kernel_file.read_exact(&mut data)?;
            partition.write_code(&data, load_addr)?;
        }

        // If memsz > filesz, the remainder is BSS (zero-filled).
        // Guest memory is already zeroed, so we only need to write the file data.

        lowest_paddr = lowest_paddr.min(load_addr);
        highest_end = highest_end.max(load_addr + mem_size);
    }

    if lowest_paddr == u64::MAX {
        return Err(anyhow::anyhow!("ELF has no PT_LOAD segments"));
    }

    // Determine the physical entry point.
    // e_entry may be a virtual address (e.g. 0xFFFFFFFF81000000) or already
    // a physical one (e.g. 0x1000000).  Check whether it falls inside any
    // PT_LOAD segment's physical range first; if so, use it as-is.
    // Otherwise, translate it using the vaddr→paddr delta.
    let entry_is_physical = phdrs.iter().any(|ph| {
        ph.p_type == PT_LOAD
            && entry >= ph.p_paddr
            && entry < ph.p_paddr + ph.p_memsz
    });

    let phys_entry = if entry_is_physical {
        entry
    } else {
        let vaddr_offset = vaddr_to_paddr_offset.unwrap_or(0);
        (entry as i64).wrapping_sub(vaddr_offset) as u64
    };

    let kernel_size = highest_end - lowest_paddr;
    info!(
        load_range = format_args!("0x{:X}..0x{:X}", lowest_paddr, highest_end),
        size = format_args!("0x{:X}", kernel_size),
        elf_entry = format_args!("0x{:X}", entry),
        phys_entry = format_args!("0x{:X}", phys_entry),
        translated = !entry_is_physical,
        "ELF kernel loaded"
    );

    // Set up boot parameters for the ELF kernel
    let code32_start = lowest_paddr;
    let initram_address: u64 = 0;
    let initram_size: usize = 0;
    setup_elf_boot_params(partition, layout::ZERO_PAGE_START, code32_start, kernel_size, initram_address, initram_size)?;

    Ok(phys_entry)
}

fn write_e820(boot_params: &mut [u8], offset: usize, mem_addr: u64, mem_size: u64, mem_type: u32) {
    boot_params[offset..offset+8].copy_from_slice(&mem_addr.to_le_bytes());
    boot_params[offset+8..offset+16].copy_from_slice(&mem_size.to_le_bytes());
    boot_params[offset+16..offset+20].copy_from_slice(&mem_type.to_le_bytes());
}

/// Set up Linux boot parameters for a bzImage kernel.
///
/// Reads the setup_header from the bzImage file and copies it into the zero page.
/// Returns the init_size value from the setup header.
fn setup_bzimage_boot_params<P: LinuxBootPartition>(partition: &P, gpa: GuestAddress, kernel_path: &str, code32_start: u64, initram_address: u64, initram_size: usize) -> Result<u32> {
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

    // Load the header into the Zero Page
    f.seek(SeekFrom::Start(0x1f1))?;
    f.read_exact(&mut boot_params[0x1f1..0x1f1 + setup_header_size])?;

    // Fill common boot_params fields and write to guest memory
    fill_boot_params(partition, &mut boot_params, gpa, code32_start, initram_address, initram_size)?;

    // Read init_size from setup header (offset 0x260 in boot_params)
    let init_size = u32::from_le_bytes([
        boot_params[0x260], boot_params[0x261],
        boot_params[0x262], boot_params[0x263]
    ]);

    Ok(init_size)
}

/// Set up Linux boot parameters for an ELF kernel.
///
/// Since ELF files don't contain a bzImage setup_header, we construct a minimal
/// one with the fields the kernel expects.
/// Returns the init_size (derived from the kernel's total memory footprint).
fn setup_elf_boot_params<P: LinuxBootPartition>(partition: &P, gpa: GuestAddress, code32_start: u64, kernel_size: u64, initram_address: u64, initram_size: usize) -> Result<u32> {
    let mut boot_params = vec![0u8; 4096];

    // Construct a minimal setup_header in the zero page.
    // 0x1F1: setup_sects = 0 (no real-mode setup code)
    boot_params[0x1F1] = 0;

    // 0x202: header magic "HdrS"
    boot_params[0x202..0x206].copy_from_slice(b"HdrS");

    // 0x206: version = 0x020F (modern boot protocol)
    boot_params[0x206..0x208].copy_from_slice(&0x020Fu16.to_le_bytes());

    // 0x260: init_size - use the kernel's total memory size (rounded up to page boundary)
    let init_size = ((kernel_size + 0xFFF) & !0xFFF) as u32;
    boot_params[0x260..0x264].copy_from_slice(&init_size.to_le_bytes());

    // Fill common boot_params fields and write to guest memory
    fill_boot_params(partition, &mut boot_params, gpa, code32_start, initram_address, initram_size)?;

    Ok(init_size)
}

/// Fill the common boot_params fields shared by both bzImage and ELF paths,
/// then write the finished zero page into guest memory.
fn fill_boot_params<P: LinuxBootPartition>(
    partition: &P,
    boot_params: &mut Vec<u8>,
    gpa: GuestAddress,
    code32_start: u64,
    _initram_address: u64,
    _initram_size: usize,
) -> Result<()> {
    // 0x210: type_of_loader. MUST be non-zero (0xFF = custom)
    boot_params[0x210] = 0xFF;

    // 0x211: loadflags. bit 0 (Loaded High) + bit 7 (Heap)
    boot_params[0x211] |= 0x81;

    // 0x214: code32_start (u32) - kernel load address
    boot_params[0x214..0x218].copy_from_slice(&(code32_start as u32).to_le_bytes());

    // 0x224: heap_end_ptr (u16)
    let heap_end: u16 = 0xFE00;
    boot_params[0x224..0x226].copy_from_slice(&heap_end.to_le_bytes());

    // 0x228: cmd_line_ptr (u32)
    let cmd_line = format!(
        "console=hvc0 8250.nr_uarts=0 root=/dev/vda rw init=/bin/sh loglevel=1"
    );
    let mut cmd_line_vec = cmd_line.as_bytes().to_vec();
    cmd_line_vec.push(0);

    partition.write_code(&cmd_line_vec, layout::CMDLINE_START.0)?;
    boot_params[0x228..0x22C].copy_from_slice(&(layout::CMDLINE_START.0 as u32).to_le_bytes());

    for i in 0x1E0..0x1EF {
        boot_params[i] = 0;
    }

    // Ensure "HdrS" magic is present (may already be set by caller)
    boot_params[0x202..0x206].copy_from_slice(b"HdrS");

    // E820 memory map
    let mut offset = 0x2D0;
    let regions = partition.memory_manager().get_regions();
    for region in regions {
        write_e820(boot_params, offset, region.start_addr().raw_value(), region.len(), 1);
        offset += 20;
    }

    // Add the 32-bit reserved area as reserved memory in E820 table
    if partition.get_memory_size() > layout::MEM_32BIT_RESERVED_START.0 {
        write_e820(boot_params, offset, layout::MEM_32BIT_RESERVED_START.0, layout::MEM_32BIT_RESERVED_SIZE, 2);
        offset += 20;
    }

    // Add individual MMIO regions as reserved
    let mmio_regions = partition.memory_manager().get_mmio_regions();
    for region in mmio_regions {
        write_e820(boot_params, offset, region.gpa.0, region.size, 2);
        offset += 20;
    }

    let e820_count = regions.len()
        + (if partition.get_memory_size() > layout::MEM_32BIT_RESERVED_START.0 { 1 } else { 0 })
        + mmio_regions.len();
    boot_params[0x1E8] = e820_count as u8;

    partition.write_code(boot_params, gpa.0)?;

    create_acpi_tables(partition.device_manager(), partition.cpu_manager(), partition.memory_manager());
    Ok(())
}

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

    // PML4[0] points to PDPTE
    let pml4_entry = layout::PDPTE_START.0 | 0x3; // Present + Writable
    tables[0..8].copy_from_slice(&pml4_entry.to_le_bytes());

    // Calculate the maximum address we need to map
    // We need to map: kernel (kernel_load_addr + init_size), zero page (0x0), and command line buffer
    let kernel_end = kernel_load_addr + (init_size as u64);
    let max_addr = kernel_end.max(layout::CMDLINE_START.0 + layout::CMDLINE_MAX_SIZE as u64); // At least map up to command line end
    
    // Calculate how many 1GB PDP entries we need
    let pdp_entries_needed = ((max_addr + 0x3FFFFFFF) >> 30) as usize + 1; // Round up to next 1GB boundary
    //let pdp_entries = pdp_entries_needed.min(4); // Limit to 4GB for now

    // PDPTE: Map entries to cover required range (each entry covers 1GB)
    let pdpte_offset = (layout::PDPTE_START.0 - layout::PML4_START.0) as usize;
    for i in 0..4 {
        let pdp_entry = ((i as u64) << 30) | 0x83; // Present + Writable + Page Size (1GB pages)
        let offset = pdpte_offset + (i * 8);
        tables[offset..offset + 8].copy_from_slice(&pdp_entry.to_le_bytes());
    }

    partition.write_code(&tables, layout::PML4_START.0)?;
    //eprintln!("  ✓ Identity paging written to 0x{:X} (mapping up to 0x{:X})", layout::PML4_START.0, max_addr);
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
        partition.write_code(std::slice::from_raw_parts(gdt.as_ptr() as *const u8, 32), layout::BOOT_GDT_START.0)?;

        let mut names = Vec::new();
        let mut values = Vec::new();

        // --- GDTR ---
        names.push(WHvX64RegisterGdtr);
        values.push(WHV_REGISTER_VALUE {
            Table: WHV_X64_TABLE_REGISTER { Base: layout::BOOT_GDT_START.0, Limit: 31, ..Default::default() }, // 4 entries * 8 bytes - 1
        });

        // --- Control Registers: 64-bit mode with paging enabled ---
        names.push(WHvX64RegisterCr3);  
        values.push(WHV_REGISTER_VALUE { Reg64: layout::PML4_START.0 });
        
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
        values.push(WHV_REGISTER_VALUE { Reg64: layout::ZERO_PAGE_START.0 });
        
        // RSP: Set stack pointer
        names.push(WHvX64RegisterRsp); 
        values.push(WHV_REGISTER_VALUE { Reg64: layout::BOOT_STACK_POINTER.0 });
        
        // RFLAGS: Interrupt must be disabled (IF flag = 0)
        // RFLAGS = 0x2 (bit 1 = reserved, always 1; IF bit 9 = 0)
        names.push(WHvX64RegisterRflags); 
        values.push(WHV_REGISTER_VALUE { Reg64: 0x2 });

        WHvSetVirtualProcessorRegisters(handle, vp_id, names.as_ptr(), names.len() as u32, values.as_ptr())?;
        Ok(())
    }
}
