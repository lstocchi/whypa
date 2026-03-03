use std::{sync::Arc, thread::JoinHandle};

use tracing::{error, warn};
use virtio_bindings::virtio_config::VIRTIO_F_VERSION_1;

use crate::{devices::{event::WindowsEvent, virtio::{ActivateError, ActivateResult, TYPE_RNG, device::{DeviceQueue, DeviceState, QueueConfig, VirtioDevice}, mmio::InterruptTransport, rng::{QUEUE_CONFIG, worker::RngWorker}}}, memory::memory::MemoryManager};

pub struct Rng {
    avail_features: u64,
    acked_features: u64,
    device_state: DeviceState,
    worker_thread: Option<JoinHandle<()>>,
    worker_stop_event: Arc<WindowsEvent>,
}

impl Rng {
    pub fn new() -> Self {
        Self {
            avail_features: 1u64 << VIRTIO_F_VERSION_1,
            acked_features: 0,
            device_state: DeviceState::Inactive,
            worker_thread: None,
            worker_stop_event: Arc::new(WindowsEvent::new().unwrap()),
        }
    }
}

impl VirtioDevice for Rng {
    fn device_type(&self) -> u32 {
        TYPE_RNG
    }

    fn device_name(&self) -> &str {
        "virtio_rng"
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
        
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        warn!("Guest attempted to write config");
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
            panic!("virtio_rng: worker thread already exists");
        }

        let [rng_q] = queues.try_into().map_err(|_| {
            error!("Cannot perform activate. Expected 1 queue(s)");
            ActivateError::BadActivate
        })?;

        

        let worker = RngWorker::new(rng_q, interrupt.clone(), mem.clone(), Arc::clone(&self.worker_stop_event));
        self.worker_thread = Some(worker.run());
        self.device_state = DeviceState::Activated(mem, interrupt);
        Ok(())
    }

    fn reset(&mut self) -> bool {
        if let Some(worker) = self.worker_thread.take() {
            let _ = self.worker_stop_event.signal();
            if let Err(e) = worker.join() {
                error!("error waiting for worker thread: {e:?}");
            }
        }
        self.device_state = DeviceState::Inactive;
        true
    }
}