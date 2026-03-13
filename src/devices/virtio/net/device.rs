use std::cmp;
use std::io::{self, Write};
use std::sync::Arc;
use std::thread::JoinHandle;

use tracing::{error, warn};
use virtio_bindings::{virtio_config::VIRTIO_F_VERSION_1, virtio_net::VIRTIO_NET_F_MAC};
use vm_memory::ByteValued;

use crate::devices::event::WindowsEvent;
use crate::devices::virtio::device::{DeviceQueue, DeviceState, QueueConfig, VirtioDevice};
use crate::devices::virtio::mmio::InterruptTransport;
use crate::devices::virtio::net::gvproxy::GvProxy;
use crate::devices::virtio::net::worker::NetWorker;
use crate::devices::virtio::net::{NUM_QUEUES, QUEUE_CONFIG};
use crate::devices::virtio::{ActivateError, ActivateResult, TYPE_NET};
use crate::memory::memory::MemoryManager;

/// Device configuration layout per VirtIO spec §5.1.4:
///
/// ```c
/// struct virtio_net_config {
///     u8 mac[6];
///     le16 status;
///     le16 max_virtqueue_pairs;
/// };
/// ```
#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct NetConfig {
    mac: [u8; 6],
    status: u16,
    max_virtqueue_pairs: u16,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for NetConfig {}

pub struct Net {
    avail_features: u64,
    acked_features: u64,
    device_state: DeviceState,
    worker_thread: Option<JoinHandle<()>>,
    worker_stop_event: Arc<WindowsEvent>,
    config: NetConfig,
}

impl Net {
    /// Create a new virtio-net device.
    ///
    /// gvproxy is spawned later when [`activate()`](VirtioDevice::activate)
    /// is called by the guest driver.
    pub fn new(mac: [u8; 6]) -> io::Result<Self> {      
        let config = NetConfig {
            mac,
            status: 0,             // we do not offer VIRTIO_NET_F_STATUS
            max_virtqueue_pairs: 1, // we do not offer VIRTIO_NET_F_MQ
        };

        Ok(Self {
            avail_features: 1u64 << VIRTIO_F_VERSION_1 | 1u64 << VIRTIO_NET_F_MAC,
            acked_features: 0,
            device_state: DeviceState::Inactive,
            worker_thread: None,
            worker_stop_event: Arc::new(WindowsEvent::new().unwrap()),
            config,
        })
    }
}

impl VirtioDevice for Net {
    fn device_type(&self) -> u32 {
        TYPE_NET
    }

    fn device_name(&self) -> &str {
        "net"
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
        warn!(
            "Net: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
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
            panic!("virtio_net: worker thread already exists");
        }

        let gvproxy = GvProxy::spawn().map_err(|e| {
            error!("Failed to spawn gvproxy: {e}");
            ActivateError::BadActivate
        })?;

        let [rx_q, tx_q]: [DeviceQueue; NUM_QUEUES] = queues.try_into().map_err(|_| {
            error!("Cannot perform activate. Expected {NUM_QUEUES} queue(s)");
            ActivateError::BadActivate
        })?;

        let worker = NetWorker::new(
            rx_q,
            tx_q,
            interrupt.clone(),
            mem.clone(),
            Arc::clone(&self.worker_stop_event),
            gvproxy,
        );
        self.worker_thread = Some(worker.run());
        self.device_state = DeviceState::Activated(mem, interrupt);
        Ok(())
    }

    fn reset(&mut self) -> bool {
        if let Some(worker) = self.worker_thread.take() {
            let _ = self.worker_stop_event.signal();
            if let Err(e) = worker.join() {
                error!("error waiting for net worker thread: {e:?}");
            }
        }
        self.device_state = DeviceState::Inactive;
        true
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        if self.worker_thread.is_some() {
            self.reset();
        }
    }
}
