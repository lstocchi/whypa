use crate::devices::virtio::device::QueueConfig;

pub mod device;
mod gvproxy;
mod worker;

const QUEUE_SIZE: u16 = 1024;

/// if VIRTIO_NET_F_MQ is not negotiated, we only have 2 queues: receiveq and transmitq.
pub const NUM_QUEUES: usize = 2;
pub static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [
    QueueConfig::new(QUEUE_SIZE), // receiveq
    QueueConfig::new(QUEUE_SIZE), // transmitq
];