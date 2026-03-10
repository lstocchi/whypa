//! VM configuration parsed from environment variables.

use anyhow::{bail, Result};
use tracing::info;

/// All user-configurable settings needed to start the VM.
pub struct VmConfig {
    pub kernel_path: String,
    pub initram_path: String,
    pub rootfs_path: String,
    pub memory_size: usize,
}

impl VmConfig {
    /// Build a [`VmConfig`] from environment variables (with sensible defaults)
    /// and validate that the referenced files exist.
    pub fn from_env() -> Result<Self> {
        let kernel_path = std::env::var("KERNEL_PATH")
            .unwrap_or_else(|_| "kernels/fedora-kernel".to_string());
        let initram_path = std::env::var("INITRAM_PATH")
            .unwrap_or_else(|_| "kernels/initramfs.img".to_string());
        let rootfs_path = std::env::var("ROOTFS_PATH")
            .unwrap_or_else(|_| "kernels/fedora-rootfs.img".to_string());

        let memory_gb: usize = std::env::var("MEMORY_GB")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        let memory_size = memory_gb * 1024 * 1024 * 1024;

        require_file(&kernel_path, "Kernel")?;
        require_file(&initram_path, "Initramfs")?;
        require_file(&rootfs_path, "Rootfs")?;

        info!(kernel = %kernel_path, initram = %initram_path,
              rootfs = %rootfs_path, memory_gb, "VM configuration loaded");

        Ok(Self { kernel_path, initram_path, rootfs_path, memory_size })
    }
}

fn require_file(path: &str, label: &str) -> Result<()> {
    if !std::fs::exists(path)? {
        bail!("{label} file not found: {path}");
    }
    Ok(())
}
