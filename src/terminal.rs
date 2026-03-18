//! Host terminal handling – raw mode, VT output, stdin reader, and escape key.

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
    ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT,
    ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING,
};

use crate::devices::event::WindowsEvent;
use crate::partition::Partition;

/// The escape byte used to quit the VM (Ctrl+], 0x1D – same as telnet).
const ESCAPE_KEY: u8 = 0x1D;

/// Console ctrl handler called by Windows on Ctrl+Break or window close.
unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
    // Any control event → request graceful shutdown.
    info!("Console control event received, shutting down");
    signal_shutdown();
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
    /// - **stdin**: disable echo, line-editing, **and** processed-input so
    ///   Ctrl+C is delivered as the raw byte `0x03` (forwarded to the guest)
    ///   instead of generating `CTRL_C_EVENT`.
    ///   Enable virtual-terminal input so we get escape sequences as-is.
    /// - **stdout**: enable VT processing so ANSI escape sequences from the
    ///   guest render correctly on the Windows console.
    /// - Registers a console ctrl handler so Ctrl+Break / window close still
    ///   trigger a graceful shutdown.
    ///
    /// To exit the VM press **Ctrl+]** (the telnet escape key).
    pub fn enter_raw_mode() -> Self {
        let saved_stdin_mode = unsafe { Self::setup_stdin() };
        unsafe {
            Self::setup_stdout();
            let _ = SetConsoleCtrlHandler(Some(ctrl_handler), true);
        }
        debug!("Host console switched to raw mode");

        Self { saved_stdin_mode }
    }

    /// Spawn a background thread that reads from the host stdin, strips CPR
    /// responses, detects the escape key (Ctrl+]), and pushes filtered bytes
    /// into a shared buffer.
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
                            let bytes = &raw[..n];

                            // Check for Ctrl+] (0x1D) — the VM escape key.
                            if let Some(pos) = bytes.iter().position(|&b| b == ESCAPE_KEY) {
                                // Forward any bytes that arrived before the escape key.
                                if pos > 0 {
                                    strip_response_and_signal(&bytes[..pos], &thread_buf, &thread_evt);
                                }
                                info!("Ctrl+] received, shutting down VM");
                                signal_shutdown();
                                break;
                            }

                            strip_response_and_signal(bytes, &thread_buf, &thread_evt);
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
        // Clear ENABLE_PROCESSED_INPUT so Ctrl+C is delivered as the raw byte
        // 0x03 and forwarded to the guest rather than generating CTRL_C_EVENT.
        // https://learn.microsoft.com/en-us/windows/console/setconsolemode
        let raw = (mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
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
        // Remove our handler (restore default Ctrl+C / Ctrl+Break behaviour).
        unsafe { let _ = SetConsoleCtrlHandler(Some(ctrl_handler), false); }

        if let Some((h, saved)) = self.saved_stdin_mode {
            unsafe { let _ = SetConsoleMode(h, saved); }
            debug!("Host console mode restored");
        }
    }
}

fn signal_shutdown() {
    Partition::cancel_vp();
}

fn strip_response_and_signal(bytes: &[u8], thread_buf: &Arc<Mutex<VecDeque<u8>>>, thread_evt: &Arc<WindowsEvent>) {
    let filtered = strip_cpr_responses(bytes);
    if !filtered.is_empty() {
        thread_buf.lock().unwrap().extend(&filtered);
        thread_evt.signal();
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
