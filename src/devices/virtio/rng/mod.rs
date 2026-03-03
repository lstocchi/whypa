use crate::devices::virtio::device::QueueConfig;

pub mod device;
pub mod worker;

pub use self::device::Rng;

const QUEUE_SIZE: u16 = 256;
pub(crate) const NUM_QUEUES: usize = 1;
pub(crate) static QUEUE_CONFIG: [QueueConfig; NUM_QUEUES] = [QueueConfig::new(QUEUE_SIZE)];