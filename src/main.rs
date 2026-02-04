#[cfg(not(target_os = "windows"))]
compile_error!("This project only supports Windows.");

mod partition;
mod memory;

use partition::Partition;
use tracing_subscriber::filter::LevelFilter;
use anyhow::Context;

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

fn main() -> anyhow::Result<()> {
    install_tracing();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Init tokio runtime")?;

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(100);

    rt.spawn(async move {
        while let Some(request) = rx.recv().await {
            match request.as_str() {
                "quit" => {
                    break;
                }
                _ => {
                    println!("Unknown request: {}", request);
                }
            }
        }
    });

    let mut partition = Partition::new().context("Failed to create partition")?;

    partition.configure(1)?;
    partition.setup()?;
    partition.create_vp(0)?;

    // The compiled bytes for the assembly above
    // Using 32-bit addressing mode which should work in 64-bit mode
    let guest_code: [u8; 14] = [
        0xB8, 0x21, 0x00, 0x00, 0x00,              // mov eax, 33
        0x67, 0x89, 0x04, 0x25, 0x00, 0x20, 0x00, 0x00,  // mov [0x2000], eax  (address-size override prefix 0x67)
        0xF4                                        // hlt
    ];

    partition.allocate_memory()?;
        
    // Write code first, then set up registers to point to it
    partition.write_code(&guest_code, 0x0000)?;
    println!("Setting up registers...");
    partition.setup_registers(0, 0x0000)?;  // Set RIP to where the code actually is
    println!("Registers set up, starting VM...");

    std::thread::spawn(move || {
        let mut iteration = 0;
        loop {
            iteration += 1;
            println!("Running VP (iteration {})...", iteration);

             // Run the VP and get exit context
            let exit_context = {
                match partition.run_vp(0) {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        println!("Error running VP: {}", e);
                        break;
                    }
                }
            };
        
            println!("VP exited, handling exit...");
            
            // Handle the exit - wrap in a match to catch any panics
            let should_continue = {
                match partition.handle_exit(0, &exit_context) {
                    Ok(cont) => cont,
                    Err(e) => {
                        println!("Error handling exit: {}", e);
                        false
                    }
                }
            };
            
            if !should_continue {
                println!("VM execution stopped after {} iterations", iteration);
                break;
            }
        }
    });

    rt.block_on(async move {
        println!("Tokio runtime started on Windows IOCP...");
        
        // Spawn your Virtio-Net, Virtio-Block, etc.
        // tokio::spawn(virtio_block_handler());
        //Ok(())
        tokio::signal::ctrl_c().await.unwrap();
    });

    println!("VM partition deleted");

    Ok(())
}