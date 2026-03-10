use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::sync::{Arc, Mutex};
use std::thread;

use tracing::error;
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};

use crate::devices::event::WindowsEvent;
use crate::devices::virtio::descriptor_utils::{Reader, Writer};
use crate::devices::virtio::device::DeviceQueue;
use crate::devices::virtio::mmio::InterruptTransport;
use crate::memory::memory::MemoryManager;

pub struct ConsoleWorker {
    /// receiveq(port0) — device writes input to the guest through these buffers
    rx_queue: DeviceQueue,
    /// transmitq(port0) — guest writes output through these buffers; we read & emit
    tx_queue: DeviceQueue,
    interrupt: InterruptTransport,
    mem: MemoryManager,
    stop_event: Arc<WindowsEvent>,

    input_buffer: Arc<Mutex<VecDeque<u8>>>,
    input_event: Arc<WindowsEvent>,
}

impl ConsoleWorker {
    pub fn new(
        rx_queue: DeviceQueue,
        tx_queue: DeviceQueue,
        interrupt: InterruptTransport,
        mem: MemoryManager,
        stop_event: Arc<WindowsEvent>,
        input_buffer: Arc<Mutex<VecDeque<u8>>>,
        input_event: Arc<WindowsEvent>,
    ) -> Self {
        Self {
            rx_queue,
            tx_queue,
            interrupt,
            mem,
            stop_event,
            input_buffer,
            input_event,
        }
    }

    pub fn run(self) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("console worker".into())
            .spawn(|| self.work())
            .unwrap()
    }

    fn work(mut self) {
        let handles: [HANDLE; 3] = [
            HANDLE(self.tx_queue.event.as_raw_handle()),
            HANDLE(self.input_event.as_raw_handle()),
            HANDLE(self.stop_event.as_raw_handle()),
        ];

        loop {
            let result = unsafe {
                WaitForMultipleObjects(
                    &handles,
                    false,    // bWaitAll: false (wake on any)
                    INFINITE, // No timeout
                )
            };

            match result {
                // WAIT_OBJECT_0 → tx queue event fired (guest wrote output)
                r if r == WAIT_OBJECT_0 => {
                    self.process_tx_virtio_queue();
                }
                // WAIT_OBJECT_0 + 1 → input event (host has data for the guest)
                r if r.0 == WAIT_OBJECT_0.0 + 1 => {
                    self.process_rx_input();
                }
                // WAIT_OBJECT_0 + 2 → stop event
                r if r.0 == WAIT_OBJECT_0.0 + 2 => {
                    tracing::debug!("console: stopping worker thread");
                    return;
                }
                _ if result == WAIT_FAILED => {
                    let err = io::Error::last_os_error();
                    error!("console: worker loop wait failed: {err}");
                    break;
                }
                _ => {
                    tracing::warn!("console: unexpected wait result: {result:?}");
                }
            }
        }
    }

    // ── transmitq processing ─────────────────────────────────────────

    /// Drain the transmit queue with notification suppression.
    fn process_tx_virtio_queue(&mut self) {
        let mem = self.mem.clone();
        loop {
            self.tx_queue.queue.disable_notification(&mem).unwrap();
            self.process_tx_queue(&mem);
            if !self.tx_queue.queue.enable_notification(&mem).unwrap() {
                break;
            }
        }
    }

    /// Process every pending descriptor chain on the transmit queue.
    ///
    /// Each chain contains the bytes the guest wants to output.  Per §5.3.6:
    /// "a buffer containing the characters is placed in the port's transmitq".
    fn process_tx_queue(&mut self, mem: &MemoryManager) {
        let mut used_any = false;

        while let Some(head) = self.tx_queue.queue.pop(mem) {
            let head_index = head.index;

            let mut reader = match Reader::new(mem, head) {
                Ok(r) => r,
                Err(e) => {
                    error!("virtio_console tx: invalid descriptor chain: {e:?}");
                    continue;
                }
            };

            let avail = reader.available_bytes();
            if avail > 0 {
                let mut buf = vec![0u8; avail];
                match reader.read(&mut buf) {
                    Ok(n) => {
                        // Write guest output to host stdout.
                        let _ = io::stdout().write_all(&buf[..n]);
                        let _ = io::stdout().flush();
                    }
                    Err(e) => {
                        error!("virtio_console tx: failed to read from descriptor: {e:?}");
                    }
                }
            }

            // Return the descriptor to the guest via the used ring.
            // len = 0 because the device doesn't write anything back for tx.
            if let Err(e) = self.tx_queue.queue.add_used(mem, head_index, 0) {
                error!("virtio_console tx: failed to add used: {e:?}");
            }
            used_any = true;
        }

        // Always signal the interrupt for console TX completions — unlike
        // block/rng where batching is fine, the guest's shell is blocked
        // waiting for each write to complete so we must not suppress the
        // notification via `needs_notification()` / EVENT_IDX.
        if used_any {
            if let Err(e) = self.interrupt.try_signal_used_queue() {
                error!("virtio_console tx: failed to signal queue: {e:?}");
            }
        }
    }

    // ── receiveq processing (input from host → guest) ──────────────

    /// Drain the shared input buffer into the guest's receiveq.
    fn process_rx_input(&mut self) {
        let data: Vec<u8> = {
            let mut buf = self.input_buffer.lock().unwrap();
            buf.drain(..).collect()
        };

        if data.is_empty() {
            return;
        }

        let mem = &self.mem;
        let mut offset = 0;
        let mut used_any = false;

        while offset < data.len() {
            let Some(head) = self.rx_queue.queue.pop(mem) else {
                tracing::warn!("virtio_console rx: no buffers available, dropping {} bytes", data.len() - offset);
                break;
            };
            let head_index = head.index;

            let mut writer = match Writer::new(mem, head) {
                Ok(w) => w,
                Err(e) => {
                    error!("virtio_console rx: invalid descriptor chain: {e:?}");
                    continue;
                }
            };

            let chunk = &data[offset..];
            let to_write = chunk.len().min(writer.available_bytes());
            match writer.write_all(&chunk[..to_write]) {
                Ok(()) => {
                    offset += to_write;
                }
                Err(e) => {
                    error!("virtio_console rx: failed to write to descriptor: {e:?}");
                }
            }

            if let Err(e) = self.rx_queue.queue.add_used(mem, head_index, to_write as u32) {
                error!("virtio_console rx: failed to add used: {e:?}");
            }
            used_any = true;
        }

        if used_any && self.rx_queue.queue.needs_notification(mem).unwrap() {
            if let Err(e) = self.interrupt.try_signal_used_queue() {
                error!("virtio_console rx: failed to signal queue: {e:?}");
            }
        }
    }
}
