//! Host terminal handling – raw mode, VT output, stdin reader, and Ctrl+C.

use std::collections::VecDeque;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{debug, error, info};
use windows::Win32::Foundation::HANDLE;
use windows::core::BOOL;
use windows::Win32::System::Console::{
    GetConsoleMode, SetConsoleMode, GetStdHandle, SetConsoleCtrlHandler,
    STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
    ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING,
};

use crate::devices::event::WindowsEvent;

/// Global flag set by the console ctrl handler.  The vCPU loop polls this.
static RUNNING: AtomicBool = AtomicBool::new(true);

/// Console ctrl handler called by Windows on Ctrl+C, Ctrl+Break, or window close.
unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
    // Any control event → request graceful shutdown.
    info!("Ctrl+C received, shutting down");
    RUNNING.store(false, Ordering::Relaxed);
    BOOL(1) // TRUE = we handled it, don't terminate the process
}

/// RAII guard that puts the Windows console into raw mode on creation and
/// restores the original mode when dropped.
pub struct HostConsole {
    saved_stdin_mode: Option<(HANDLE, CONSOLE_MODE)>,
}

impl HostConsole {
    /// Switch the host console into raw mode suitable for guest I/O:
    ///
    /// - **stdin**: disable echo and line-editing; keep processed-input so
    ///   Ctrl+C still generates `CTRL_C_EVENT`.
    ///   Enable virtual-terminal input so we get escape sequences as-is.
    /// - **stdout**: enable VT processing so ANSI escape sequences from the
    ///   guest render correctly on the Windows console.
    /// - Registers a console ctrl handler so Ctrl+C sets the [`running()`]
    ///   flag to `false` instead of killing the process.
    pub fn enter_raw_mode() -> Self {
        let saved_stdin_mode = unsafe { Self::setup_stdin() };
        unsafe {
            Self::setup_stdout();
            let _ = SetConsoleCtrlHandler(Some(ctrl_handler), true);
        }
        debug!("Host console switched to raw mode");

        Self { saved_stdin_mode }
    }

    /// Returns a reference to the global shutdown flag.
    ///
    /// The vCPU loop should poll this with [`AtomicBool::load`] to detect
    /// Ctrl+C.
    pub fn running(&self) -> &'static AtomicBool {
        &RUNNING
    }

    /// Spawn a background thread that reads from the host stdin, strips CPR
    /// responses, and pushes filtered bytes into a shared buffer.
    ///
    /// Returns the `(buffer, event)` pair that should be handed to the virtio
    /// console device.
    pub fn spawn_stdin_reader(&self) -> (Arc<Mutex<VecDeque<u8>>>, Arc<WindowsEvent>) {
        let buffer: Arc<Mutex<VecDeque<u8>>> = Arc::new(Mutex::new(VecDeque::new()));
        let event = Arc::new(WindowsEvent::new().expect("create stdin WindowsEvent"));

        let thread_buf = Arc::clone(&buffer);
        let thread_evt = Arc::clone(&event);

        std::thread::Builder::new()
            .name("stdin-reader".into())
            .spawn(move || {
                let stdin = std::io::stdin();
                let mut handle = stdin.lock();
                let mut raw = [0u8; 256];
                loop {
                    match handle.read(&mut raw) {
                        Ok(0) => {
                            debug!("stdin EOF");
                            break;
                        }
                        Ok(n) => {
                            let filtered = strip_cpr_responses(&raw[..n]);
                            if !filtered.is_empty() {
                                thread_buf.lock().unwrap().extend(&filtered);
                                thread_evt.signal();
                            }
                        }
                        Err(e) => {
                            error!(error = %e, "Error reading stdin");
                            break;
                        }
                    }
                }
            })
            .expect("spawn stdin-reader thread");

        (buffer, event)
    }

    // -- private helpers ----------------------------------------------------

    unsafe fn setup_stdin() -> Option<(HANDLE, CONSOLE_MODE)> {
        let h = GetStdHandle(STD_INPUT_HANDLE).ok()?;
        let mut mode = CONSOLE_MODE::default();
        GetConsoleMode(h, &mut mode).ok()?;

        let saved = mode;
        // Keep ENABLE_PROCESSED_INPUT so Ctrl+C still generates CTRL_C_EVENT
        // (caught by our SetConsoleCtrlHandler callback).
        let raw = (mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT))
            | ENABLE_VIRTUAL_TERMINAL_INPUT;
        let _ = SetConsoleMode(h, raw);

        Some((h, saved))
    }

    unsafe fn setup_stdout() {
        if let Some(h) = GetStdHandle(STD_OUTPUT_HANDLE).ok() {
            let mut mode = CONSOLE_MODE::default();
            if GetConsoleMode(h, &mut mode).is_ok() {
                let _ = SetConsoleMode(
                    h,
                    mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                );
            }
        }
    }
}

impl Drop for HostConsole {
    fn drop(&mut self) {
        // Remove our handler (restore default Ctrl+C behaviour).
        unsafe { let _ = SetConsoleCtrlHandler(Some(ctrl_handler), false); }

        if let Some((h, saved)) = self.saved_stdin_mode {
            unsafe { let _ = SetConsoleMode(h, saved); }
            debug!("Host console mode restored");
        }
    }
}

/// Strip Cursor Position Report responses (`ESC[row;colR`) from raw bytes.
///
/// The host terminal generates these in response to DSR queries (`ESC[6n`)
/// that the guest shell sends.  If they leak into the guest's stdin they
/// appear as garbage text like `^[[30;5R`.
fn strip_cpr_responses(bytes: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 2 < bytes.len() && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j > i + 2 && j < bytes.len() && bytes[j] == b'R' {
                i = j + 1;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    result
}
