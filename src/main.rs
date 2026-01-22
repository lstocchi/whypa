#[cfg(not(target_os = "windows"))]
compile_error!("This project only supports Windows.");

mod partition;
mod virtio;

use partition::Partition;
use std::result::Result;
use std::env;

fn main() -> Result<(), String> {
    // Check command line arguments
    let args: Vec<String> = env::args().collect();
    let disk_image_path = if args.len() > 1 {
        Some(args[1].clone())
    } else {
        None
    };
    
    // UEFI firmware path - can be provided as second argument or use default
    let uefi_firmware_path = if args.len() > 2 {
        Some(args[2].clone())
    } else {
        // Default to common OVMF firmware names
        if std::path::Path::new("OVMF.fd").exists() {
            Some("OVMF.fd".to_string())
        } else if std::path::Path::new("OVMF_CODE.fd").exists() {
            Some("OVMF_CODE.fd".to_string())
        } else {
            None
        }
    };

    println!("Creating VM partition...");

    let mut partition = Partition::new()?;
    
    // Configure for OS boot - use more memory (2GB default)
    partition.configure(1)?;
    partition.setup()?;
    partition.create_vp(0)?;
    
    // Allocate memory for the VM
    partition.allocate_memory()?;
    println!("Allocated {} MB of memory", partition.get_memory_size() / (1024 * 1024));

    if let Some(image_path) = disk_image_path {
        println!("\n=== Booting from disk image ===");
        
        // Load UEFI firmware if provided
        if let Some(ref firmware_path) = uefi_firmware_path {
            println!("\n=== Loading UEFI Firmware ===");
            partition.load_uefi_firmware(firmware_path)?;
        } else {
            println!("\nWarning: No UEFI firmware provided.");
            println!("To boot Fedora, you need UEFI firmware (e.g., OVMF.fd).");
            println!("Usage: {} <disk_image> [uefi_firmware]", args[0]);
            println!("Or place OVMF.fd or OVMF_CODE.fd in the current directory.");
        }
        
        // Load the disk image
        partition.load_disk_image(&image_path)?;
        
        // Map the disk image to guest physical address space
        partition.map_disk_image()?;
        
        // Map VirtIO MMIO region
        partition.map_virtio_mmio()?;
        
        // UEFI firmware entry point is typically at 0x100000 (1MB) for 64-bit systems
        // This is where OVMF expects to start execution
        let boot_address = 0x100000;
        
        println!("\n=== Setting up virtual processor for UEFI boot ===");
        println!("Boot address: 0x{:X}", boot_address);
        partition.setup_registers_for_boot(0, boot_address)?;
        
        println!("\n=== Starting VM execution ===");
        if uefi_firmware_path.is_some() {
            println!("UEFI firmware loaded - attempting to boot Fedora...");
        } else {
            println!("Warning: No UEFI firmware loaded. Boot may fail.");
        }
        println!("Note: For a full OS boot, you still need:");
        println!("  - Disk controller emulation (AHCI/IDE)");
        println!("\nRunning VM with exit handling loop...");
        
        // Main VM execution loop
        let mut exit_count = 0u64;
        loop {
            exit_count += 1;
            
            // Run the VM - it will exit on various reasons (I/O, memory access, etc.)
            let exit_context = partition.run_vp(0)?;
            
            // Handle the exit and determine if we should continue
            let should_continue = partition.handle_exit(0, &exit_context)?;
            
            if !should_continue {
                println!("\nVM execution stopped (exit #{})", exit_count);
                break;
            }
            
            // Continue execution - the loop will run the VM again
            if exit_count % 1000 == 0 {
                println!("VM still running... ({} exits handled)", exit_count);
            }
        }
        
        println!("\nTotal exits handled: {}", exit_count);
    } else {
        println!("\n=== Running simple test code ===");
        println!("Usage: {} <path_to_disk_image.raw>", args[0]);
        println!("Running simple infinite loop test instead...\n");
        
        // Write a simple infinite loop: jmp $ (0xEB 0xFE)
        let code = vec![0xEB, 0xFE];
        partition.write_code(&code, 0)?;

        println!("Setting up virtual processor registers...");
        partition.setup_registers(0, 0)?;

        println!("Running virtual processor...");
        let exit_context = partition.run_vp(0)?;

        println!("VM exited");
        println!("Exit reason: {}", exit_context.ExitReason.0);
    }

    partition.delete()?;
    println!("VM partition deleted");
    
    Ok(())
}
