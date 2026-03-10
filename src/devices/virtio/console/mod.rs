use crate::devices::virtio::device::QueueConfig;

pub mod device;
pub mod worker;

const QUEUE_SIZE: u16 = 256;

/// Port 0 always has receiveq (index 0) and transmitq (index 1).
/// Control queues and additional ports only exist with VIRTIO_CONSOLE_F_MULTIPORT.
pub const NUM_QUEUES: usize = 2;
pub static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [
    QueueConfig::new(QUEUE_SIZE), // receiveq(port0)
    QueueConfig::new(QUEUE_SIZE), // transmitq(port0)
];
