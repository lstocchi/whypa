//! gvproxy backend — spawns gvproxy and connects via stdio. It gets killed on drop.
//! 
//! gvproxy (from [gvisor-tap-vsock]) is a user-mode networking stack that
//! provides DHCP, DNS, NAT, and port forwarding for the guest VM. The
//! protocol used is 2-byte LE length-prefixed ethernet frames.
//! [gvisor-tap-vsock]: https://github.com/containers/gvisor-tap-vsock

use which::which;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use tracing::{debug, info};

pub struct GvProxy {
    child: Child,
    pid_file: PathBuf,
    log_file: PathBuf,
}

impl GvProxy {
    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    /// Spawn a gvproxy process listening on stdio.
    ///
    /// Looks for `gvproxy` next to the current executable first, then falls back to `PATH`.
    pub fn spawn() -> io::Result<Self> {
        let gvproxy = find_gvproxy().ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "gvproxy not found"))?;
        let gvproxy_path = gvproxy.to_string_lossy().into_owned();

        let pid_file = std::env::temp_dir().join(format!("gvproxy-{}.pid", std::process::id()));
        let log_file = std::env::temp_dir().join(format!("gvproxy-{}.log", std::process::id()));

        let child = Command::new(gvproxy)
            .arg("-listen-stdio")
            .arg("accept")
            .arg("-pid-file")
            .arg(&pid_file)
            .arg("-log-file")
            .arg(&log_file)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("failed to spawn gvproxy {}: {e}", gvproxy_path),
                )
            })?;

        // Wait for gvproxy to signal readiness via its PID file, or crash.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if pid_file.exists() {
                break; // gvproxy is initialized and ready
            }
            std::thread::sleep(Duration::from_millis(20));
        }

        if !pid_file.exists() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "gvproxy did not become ready within 5 s",
            ));
        }

        info!("gvproxy started (pid {}), log: {}", child.id(), log_file.display());
        Ok(Self { child, pid_file, log_file })
    }

}

impl Drop for GvProxy {
    fn drop(&mut self) {
        debug!("shutting down gvproxy (pid {})", self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.pid_file);
    }
}

/// Find the gvproxy binary — first next to the current executable, then PATH.
fn find_gvproxy() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe
            .parent()
            .unwrap_or(Path::new("."))
            .join("gvproxy.exe");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    if let Ok(path) = which("gvproxy") {
        return Some(path);
    }
    None
}
