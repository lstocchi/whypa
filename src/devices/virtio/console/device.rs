use std::cmp;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use tracing::error;
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;
use vm_memory::ByteValued;

use crate::devices::event::WindowsEvent;
use crate::devices::virtio::console::{NUM_QUEUES, QUEUE_CONFIG};
use crate::devices::virtio::console::worker::ConsoleWorker;
use crate::devices::virtio::device::{DeviceQueue, DeviceState, QueueConfig, VirtioDevice};
use crate::devices::virtio::mmio::InterruptTransport;
use crate::devices::virtio::{ActivateError, ActivateResult, TYPE_CONSOLE};
use crate::memory::memory::MemoryManager;

const VIRTIO_CONSOLE_F_SIZE: u32 = 0;
const VIRTIO_CONSOLE_F_MULTIPORT: u32 = 1;
const VIRTIO_CONSOLE_F_EMERG_WRITE: u32 = 2;

/// Device configuration layout per VirtIO spec §5.3.4:
///
/// ```c
/// struct virtio_console_config {
///     le16 cols;
///     le16 rows;
///     le32 max_nr_ports;
///     le32 emerg_wr;
/// };
/// ```
#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct ConsoleConfig {
    cols: u16,
    rows: u16,
    max_nr_ports: u32,
    emerg_wr: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for ConsoleConfig {}

pub struct Console {
    avail_features: u64,
    acked_features: u64,
    device_state: DeviceState,
    worker_thread: Option<JoinHandle<()>>,
    worker_stop_event: Arc<WindowsEvent>,
    worker_input_buffer: Arc<Mutex<VecDeque<u8>>>,
    worker_input_event: Arc<WindowsEvent>,
    config: ConsoleConfig,
}

impl Console {
    pub fn new(cols: u16, rows: u16, worker_input_buffer: Arc<Mutex<VecDeque<u8>>>, worker_input_event: Arc<WindowsEvent>) -> Self {
        let config = ConsoleConfig {
            cols,
            rows,
            max_nr_ports: 0, // not used — MULTIPORT feature not enabled
            emerg_wr: 0,     // not used — EMERG_WRITE feature not enabled
        };

        Self {
            avail_features: 1u64 << VIRTIO_F_VERSION_1
                | 1u64 << VIRTIO_CONSOLE_F_SIZE,
            acked_features: 0,
            device_state: DeviceState::Inactive,
            worker_thread: None,
            worker_stop_event: Arc::new(WindowsEvent::new().unwrap()),
            worker_input_buffer,
            worker_input_event,
            config,
        }
    }
}

impl VirtioDevice for Console {
    fn device_type(&self) -> u32 {
        TYPE_CONSOLE
    }

    fn device_name(&self) -> &str {
        "console"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &QUEUE_CONFIG
    }

    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features;
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // Per §5.3.5.1: The device MUST allow a write to emerg_wr, even on
        // an unconfigured device. The device SHOULD transmit the lower byte
        // written to emerg_wr to an appropriate log or output method.
        // cols(2) + rows(2) + max_nr_ports(4) = offset 8 for emerg_wr
        let emerg_wr_offset = 8u64;
        if offset == emerg_wr_offset && data.len() >= 1 {
            // Emergency write: output the lower byte
            let ch = data[0];
            eprint!("{}", ch as char);
            return;
        }
        error!("Guest attempted to write config at offset {offset}");
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn activate(
        &mut self,
        mem: MemoryManager,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if self.worker_thread.is_some() {
            panic!("virtio_console: worker thread already exists");
        }

        let [rx_q, tx_q]: [DeviceQueue; NUM_QUEUES] = queues.try_into().map_err(|_| {
            error!("Cannot perform activate. Expected {NUM_QUEUES} queue(s)");
            ActivateError::BadActivate
        })?;

        let worker = ConsoleWorker::new(
            rx_q,
            tx_q,
            interrupt.clone(),
            mem.clone(),
            Arc::clone(&self.worker_stop_event),
            Arc::clone(&self.worker_input_buffer),
            Arc::clone(&self.worker_input_event),
        );
        self.worker_thread = Some(worker.run());
        self.device_state = DeviceState::Activated(mem, interrupt);
        Ok(())
    }

    fn reset(&mut self) -> bool {
        if let Some(worker) = self.worker_thread.take() {
            let _ = self.worker_stop_event.signal();
            if let Err(e) = worker.join() {
                error!("error waiting for console worker thread: {e:?}");
            }
        }
        self.device_state = DeviceState::Inactive;
        true
    }
}
