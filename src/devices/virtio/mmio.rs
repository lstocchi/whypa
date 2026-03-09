// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::fmt::{Display, Formatter};
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use virtio_bindings::virtio_ring::VIRTIO_RING_F_EVENT_IDX;

use super::device_status;
use super::*;

use crate::byte_order;
use crate::devices::bus::BusDevice;
use crate::devices::event::WindowsEvent;
use crate::devices::legacy::irqchip::IrqChip;
use crate::devices::virtio::device::{DeviceQueue, QueueConfig, VirtioDevice};
use crate::devices::virtio::queue::Queue;
use crate::memory::memory::{GuestAddress, MemoryManager};
use tracing::{debug, error, warn};


//TODO crosvm uses 0 here, but IIRC virtio specified some other vendor id that should be used
const VENDOR_ID: u32 = 0;

//required by the virtio mmio device register layout at offset 0 from base
const MMIO_MAGIC_VALUE: u32 = 0x7472_6976;

//current version specified by the mmio standard (legacy devices used 1 here)
const MMIO_VERSION: u32 = 2;

#[derive(Debug)]
pub enum CreateMmioTransportError {
    CreateInterruptEventFd(io::Error),
}

impl Display for CreateMmioTransportError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            CreateMmioTransportError::CreateInterruptEventFd(err) => {
                write!(f, "failed to create interrupt eventfd: {err}")
            }
        }
    }
}

impl std::error::Error for CreateMmioTransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CreateMmioTransportError::CreateInterruptEventFd(err) => Some(err),
        }
    }
}

/// Implements the
/// [MMIO](http://docs.oasis-open.org/virtio/virtio/v1.0/cs04/virtio-v1.0-cs04.html#x1-1090002)
/// transport for virtio devices.
///
/// This requires 3 points of installation to work with a VM:
///
/// 1. Mmio reads and writes must be sent to this device at what is referred to here as MMIO base.
/// 1. `Mmio::queue_evts` must be installed at `virtio::NOTIFY_REG_OFFSET` offset from the MMIO
///    base. Each event in the array must be signaled if the index is written at that offset.
/// 1. `Mmio::interrupt_evt` must signal an interrupt that the guest driver is listening to when it
///    is written to.
///
/// Typically one page (4096 bytes) of MMIO address space is sufficient to handle this transport
/// and inner virtio device.
pub struct MmioTransport {
    device: Arc<Mutex<dyn VirtioDevice>>,
    // The register where feature bits are stored.
    pub(crate) features_select: u32,
    // The register where features page is selected.
    pub(crate) acked_features_select: u32,
    pub(crate) queue_select: u32,
    pub(crate) device_status: u32,
    pub(crate) config_generation: u32,
    mem: MemoryManager,
    // Queues owned by the transport during negotiation.
    // These are moved to the device on activation.
    queues: Option<Vec<Queue>>,
    // Queue eventfds - kept by transport to send notifications.
    // Arc clones are passed to the device on activation.
    queue_evts: Vec<Arc<WindowsEvent>>,
    // Stored queue config from device for recreating queues after reset.
    queue_config: Vec<QueueConfig>,
    shm_region_select: u32,
    interrupt: InterruptTransport,
}

struct InterruptTransportInner {
    log_target: String,
    status: AtomicUsize,
    event: WindowsEvent,
    intc: IrqChip,
    irq_line: Option<u32>,
}

#[derive(Clone)]
pub struct InterruptTransport(Arc<InterruptTransportInner>);

impl InterruptTransport {
    pub fn new(intc: IrqChip, log_target: String) -> Result<Self, CreateMmioTransportError> {
        Ok(Self(Arc::new(InterruptTransportInner {
            log_target,
            status: AtomicUsize::new(0),
            event: WindowsEvent::new().map_err(CreateMmioTransportError::CreateInterruptEventFd)?,
            intc,
            irq_line: None,
        })))
    }

    pub fn status(&self) -> &AtomicUsize {
        &self.0.status
    }

    pub fn event(&self) -> &WindowsEvent {
        &self.0.event
    }

    pub fn intc(&self) -> &IrqChip {
        &self.0.intc
    }

    pub fn irq_line(&self) -> Option<u32> {
        self.0.irq_line
    }

    fn set_irq_line(&mut self, irq_line: u32) {
        debug!("[{}] set_irq_line: {irq_line}", self.0.log_target);
        match Arc::get_mut(&mut self.0) {
            None => {
                error!("Cannot change irq_line of activated device");
            }
            Some(interrupt) => {
                interrupt.irq_line = Some(irq_line);
            }
        }
    }

    fn try_signal(&self, status: u32) -> Result<(), anyhow::Error> {
        self.status().fetch_or(status as usize, Ordering::SeqCst);
        self.intc()
            .lock()
            .unwrap()
            .set_irq(self.0.irq_line, Some(&self.0.event))?;
        Ok(())
    }

    pub fn try_signal_used_queue(&self) -> Result<(), anyhow::Error> {
        debug!("[{}] interrupt: signal_used_queue", self.0.log_target);
        self.try_signal(VIRTIO_MMIO_INT_VRING)
    }

    pub fn try_signal_config_change(&self) -> Result<(), anyhow::Error> {
        debug!("[{}] interrupt: signal_config_change", self.0.log_target);
        self.try_signal(VIRTIO_MMIO_INT_CONFIG)
    }

    /// De-assert the IRQ line on the interrupt controller, clearing the IOAPIC
    /// IRR bit.  Called after InterruptAck drives the status register to zero.
    pub fn try_clear_irq(&self) {
        if let Err(e) = self
            .intc()
            .lock()
            .unwrap()
            .clear_irq(self.0.irq_line)
        {
            warn!("[{}] Failed to clear IRQ: {e:?}", self.0.log_target);
        }
    }

    pub fn signal_used_queue(&self) {
        if let Err(e) = self.try_signal_used_queue() {
            warn!("[{}] Failed to signal used queue: {e:?}", self.0.log_target);
        }
    }

    pub fn signal_config_change(&self) {
        if let Err(e) = self.try_signal_config_change() {
            warn!("[{}] Failed to signal config change: {e:?}", self.0.log_target);
        }
    }
}

impl MmioTransport {
    /// Constructs a new MMIO transport for the given virtio device.
    pub fn new(
        mem: MemoryManager,
        intc: IrqChip,
        device: Arc<Mutex<dyn VirtioDevice>>,
    ) -> Result<MmioTransport, CreateMmioTransportError> {
        let locked = device
            .try_lock()
            .expect("Mutex of VirtioDevice should not be locked when calling MmioTransport::new");

        let debug_log_target = format!("{}[{}]", module_path!(), locked.device_name());
        let queue_config: Vec<QueueConfig> = locked.queue_config().to_vec();
        drop(locked);

        let queues = Self::create_queues(&queue_config);
        let queue_evts = Self::create_queue_evts(queue_config.len())?;

        Ok(MmioTransport {
            interrupt: InterruptTransport::new(intc, debug_log_target)?,
            device,
            features_select: 0,
            acked_features_select: 0,
            queue_select: 0,
            device_status: device_status::INIT,
            config_generation: 0,
            mem,
            queues: Some(queues),
            queue_evts,
            queue_config,
            shm_region_select: 0,
        })
    }

    /// Create queues from queue configuration.
    fn create_queues(queue_config: &[QueueConfig]) -> Vec<Queue> {
        queue_config.iter().map(|c| Queue::new(c.size)).collect()
    }

    /// Create eventfds for queue notifications.
    fn create_queue_evts(count: usize) -> Result<Vec<Arc<WindowsEvent>>, CreateMmioTransportError> {
        let mut queue_evts = Vec::with_capacity(count);
        for _ in 0..count {
            queue_evts.push(Arc::new(
                WindowsEvent::new()
                    .map_err(CreateMmioTransportError::CreateInterruptEventFd)?,
            ));
        }
        Ok(queue_evts)
    }

    /// Set the irq line for the device.
    /// NOTE: Can only be called when the device is not activated
    pub fn set_irq_line(&mut self, irq_line: u32) {
        self.interrupt.set_irq_line(irq_line);
    }

    pub fn interrupt_evt(&self) -> &WindowsEvent {
        self.interrupt.event()
    }

    pub fn locked_device(&self) -> MutexGuard<'_, dyn VirtioDevice + 'static> {
        self.device.lock().expect("Poisoned device lock")
    }

    // Gets the encapsulated VirtioDevice.
    pub fn device(&self) -> Arc<Mutex<dyn VirtioDevice>> {
        self.device.clone()
    }

    /// Returns a reference to the queue eventfds. Used by the VMM to register
    /// queue notifications with KVM.
    pub fn queue_evts(&self) -> &[Arc<WindowsEvent>] {
        &self.queue_evts
    }

    fn check_device_status(&self, set: u32, clr: u32) -> bool {
        self.device_status & (set | clr) == set
    }

    fn with_queue<U, F>(&self, d: U, f: F) -> U
    where
        F: FnOnce(&Queue) -> U,
    {
        match &self.queues {
            Some(queues) => match queues.get(self.queue_select as usize) {
                Some(queue) => f(queue),
                None => d,
            },
            None => d,
        }
    }

    fn with_queue_mut<F: FnOnce(&mut Queue)>(&mut self, f: F) -> bool {
        match &mut self.queues {
            Some(queues) => {
                if let Some(queue) = queues.get_mut(self.queue_select as usize) {
                    f(queue);
                    true
                } else {
                    false
                }
            }
            None => false,
        }
    }

    fn update_queue_field<F: FnOnce(&mut Queue)>(&mut self, f: F) {
        if self.check_device_status(device_status::FEATURES_OK, device_status::FAILED) {
            // FIXME: check if activated!
            self.with_queue_mut(f);
        } else {
            warn!(
                "update virtio queue in invalid state 0x{:x}",
                self.device_status
            );
        }
    }

    fn reset(&mut self) {
        if self.locked_device().is_activated() {
            debug!("reset device while it's still in active state");
        }
        self.features_select = 0;
        self.acked_features_select = 0;
        self.queue_select = 0;
        self.interrupt.0.status.store(0, Ordering::SeqCst);
        self.device_status = device_status::INIT;
        // Do not reset config_generation and keep it monotonically increasing.
        // Recreate queues from queue_config for the next negotiation cycle.
        // Keep queue_evts as is - they are reused across reset cycles.
        // TODO: consider resting the events when we refactor event handling
        self.queues = Some(Self::create_queues(&self.queue_config));
        // . Do not reset config_generation and keep it monotonically increasing
    }

    fn activate(&mut self) {
        let Some(queues) = self.queues.take() else {
            return;
        };

        let mut device_queues: Vec<DeviceQueue> = queues
            .into_iter()
            .zip(self.queue_evts.iter().cloned())
            .map(|(queue, event)| DeviceQueue::new(queue, event))
            .collect();

        let mut locked_device = self.locked_device();
        let event_idx_enabled =
            (locked_device.acked_features() & (1 << VIRTIO_RING_F_EVENT_IDX)) != 0;
        for dq in &mut device_queues {
            dq.queue.set_event_idx(event_idx_enabled);
        }
        locked_device
            .activate(self.mem.clone(), self.interrupt.clone(), device_queues)
            .expect("Failed to activate device");
    }

    /// Update device status according to the state machine defined by VirtIO Spec 1.0.
    /// Please refer to VirtIO Spec 1.0, section 2.1.1 and 3.1.1.
    ///
    /// The driver MUST update device status, setting bits to indicate the completed steps
    /// of the driver initialization sequence specified in 3.1. The driver MUST NOT clear
    /// a device status bit. If the driver sets the FAILED bit, the driver MUST later reset
    /// the device before attempting to re-initialize.
    #[allow(unused_assignments)]
    fn set_device_status(&mut self, status: u32) {
        use device_status::*;
        // match changed bits
        match !self.device_status & status {
            ACKNOWLEDGE if self.device_status == INIT => {
                self.device_status = status;
            }
            DRIVER if self.device_status == ACKNOWLEDGE => {
                self.device_status = status;
            }
            FEATURES_OK if self.device_status == (ACKNOWLEDGE | DRIVER) => {
                self.device_status = status;
            }
            DRIVER_OK if self.device_status == (ACKNOWLEDGE | DRIVER | FEATURES_OK) => {
                self.device_status = status;
                let device_activated = self.locked_device().is_activated();
                if !device_activated {
                    self.activate();
                }
            }
            _ if (status & FAILED) != 0 => {
                // TODO: notify backend driver to stop the device
                self.device_status |= FAILED;
            }
            _ if status == 0 => {
                if self.locked_device().is_activated() && !self.locked_device().reset() {
                    self.device_status |= FAILED;
                }

                // If the backend device driver doesn't support reset,
                // just leave the device marked as FAILED.
                if self.device_status & FAILED == 0 {
                    self.reset();
                }
            }
            _ => {
                warn!(
                    "invalid virtio driver status transition: 0x{:x} -> 0x{:x}",
                    self.device_status, status
                );
            }
        }
    }
}

impl BusDevice for MmioTransport {
    fn read(&mut self, _vcpuid: u64, offset: u64, data: &mut [u8]) {
        match offset {
            0x00..=0xff if data.len() == 4 => {
                let v = match offset {
                    0x0 => MMIO_MAGIC_VALUE,
                    0x04 => MMIO_VERSION,
                    0x08 => self.locked_device().device_type(),
                    0x0c => VENDOR_ID, // vendor id
                    0x10 => {
                        let mut features = self
                            .locked_device()
                            .avail_features_by_page(self.features_select);
                        if self.features_select == 1 {
                            features |= 0x1; // enable support of VirtIO Version 1
                        }
                        features
                    }
                    0x34 => self.with_queue(0, |q| u32::from(q.get_max_size())),
                    0x44 => self.with_queue(0, |q| q.ready as u32),
                    0x60 => self.interrupt.status().load(Ordering::SeqCst) as u32,
                    0x70 => self.device_status,
                    0xfc => self.config_generation,
                    0xb0..=0xbc => {
                        // For no SHM region or invalid region the kernel looks for length of -1
                        let (shm_base, shm_len) = if self.shm_region_select > 1 {
                            (0, !0)
                        } else {
                            match self.locked_device().shm_region() {
                                Some(region) => (region.guest_addr, region.size as u64),
                                None => (0, !0),
                            }
                        };
                        match offset {
                            0xb0 => shm_len as u32,
                            0xb4 => (shm_len >> 32) as u32,
                            0xb8 => shm_base as u32,
                            0xbc => (shm_base >> 32) as u32,
                            _ => {
                                error!("invalid shm region offset");
                                0
                            }
                        }
                    }
                    _ => {
                        warn!("unknown virtio mmio register read: 0x{offset:x}");
                        return;
                    }
                };
                byte_order::write_le_u32(data, v);
            }
            0x100..=0xfff => self.locked_device().read_config(offset - 0x100, data),
            _ => {
                warn!(
                    "invalid virtio mmio read: 0x{:x}:0x{:x}",
                    offset,
                    data.len()
                );
            }
        };
    }

    fn write(&mut self, _vcpuid: u64, offset: u64, data: &[u8]) {
        fn hi(v: &mut GuestAddress, x: u32) {
            *v = GuestAddress((v.0 & 0xffff_ffff) | (u64::from(x) << 32))
        }

        fn lo(v: &mut GuestAddress, x: u32) {
            *v = GuestAddress((v.0 & !0xffff_ffff) | u64::from(x))
        }

        match offset {
            0x00..=0xff if data.len() == 4 => {
                let v = byte_order::read_le_u32(data);
                match offset {
                    0x14 => self.features_select = v,
                    0x20 => {
                        if self.check_device_status(
                            device_status::DRIVER,
                            device_status::FEATURES_OK | device_status::FAILED,
                        ) {
                            self.locked_device()
                                .ack_features_by_page(self.acked_features_select, v);
                        } else {
                            warn!(
                                "ack virtio features in invalid state 0x{:x}",
                                self.device_status
                            );
                        }
                    }
                    0x24 => self.acked_features_select = v,
                    0x30 => self.queue_select = v,
                    0x38 => self.update_queue_field(|q| q.size = v as u16),
                    0x44 => self.update_queue_field(|q| q.ready = v == 1),
                    0x50 => {
                        // Queue notification - write to the eventfd for the specified queue.
                        if let Some(eventfd) = self.queue_evts.get(v as usize) {
                            eventfd.signal().unwrap();
                        } else {
                            warn!("invalid queue index for notification: {v}");
                        }
                    }
                    0x64 => {
                        if self.check_device_status(device_status::DRIVER_OK, 0) {
                            let prev = self.interrupt
                                .status()
                                .fetch_and(!(v as usize), Ordering::SeqCst);
                            // If the interrupt status is now zero, de-assert the
                            // IRQ line so the IOAPIC's IRR is cleared.  This
                            // prevents level-triggered re-delivery on EOI when
                            // the device has nothing more to report.
                            if prev & !(v as usize) == 0 {
                                self.interrupt.try_clear_irq();
                            }
                        }
                    }
                    0x70 => self.set_device_status(v),
                    0x80 => self.update_queue_field(|q| lo(&mut q.desc_table, v)),
                    0x84 => self.update_queue_field(|q| hi(&mut q.desc_table, v)),
                    0x90 => self.update_queue_field(|q| lo(&mut q.avail_ring, v)),
                    0x94 => self.update_queue_field(|q| hi(&mut q.avail_ring, v)),
                    0xa0 => self.update_queue_field(|q| lo(&mut q.used_ring, v)),
                    0xa4 => self.update_queue_field(|q| hi(&mut q.used_ring, v)),
                    0xac => self.shm_region_select = v,
                    _ => {
                        warn!("unknown virtio mmio register write: 0x{offset:x}");
                    }
                }
            }
            0x100..=0xfff => {
                if self.check_device_status(device_status::DRIVER, device_status::FAILED) {
                    self.locked_device().write_config(offset - 0x100, data)
                } else {
                    warn!("can not write to device config data area before driver is ready");
                }
            }
            _ => {
                warn!(
                    "invalid virtio mmio write: 0x{:x}:0x{:x}",
                    offset,
                    data.len()
                );
            }
        }
    }

    fn interrupt(&self, irq_mask: u32) -> std::io::Result<()> {
        self.interrupt
            .status()
            .fetch_or(irq_mask as usize, Ordering::SeqCst);
        // interrupt_evt() is safe to unwrap because the inner interrupt_evt is initialized in the
        // constructor.
        // write() is safe to unwrap because the inner syscall is tailored to be safe as well.
        self.interrupt.event().signal().unwrap();
        Ok(())
    }
}