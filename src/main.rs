#[cfg(not(target_os = "windows"))]
compile_error!("This project only supports Windows.");

mod cpu;
mod partition;
mod memory;
mod emulator;
mod linux_boot;
mod bootparam;
mod acpi;
mod device_manager;
mod devices;
mod byte_order;

use partition::{Partition, MmioHandler};
use memory::memory::MemoryPerms;
use tracing::debug;
use tracing_subscriber::filter::LevelFilter;
use anyhow::Context;
use anyhow::Result;
use std::sync::{Arc, Mutex};

/// Install and configure the tracing/logging system.
fn install_tracing() {
    use tracing_error::ErrorLayer;
    use tracing_subscriber::fmt;
    use tracing_subscriber::prelude::*;

    let format = fmt::format().without_time().with_target(false).compact();

    let fmt_layer = fmt::layer()
        .event_format(format)
        .with_writer(std::io::stderr);
    let filter_layer = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .with_env_var("WHYP_LOG")
        .from_env_lossy();

    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(ErrorLayer::default())
        .init();
}


static mut PM_TICK: u32 = 0;
fn main() -> anyhow::Result<()> {
    install_tracing();

    tracing::debug!("Starting VM...");
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Init tokio runtime")?;

    let (_tx, mut rx) = tokio::sync::mpsc::channel::<String>(100);

    rt.spawn(async move {
        while let Some(request) = rx.recv().await {
            match request.as_str() {
                "quit" => {
                    break;
                }
                _ => {
                    //eprintln!("Unknown request: {:?}", request);
                }
            }
        }
    });


    let memory_size = (5 as usize) * 1024 * 1024 * 1024 ; // 5GB for Linux
    let mut partition = Partition::new(memory_size).context("Failed to create partition")?;

    partition.configure(1)?;
    partition.setup()?;
    partition.create_vp(0)?;

    // Create a simple test MMIO handler
    struct TestMmioHandler {
        magic_value: u32,
    }
    
    impl MmioHandler for TestMmioHandler {
        fn handle_read(&self, offset: u64, size: u32) -> Result<u64> {
            //eprintln!("  [MMIO Handler] Read request: offset=0x{:X}, size={:?}", offset, size);
            match offset {
                0x0 => {
                    //eprintln!("  [MMIO Handler] Returning magic value: 0x{:X}", self.magic_value);
                    Ok(self.magic_value as u64)
                }
                0x4 => {
                    let version = 0x12345678u32;
                    //eprintln!("  [MMIO Handler] Returning version: 0x{:X}", version);
                    Ok(version as u64)
                }
                _ => {
                    //eprintln!("  [MMIO Handler] Unknown offset, returning 0");
                    Ok(0)
                }
            }
        }
        
        fn handle_write(&mut self, offset: u64, size: u32, value: u64) -> Result<()> {
            //eprintln!("  [MMIO Handler] Write request: offset=0x{:X}, size={:?}, value=0x{:X}", offset, size, value);
            Ok(())
        }
    }
    
    // Check if we should boot Linux kernel or use test assembly
    let kernel_path = std::env::var("KERNEL_PATH").unwrap_or_else(|_| "kernels/vmlinuz2".to_string());
    let initram_path = std::env::var("INITRAM_PATH").unwrap_or_else(|_| "kernels/initramfs.img".to_string());
    
    
    
    partition.allocate_memory_with_size(memory_size as u64, MemoryPerms::RWX)?;
    
    // Register MMIO region at 0x180000000 (6GB, well above allocated memory)
    /* const MMIO_BASE: u64 = 0x180000000;
    //eprintln!("Registering test MMIO region at 0x{:X}...", MMIO_BASE);
    partition.register_mmio_region(
        MMIO_BASE,
        0x100,  // 256 bytes
        "Test MMIO Device".to_string(),
        Some("test_mmio".to_string()),
    )?;
    
    // Register the handler
    partition.register_mmio_handler(
        "test_mmio".to_string(),
        Box::new(TestMmioHandler {
            magic_value: 0xDEADBEEF,
        }),
    );
    //eprintln!("MMIO handler registered!"); */
    
    // Register IOAPIC MMIO region
    use crate::memory::layout::IOAPIC_START;
    use crate::devices::legacy::ioapic::{IoApic, IoApicMmioAdapter};
    use crate::devices::legacy::irqchip::IrqChipDevice;
    const IOAPIC_BASE: u64 = IOAPIC_START.0;
    const IOAPIC_REGION_SIZE: u64 = 0x1000; // 4KB IOAPIC MMIO region

    // Create the software IOAPIC backed by WHP interrupt injection
    let ioapic = IoApic::new(partition.handle);
    let ioapic_irqchip: crate::devices::legacy::irqchip::IrqChip = Arc::new(Mutex::new(
        IrqChipDevice::new(Box::new(ioapic))
    ));
    let ioapic_mmio_adapter = IoApicMmioAdapter::new(ioapic_irqchip.clone());

    // Register IOAPIC MMIO region
    partition.register_mmio_region(
        IOAPIC_BASE,
        IOAPIC_REGION_SIZE,
        "IOAPIC".to_string(),
        Some("ioapic".to_string()),
    )?;

    // Register the IOAPIC MMIO handler
    partition.register_mmio_handler(
        "ioapic".to_string(),
        Box::new(ioapic_mmio_adapter),
    );

    
    

    
    //eprintln!("IOAPIC registered at 0x{:X} (size: 0x{:X})", IOAPIC_BASE, IOAPIC_REGION_SIZE);

    // Register PCI ECAM (memory-mapped config space) MMIO region
    // When ACPI works, the kernel discovers the PCI root bridge and tries to scan
    // PCI buses via ECAM at PCI_MMCONFIG_START. Without a handler, the MMIO access
    // to unmapped GPA causes the VM to stop. Return 0xFFFFFFFF for all reads
    // (standard PCI response meaning "no device present").
    use crate::memory::layout::{PCI_MMCONFIG_START, PCI_MMCONFIG_SIZE};
    
    struct PciEcamHandler;
    
    impl MmioHandler for PciEcamHandler {
        fn handle_read(&self, _offset: u64, _size: u32) -> Result<u64> {
            // 0xFFFFFFFF = no PCI device present at this bus/device/function
            Ok(0xFFFFFFFF)
        }
        
        fn handle_write(&mut self, _offset: u64, _size: u32, _value: u64) -> Result<()> {
            // Ignore writes - no PCI devices
            Ok(())
        }
    }
    
    partition.register_mmio_region(
        PCI_MMCONFIG_START.0,
        PCI_MMCONFIG_SIZE,
        "PCI ECAM".to_string(),
        Some("pci_ecam".to_string()),
    )?;
    
    partition.register_mmio_handler(
        "pci_ecam".to_string(),
        Box::new(PciEcamHandler),
    );

    // Register virtio block device
    //eprintln!("\n=== Registering Virtio Block Device ===");
    // Use the layout-defined virtio MMIO address in the 32-bit reserved area
    // This is at 0xF8000000 (after PCI MMCONFIG), which is in reserved space
    // and accessible during early boot
    use crate::memory::layout::VIRTIO_MMIO_START;
    const VIRTIO_MMIO_BASE: u64 = VIRTIO_MMIO_START.0;
    const VIRTIO_MMIO_SIZE: u64 = 0x1000; // 4KB per device (standard virtio-mmio size)
    const VIRTIO_IRQ: u32 = 20; // IRQ line for virtio device
    
    // Create a disk image if it doesn't exist
    let disk_image_path = std::env::var("DISK_IMAGE").unwrap_or_else(|_| "kernels/alpine-virt.img".to_string());
    if !std::fs::metadata(&disk_image_path).is_ok() {
        //eprintln!("Creating disk image: {}", disk_image_path);
        // Create a 1GB disk image
        let file = std::fs::File::create(&disk_image_path)?;
        file.set_len(1024 * 1024 * 1024)?;
        //eprintln!("Disk image created: {} (1GB)", disk_image_path);
    }
    
    // Create virtio block device
    use crate::devices::virtio::block::{Block, CacheType, ImageType, SyncMode};
    let block_device = Arc::new(Mutex::new(Block::new(
        "vda1".to_string(),
        None, // partuuid
        CacheType::Writeback,
        disk_image_path.clone(),
        ImageType::Raw,
        false, // not read-only
        false, // not direct I/O
        SyncMode::Full,
    )?));
    
    // Create MMIO transport using the shared IOAPIC as the interrupt controller
    use crate::devices::virtio::mmio::MmioTransport;
    let mem_manager = partition.memory_manager().clone();
    let mut mmio_transport = MmioTransport::new(
        mem_manager,
        ioapic_irqchip.clone(),
        block_device.clone(),
    )?;
    
    // Set the IRQ line for the device
    mmio_transport.set_irq_line(VIRTIO_IRQ);
    
    // Create adapter to bridge MmioTransport to MmioHandler
    use crate::devices::virtio::mmio_adapter::MmioTransportAdapter;
    let mmio_adapter = MmioTransportAdapter::new(mmio_transport);
    
    // Register MMIO region
     partition.register_mmio_region(
        VIRTIO_MMIO_BASE,
        VIRTIO_MMIO_SIZE,
        "Virtio Block Device".to_string(),
        Some("virtio_block".to_string()),
    )?;
    
    // Register the handler
    partition.register_mmio_handler(
        "virtio_block".to_string(),
        Box::new(mmio_adapter),
    );
    
    // Register virtio device with DeviceManager for ACPI
    partition.device_manager_mut().register_virtio_mmio(
        "VBLK".to_string(), // Device name in ACPI
        1,                  // UID
        VIRTIO_MMIO_BASE,
        VIRTIO_MMIO_SIZE,
        VIRTIO_IRQ,
    );
    //eprintln!("Virtio block device registered at 0x{:X} with IRQ {} (ACPI: VBLK)", VIRTIO_MMIO_BASE, VIRTIO_IRQ);

    // Register virtio-rng device
    const VIRTIO_RNG_MMIO_BASE: u64 = VIRTIO_MMIO_BASE + VIRTIO_MMIO_SIZE;
    const VIRTIO_RNG_IRQ: u32 = 21;

    use crate::devices::virtio::rng::Rng;
    let rng_device = Arc::new(Mutex::new(Rng::new()));

    let mut rng_mmio_transport = MmioTransport::new(
        partition.memory_manager().clone(),
        ioapic_irqchip.clone(),
        rng_device.clone(),
    )?;
    rng_mmio_transport.set_irq_line(VIRTIO_RNG_IRQ);

    let rng_mmio_adapter = MmioTransportAdapter::new(rng_mmio_transport);

     partition.register_mmio_region(
        VIRTIO_RNG_MMIO_BASE,
        VIRTIO_MMIO_SIZE,
        "Virtio RNG Device".to_string(),
        Some("virtio_rng".to_string()),
    )?;

    partition.register_mmio_handler(
        "virtio_rng".to_string(),
        Box::new(rng_mmio_adapter),
    );

    partition.device_manager_mut().register_virtio_mmio(
        "VRNG".to_string(),
        2,
        VIRTIO_RNG_MMIO_BASE,
        VIRTIO_MMIO_SIZE,
        VIRTIO_RNG_IRQ,
    );
    
    let kernel_entry: u64;
    
   
    // Boot Linux kernel
    //eprintln!("\n=== Booting Linux Kernel ===");
    if !std::fs::exists(&kernel_path)? {
        return Err(anyhow::anyhow!("Kernel file not found"));
    }
    
    kernel_entry = partition.load_linux_kernel(&kernel_path, &initram_path, memory_size as u64)?;
    partition.setup_linux_registers(0, kernel_entry)?;
    //eprintln!("Linux kernel loaded and ready to boot!");
    
    
    //eprintln!("Starting VM...");

    std::thread::spawn(move || {
        let mut iteration = 0;
        
        // Debug: Verify RIP before first run
        
        match partition.verify_rip(0) {
            Ok(rip) => {
                //eprintln!("Pre-run RIP check: 0x{:X} (expected: 0x{:X})", rip, kernel_entry);
                if rip != kernel_entry {
                    //eprintln!("  ⚠️  WARNING: RIP mismatch before first run!");
                }
            }
            Err(e) => {
                //eprintln!("  ⚠️  Could not verify RIP: {:?}", e);
            }
        }
        
        
        let mut last_rip = 0u64;
        let mut rip_repeat_count = 0;
        
        loop {
            iteration += 1;
            
            // Run the VP and get exit context
            let exit_context = {
                match partition.run_vp(0) {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        //eprintln!("Error running VP: {:?}", e);
                        break;
                    }
                }
            };
            
            // Check if we're stuck in a loop at the same RIP
            let current_rip = exit_context.VpContext.Rip;
            if current_rip == last_rip {
                rip_repeat_count += 1;
                if rip_repeat_count > 1000 && iteration % 1000 == 0 {
                    //eprintln!("⚠️  Stuck at RIP 0x{:X} for {} iterations (total: {})", 
                        //current_rip, rip_repeat_count, iteration);
                }
            } else {
                if rip_repeat_count > 100 {
                    //eprintln!("✓ Moved from stuck RIP 0x{:X} to 0x{:X} after {} iterations", 
                        //last_rip, current_rip, rip_repeat_count);
                }
                last_rip = current_rip;
                rip_repeat_count = 0;
            }
            
            // Handle the exit - wrap in a match to catch any panics
            let should_continue = {
                match partition.handle_exit(0, &exit_context) {
                    Ok(cont) => cont,
                    Err(e) => {
                        //eprintln!("Error handling exit: {:?}", e);
                        false
                    }
                }
            };
            
            if !should_continue {
                //eprintln!("VM execution stopped after {:?} iterations", iteration);
                break;
            }
            
            // Log progress every 100k iterations
            if iteration % 100000 == 0 {
                //eprintln!("VM running: {} iterations, current RIP: 0x{:X}", iteration, current_rip);
            }
        }
    });

    rt.block_on(async move {
        //eprintln!("Tokio runtime started on Windows IOCP...");
        
        // Spawn your Virtio-Net, Virtio-Block, etc.
        // tokio::spawn(virtio_block_handler());
        //Ok(())
        tokio::signal::ctrl_c().await.unwrap();
    });

    //eprintln!("VM partition deleted");

    Ok(())
}