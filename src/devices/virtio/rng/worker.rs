use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::thread;

use tracing::error;
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::Security::Cryptography::{BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG};
use windows::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};

use crate::devices::event::WindowsEvent;
use crate::devices::virtio::descriptor_utils::Writer;
use crate::devices::virtio::device::DeviceQueue;
use crate::devices::virtio::mmio::InterruptTransport;
use crate::memory::memory::MemoryManager;

pub struct RngWorker {
    device_queue: DeviceQueue,
    interrupt: InterruptTransport,
    mem: MemoryManager,
    stop_event: Arc<WindowsEvent>,
}

impl RngWorker {
    pub fn new(
        device_queue: DeviceQueue,
        interrupt: InterruptTransport,
        mem: MemoryManager,
        stop_event: Arc<WindowsEvent>,
    ) -> Self {
        Self { device_queue, interrupt, mem, stop_event }
    }

    pub fn run(self) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("rng worker".into())
            .spawn(|| self.work())
            .unwrap()
    }

    pub fn work(mut self) {
        let handles: [HANDLE; 2] = [
            HANDLE(self.device_queue.event.as_raw_handle()), 
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
                // WAIT_OBJECT_0 is the first handle in the array
                r if r == WAIT_OBJECT_0 => {
                    self.process_virtio_queues();
                }
                // WAIT_OBJECT_0 + 1 is the second handle (stop_fd)
                r if r.0 == WAIT_OBJECT_0.0 + 1 => {
                    tracing::debug!("stopping worker thread");
                    // No need to "read" the event; Auto-Reset took care of it.
                    return;
                }
                // Error handling
                _ if result == WAIT_FAILED => {
                    let err = std::io::Error::last_os_error();
                    tracing::error!("Worker loop wait failed: {}", err);
                    break;
                }
                _ => {
                    tracing::warn!("Unexpected wait result: {:?}", result);
                }
            }
        }
    }

    /// Process device virtio queue(s).
    fn process_virtio_queues(&mut self) {
        let mem = self.mem.clone();
        loop {
            self.device_queue.queue.disable_notification(&mem).unwrap();

            self.process_queue();

            if !self.device_queue.queue.enable_notification(&mem).unwrap() {
                break;
            }
        }
    }

    pub fn stop_worker(&self) {
        self.stop_event.signal();
    }

    fn process_queue(&mut self) {
        let mem = &self.mem;
        while let Some(head) = self.device_queue.queue.pop(mem) {
            let mut writer = match Writer::new(mem, head.clone()) {
                Ok(w) => w,
                Err(e) => {
                    error!("virtio_rng: invalid descriptor chain: {e:?}");
                    continue;
                }
            };

            let len = writer.available_bytes();
            if len == 0 {
                if let Err(e) = self.device_queue.queue.add_used(mem, head.index, 0) {
                    error!("virtio_rng: failed to add used: {e:?}");
                }
                continue;
            }

            let mut buf = vec![0u8; len];
            let result = unsafe {
                BCryptGenRandom(None, &mut buf, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
            };

            let written = if result.is_ok() {
                match writer.write_all(&buf) {
                    Ok(()) => len,
                    Err(e) => {
                        error!("virtio_rng: failed to write random bytes: {e:?}");
                        0
                    }
                }
            } else {
                error!("virtio_rng: BCryptGenRandom failed: {:?}", result);
                0
            };

            if let Err(e) = self.device_queue.queue.add_used(mem, head.index, written as u32) {
                error!("virtio_rng: failed to add used: {e:?}");
            }

            if self.device_queue.queue.needs_notification(mem).unwrap() {
                if let Err(e) = self.interrupt.try_signal_used_queue() {
                    error!("virtio_rng: failed to signal queue: {e:?}");
                }
            }
        }
    }

}