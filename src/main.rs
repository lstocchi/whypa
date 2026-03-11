#[cfg(not(target_os = "windows"))]
compile_error!("This project only supports Windows.");

mod acpi;
mod bootparam;
mod byte_order;
mod config;
mod cpu;
mod device_manager;
mod devices;
mod emulator;
mod linux_boot;
mod memory;
mod partition;
mod terminal;
mod vm;

use anyhow::Context;
use memory::memory::MemoryPerms;
use tracing::{debug, info};
use tracing_subscriber::filter::LevelFilter;

use config::VmConfig;
use partition::Partition;
use terminal::HostConsole;

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
    debug!("Starting VM...");

    let cfg = VmConfig::from_env()?;

    // Create and configure the partition.
    info!(memory_size_gb = cfg.memory_size / (1024 * 1024 * 1024), "Creating partition");
    let mut partition = Partition::new(cfg.memory_size)
        .context("Failed to create partition")?;

    partition.configure(1)?; // single vCPU for now
    partition.setup()?;
    partition.create_vp(0)?;
    partition.allocate_memory_with_size(cfg.memory_size as u64, MemoryPerms::RWX)?;
    debug!("Partition configured and memory allocated");

    // Enter raw mode, register the Ctrl+C handler, and start the stdin reader.
    let console = HostConsole::enter_raw_mode();
    let (input_buffer, input_event) = console.spawn_stdin_reader();

    // Register all devices (IOAPIC, PCI ECAM, virtio block/rng/console).
    device_manager::register_devices(
        &mut partition,
        &cfg.rootfs_path,
        input_buffer,
        input_event,
    )?;

    // Load and boot the Linux kernel.
    info!(kernel = %cfg.kernel_path, initram = %cfg.initram_path, "Loading Linux kernel");
    let kernel_entry = partition.load_linux_kernel(
        &cfg.kernel_path,
        &cfg.initram_path,
    )?;
    partition.setup_linux_registers(0, kernel_entry)?;
    info!(entry = format_args!("0x{:X}", kernel_entry), "Kernel loaded, starting VM");

    // Run the vCPU loop on the main thread until shutdown.
    vm::run(&mut partition, kernel_entry, console.running());

    Ok(())
}
