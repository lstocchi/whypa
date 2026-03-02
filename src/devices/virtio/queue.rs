use std::fmt;
use std::{cmp::min, fmt::Display, num::Wrapping};
use std::sync::atomic::{fence, Ordering};

use zerocopy::FromBytes;
use tracing::error;

use crate::memory::memory::{GuestAddress, MemoryManager};

/// Virtio Queue related errors.
#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum Error {
    /// Address overflow.
    AddressOverflow,
    /// Failed to access guest memory.
    GuestMemory(anyhow::Error),
    /// Invalid indirect descriptor.
    InvalidIndirectDescriptor,
    /// Invalid indirect descriptor table.
    InvalidIndirectDescriptorTable,
    /// Invalid descriptor chain.
    InvalidChain,
    /// Invalid descriptor index.
    InvalidDescriptorIndex,
    /// Invalid max_size.
    InvalidMaxSize,
    /// Invalid Queue Size.
    InvalidSize,
    /// Invalid alignment of descriptor table address.
    InvalidDescTableAlign,
    /// Invalid alignment of available ring address.
    InvalidAvailRingAlign,
    /// Invalid alignment of used ring address.
    InvalidUsedRingAlign,
    /// Invalid available ring index.
    InvalidAvailRingIndex,
    /// The queue is not ready for operation.
    QueueNotReady,
    /// Volatile memory error.
    //VolatileMemoryError(VolatileMemoryError),
    /// The combined length of all the buffers in a `DescriptorChain` would overflow.
    DescriptorChainOverflow,
    /// No memory region for this address range.
    FindMemoryRegion,
    /// Descriptor guest memory error.
    //GuestMemoryError(GuestMemoryError),
    /// DescriptorChain split is out of bounds.
    SplitOutOfBounds(usize),
}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            AddressOverflow => write!(f, "address overflow"),
            GuestMemory(_) => write!(f, "error accessing guest memory"),
            InvalidChain => write!(f, "invalid descriptor chain"),
            InvalidIndirectDescriptor => write!(f, "invalid indirect descriptor"),
            InvalidIndirectDescriptorTable => write!(f, "invalid indirect descriptor table"),
            InvalidDescriptorIndex => write!(f, "invalid descriptor index"),
            InvalidMaxSize => write!(f, "invalid queue maximum size"),
            InvalidSize => write!(f, "invalid queue size"),
            InvalidDescTableAlign => write!(
                f,
                "virtio queue descriptor table breaks alignment constraints"
            ),
            InvalidAvailRingAlign => write!(
                f,
                "virtio queue available ring breaks alignment constraints"
            ),
            InvalidUsedRingAlign => {
                write!(f, "virtio queue used ring breaks alignment constraints")
            }
            InvalidAvailRingIndex => write!(
                f,
                "invalid available ring index (more descriptors to process than queue size)"
            ),
            QueueNotReady => write!(f, "trying to process requests on a queue that's not ready"),
            //VolatileMemoryError(e) => write!(f, "volatile memory error: {e}"),
            DescriptorChainOverflow => write!(
                f,
                "the combined length of all the buffers in a `DescriptorChain` would overflow"
            ),
            FindMemoryRegion => write!(f, "no memory region for this address range"),
            //GuestMemoryError(e) => write!(f, "descriptor guest memory error: {e}"),
            SplitOutOfBounds(off) => write!(f, "`DescriptorChain` split is out of bounds: {off}"),
        }
    }
}

impl std::error::Error for Error {}

/// Represents the contents of an element from the used virtqueue ring.
// Note that the `ByteValued` implementation of this structure expects the `VirtqUsedElem` to store
// only plain old data types.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct VirtqUsedElem {
    id: u32,
    len: u32,
}

impl VirtqUsedElem {
    /// Create a new `VirtqUsedElem` instance.
    ///
    /// # Arguments
    /// * `id` - the index of the used descriptor chain.
    /// * `len` - the total length of the descriptor chain which was used (written to).
    pub(crate) fn new(id: u32, len: u32) -> Self {
        VirtqUsedElem { id, len }
    }
}

// Descriptor Flags
pub(crate) const VIRTQ_DESC_F_NEXT: u16     = 0x1; // Marks this buffer as chained
pub(crate) const VIRTQ_DESC_F_WRITE: u16    = 0x2; // Device-writable (Guest Read)
/// Size of used ring header: flags (u16) + idx (u16)
pub(crate) const VIRTQ_USED_RING_HEADER_SIZE: u64 = 4;

/// Size of one element in the used ring, id (le32) + len (le32).
pub(crate) const VIRTQ_USED_ELEMENT_SIZE: u64 = 8;

/// Size of available ring header: flags(u16) + idx(u16)
pub(crate) const VIRTQ_AVAIL_RING_HEADER_SIZE: u64 = 4;

/// Size of one element in the available ring (le16).
pub(crate) const VIRTQ_AVAIL_ELEMENT_SIZE: u64 = 2;

pub(crate) const VRING_USED_F_NO_NOTIFY: u32 = 1;

// MemoryManager::read_obj() will be used to fetch the descriptor,
// which has an explicit constraint that the entire descriptor doesn't
// cross the page boundary. Otherwise the descriptor may be splitted into
// two mmap regions which causes failure of MemoryManager::read_obj().
//
// The Virtio Spec 1.0 defines the alignment of VirtIO descriptor is 16 bytes,
// which fulfills the explicit constraint of MemoryManager::read_obj().

/// An iterator over a single descriptor chain.  Not to be confused with AvailIter,
/// which iterates over the descriptor chain heads in a queue.
pub struct DescIter<'a> {
    next: Option<DescriptorChain<'a>>,
}

impl<'a> DescIter<'a> {
    /// Returns an iterator that only yields the readable descriptors in the chain.
    pub fn readable(self) -> impl Iterator<Item = DescriptorChain<'a>> {
        self.take_while(DescriptorChain::is_read_only)
    }

    /// Returns an iterator that only yields the writable descriptors in the chain.
    pub fn writable(self) -> impl Iterator<Item = DescriptorChain<'a>> {
        self.skip_while(DescriptorChain::is_read_only)
    }
}

impl<'a> Iterator for DescIter<'a> {
    type Item = DescriptorChain<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(current) = self.next.take() {
            self.next = current.next_descriptor();
            Some(current)
        } else {
            None
        }
    }
}

/// A virtio descriptor constraints with C representive.
#[repr(C)]
#[derive(Default, Clone, Copy, FromBytes)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// A virtio descriptor chain.
#[derive(Clone)]
pub struct DescriptorChain<'a> {
    desc_table: GuestAddress,
    queue_size: u16, /// Number of descriptors (must be a power of 2, e.g., 128 or 256)
    ttl: u16, // used to prevent infinite chain cycles

    /// Reference to guest memory
    pub mem: &'a MemoryManager,

    /// Index into the descriptor table
    pub index: u16,

    /// Guest physical address of device specific data
    pub addr: GuestAddress,

    /// Length of device specific data
    pub len: u32,

    /// Includes next, write, and indirect bits
    pub flags: u16,

    /// Index into the descriptor table of the next descriptor if flags has
    /// the next bit set
    pub next: u16,
}

impl<'a> DescriptorChain<'a> {
    pub fn checked_new(
        mem: &MemoryManager,
        desc_table: GuestAddress,
        queue_size: u16,
        index: u16,
    ) -> Option<DescriptorChain<'_>> {
        if index >= queue_size {
            return None;
        }

        let desc_addr = desc_table.checked_add(u64::from(index) * 16)?;

        // These reads can't fail unless Guest memory is hopelessly broken.
        let desc = mem.read_obj::<Descriptor>(desc_addr).ok()?;
        let chain = DescriptorChain {
            mem,
            desc_table,
            queue_size,
            ttl: queue_size,
            index,
            addr: GuestAddress(desc.addr),
            len: desc.len,
            flags: desc.flags,
            next: desc.next,
        };

        if chain.is_valid() {
            Some(chain)
        } else {
            None
        }
    }

    fn is_valid(&self) -> bool {
        !self.has_next() || self.next < self.queue_size
    }

    /// Gets if this descriptor chain has another descriptor chain linked after it.
    pub fn has_next(&self) -> bool {
        self.flags & VIRTQ_DESC_F_NEXT != 0 && self.ttl > 1
    }

    /// If the driver designated this as a write only descriptor.
    ///
    /// If this is false, this descriptor is read only.
    /// Write only means the the emulated device can write and the driver can read.
    pub fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }

    /// If the driver designated this as a read only descriptor.
    ///
    /// If this is false, this descriptor is write only.
    /// Read only means the emulated device can read and the driver can write.
    pub fn is_read_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE == 0
    }

    /// Gets the next descriptor in this descriptor chain, if there is one.
    ///
    /// Note that this is distinct from the next descriptor chain returned by `AvailIter`, which is
    /// the head of the next _available_ descriptor chain.
    pub fn next_descriptor(&self) -> Option<DescriptorChain<'a>> {
        if self.has_next() {
            DescriptorChain::checked_new(self.mem, self.desc_table, self.queue_size, self.next).map(
                |mut c| {
                    c.ttl = self.ttl - 1;
                    c
                },
            )
        } else {
            None
        }
    }

    /// Produces an iterator over all the descriptors in this chain.
    #[allow(clippy::should_implement_trait)]
    pub fn into_iter(self) -> DescIter<'a> {
        DescIter { next: Some(self) }
    }

    pub fn descriptor(&self) -> Descriptor {
        Descriptor {
            addr: self.addr.0,
            len: self.len,
            flags: self.flags,
            next: self.next,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
/// A virtio queue's parameters.
pub struct Queue {
    /// The maximal size in elements offered by the device
    pub(crate) max_size: u16,

    /// The queue size in elements the driver selected
    pub size: u16,

    /// Indicates if the queue is finished with configuration
    pub ready: bool,

    /// Guest physical address of the descriptor table
    pub desc_table: GuestAddress,

    /// Guest physical address of the available ring
    pub avail_ring: GuestAddress,

    /// Guest physical address of the used ring
    pub used_ring: GuestAddress,

    pub(crate) next_avail: Wrapping<u16>,
    pub(crate) next_used: Wrapping<u16>,

    /// VIRTIO_F_RING_EVENT_IDX negotiated.
    event_idx_enabled: bool,

    /// The number of descriptor chains placed in the used ring via `add_used`
    /// since the last time `needs_notification` was called on the associated queue.
    num_added: Wrapping<u16>,
}

impl Queue {
    /// Constructs an empty virtio queue with the given `max_size`.
    pub fn new(max_size: u16) -> Queue {
        Queue {
            max_size,
            size: 0,
            ready: false,
            desc_table: GuestAddress(0),
            avail_ring: GuestAddress(0),
            used_ring: GuestAddress(0),
            next_avail: Wrapping(0),
            next_used: Wrapping(0),
            event_idx_enabled: false,
            num_added: Wrapping(0),
        }
    }

    pub fn get_max_size(&self) -> u16 {
        self.max_size
    }

    /// Return the actual size of the queue, as the driver may not set up a
    /// queue as big as the device allows.
    pub fn actual_size(&self) -> u16 {
        min(self.size, self.max_size)
    }

    pub fn is_valid(&self, mem: &MemoryManager) -> bool {
        let queue_size = u64::from(self.actual_size());
        let desc_table= self.desc_table;
        let desc_table_size = 16 * queue_size;
        let avail_ring = self.avail_ring;
        let avail_ring_size = 6 + 2 * queue_size;
        let used_ring = self.used_ring;
        let used_ring_size = 6 + 8 * queue_size;
        if !self.ready {
            error!("attempt to use virtio queue that is not marked ready");
            false
        } else if self.size > self.max_size || self.size == 0 || (self.size & (self.size - 1)) != 0
        {
            error!("virtio queue with invalid size: {}", self.size);
            false
        } else if desc_table
            .checked_add(desc_table_size)
            .is_none_or(|v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue descriptor table goes out of bounds: start:0x{:08x} size:0x{:08x}",
                desc_table.raw_value(),
                desc_table_size
            );
            false
        } else if avail_ring
            .checked_add(avail_ring_size)
            .is_none_or(|v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue available ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                avail_ring.raw_value(),
                avail_ring_size
            );
            false
        } else if used_ring
            .checked_add(used_ring_size)
            .is_none_or(|v| !mem.address_in_range(v))
        {
            error!(
                "virtio queue used ring goes out of bounds: start:0x{:08x} size:0x{:08x}",
                used_ring.raw_value(),
                used_ring_size
            );
            false
        } else if desc_table.raw_value() & 0xf != 0 {
            error!("virtio queue descriptor table breaks alignment constraints");
            false
        } else if avail_ring.raw_value() & 0x1 != 0 {
            error!("virtio queue available ring breaks alignment constraints");
            false
        } else if used_ring.raw_value() & 0x3 != 0 {
            error!("virtio queue used ring breaks alignment constraints");
            false
        } else {
            true
        }
    }

    /// Returns the number of yet-to-be-popped descriptor chains in the avail ring.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self, mem: &MemoryManager) -> u16 {
        (self.avail_idx(mem, Ordering::Acquire).unwrap() - self.next_avail).0
    }

    /// Checks if the driver has made any descriptor chains available in the avail ring.
    pub fn is_empty(&self, mem: &MemoryManager) -> bool {
        self.len(mem) == 0
    }

    /// Pop the first available descriptor chain from the avail ring.
    pub fn pop<'b>(&mut self, mem: &'b MemoryManager) -> Option<DescriptorChain<'b>> {
        if self.len(mem) == 0 || self.actual_size() == 0 {
            return None;
        }

        // We'll need to find the first available descriptor, that we haven't yet popped.
        // In a naive notation, that would be:
        // `descriptor_table[avail_ring[next_avail]]`.
        //
        // First, we compute the byte-offset (into `self.avail_ring`) of the index of the next available
        // descriptor. `self.avail_ring` stores the address of a `struct virtq_avail`, as defined by
        // the VirtIO spec:
        //
        // ```C
        // struct virtq_avail {
        //   le16 flags;
        //   le16 idx;
        //   le16 ring[QUEUE_SIZE];
        //   le16 used_event
        // }
        // ```
        //
        // We use `self.next_avail` to store the position, in `ring`, of the next available
        // descriptor index, with a twist: we always only increment `self.next_avail`, so the
        // actual position will be `self.next_avail % self.actual_size()`.
        // We are now looking for the offset of `ring[self.next_avail % self.actual_size()]`.
        // `ring` starts after `flags` and `idx` (4 bytes into `struct virtq_avail`), and holds
        // 2-byte items, so the offset will be:
        let index_offset = 4 + 2 * (self.next_avail.0 % self.actual_size());

        // Make sure we catch all updates on the queue
        std::sync::atomic::fence(Ordering::Acquire);

        // `self.is_valid()` already performed all the bound checks on the descriptor table
        // and virtq rings, so it's safe to unwrap guest memory reads and to use unchecked
        // offsets.
        let desc_index: u16 = mem
            .read_obj(self.avail_ring.unchecked_add(u64::from(index_offset)))
            .unwrap();

        DescriptorChain::checked_new(mem, self.desc_table, self.actual_size(), desc_index)
            .inspect(|_| self.next_avail += Wrapping(1))
    }

    /// Undo the effects of the last `self.pop()` call.
    /// The caller can use this, if it was unable to consume the last popped descriptor chain.
    pub fn undo_pop(&mut self) {
        self.next_avail -= Wrapping(1);
    }

    pub fn add_used(
        &mut self,
        mem: &MemoryManager,
        head_index: u16,
        len: u32,
    ) -> Result<(), Error> {
        if head_index >= self.size {
            error!("attempted to add out of bounds descriptor to used ring: {head_index}");
            return Err(Error::InvalidDescriptorIndex);
        }

        let next_used_index = u64::from(self.next_used.0 % self.size);
        // This can not overflow an u64 since it is working with relatively small numbers compared
        // to u64::MAX.
        let offset = VIRTQ_USED_RING_HEADER_SIZE + next_used_index * VIRTQ_USED_ELEMENT_SIZE;
        let addr = self
            .used_ring
            .checked_add(offset)
            .ok_or(Error::AddressOverflow)?;
        mem.write_obj(VirtqUsedElem::new(head_index.into(), len), addr)
            .map_err(|e| Error::GuestMemory(e));

        self.next_used += Wrapping(1);
        self.num_added += Wrapping(1);

        mem.store(
            self.next_used.0,
            self.used_ring
                .checked_add(2)
                .ok_or(Error::AddressOverflow)?,
            Ordering::Release,
        )
        .map_err(Error::GuestMemory)
    }
    // If the VIRTIO_F_EVENT_IDX feature bit is not negotiated, the flags field in the available
    // ring offers a crude mechanism for the driver to inform the device that it doesn’t want
    // interrupts when buffers are used. Otherwise virtq_avail.used_event is a more performant
    // alternative where the driver specifies how far the device can progress before interrupting.
    //
    // Neither of these interrupt suppression methods are reliable, as they are not synchronized
    // with the device, but they serve as useful optimizations. So we only ensure access to the
    // virtq_avail.used_event is atomic, but do not need to synchronize with other memory accesses.
    fn used_event(&self, mem: &MemoryManager, order: Ordering) -> Result<Wrapping<u16>, Error> {
        // This can not overflow an u64 since it is working with relatively small numbers compared
        // to u64::MAX.
        let used_event_offset =
            VIRTQ_AVAIL_RING_HEADER_SIZE + u64::from(self.size) * VIRTQ_AVAIL_ELEMENT_SIZE;
        let used_event_addr = self
            .avail_ring
            .checked_add(used_event_offset)
            .ok_or(Error::AddressOverflow)?;

        mem.load(used_event_addr, order)
            .map(Wrapping)
            .map_err(Error::GuestMemory)
    }

    // Helper method that writes `val` to the `avail_event` field of the used ring, using
    // the provided ordering.
    fn set_avail_event(
        &self,
        mem: &MemoryManager,
        val: u16,
        order: Ordering,
    ) -> Result<(), Error> {
        // This can not overflow an u64 since it is working with relatively small numbers compared
        // to u64::MAX.
        let avail_event_offset =
            VIRTQ_USED_RING_HEADER_SIZE + VIRTQ_USED_ELEMENT_SIZE * u64::from(self.size);
        let addr = self
            .used_ring
            .checked_add(avail_event_offset)
            .ok_or(Error::AddressOverflow)?;

        mem.store(val, addr, order).map_err(Error::GuestMemory)
    }

    pub fn set_event_idx(&mut self, enabled: bool) {
        self.event_idx_enabled = enabled;
    }

    // Set the value of the `flags` field of the used ring, applying the specified ordering.
    fn set_used_flags(
        &mut self,
        mem: &MemoryManager,
        val: u16,
        order: Ordering,
    ) -> Result<(), Error> {
        mem.store(val, self.used_ring, order)
            .map_err(Error::GuestMemory)
    }

    // Write the appropriate values to enable or disable notifications from the driver.
    //
    // Every access in this method uses `Relaxed` ordering because a fence is added by the caller
    // when appropriate.
    fn set_notification(&mut self, mem: &MemoryManager, enable: bool) -> Result<(), Error> {
        if enable {
            if self.event_idx_enabled {
                // We call `set_avail_event` using the `next_avail` value, instead of reading
                // and using the current `avail_idx` to avoid missing notifications. More
                // details in `enable_notification`.
                self.set_avail_event(mem, self.next_avail.0, Ordering::Relaxed)
            } else {
                self.set_used_flags(mem, 0, Ordering::Relaxed)
            }
        } else if !self.event_idx_enabled {
            self.set_used_flags(mem, VRING_USED_F_NO_NOTIFY as u16, Ordering::Relaxed)
        } else {
            // Notifications are effectively disabled by default after triggering once when
            // `VIRTIO_F_EVENT_IDX` is negotiated, so we don't do anything in that case.
            Ok(())
        }
    }

    // TODO: Turn this into a doc comment/example.
    // With the current implementation, a common way of consuming entries from the available ring
    // while also leveraging notification suppression is to use a loop, for example:
    //
    // loop {
    //     // We have to explicitly disable notifications if `VIRTIO_F_EVENT_IDX` has not been
    //     // negotiated.
    //     self.disable_notification()?;
    //
    //     for chain in self.iter()? {
    //         // Do something with each chain ...
    //         // Let's assume we process all available chains here.
    //     }
    //
    //     // If `enable_notification` returns `true`, the driver has added more entries to the
    //     // available ring.
    //     if !self.enable_notification()? {
    //         break;
    //     }
    // }
    pub fn enable_notification(&mut self, mem: &MemoryManager) -> Result<bool, Error> {
        self.set_notification(mem, true)?;
        // Ensures the following read is not reordered before any previous write operation.
        fence(Ordering::SeqCst);

        // We double check here to avoid the situation where the available ring has been updated
        // just before we re-enabled notifications, and it's possible to miss one. We compare the
        // current `avail_idx` value to `self.next_avail` because it's where we stopped processing
        // entries. There are situations where we intentionally avoid processing everything in the
        // available ring (which will cause this method to return `true`), but in that case we'll
        // probably not re-enable notifications as we already know there are pending entries.
        self.avail_idx(mem, Ordering::Relaxed)
            .map(|idx| idx != self.next_avail)
    }

    pub fn disable_notification(&mut self, mem: &MemoryManager) -> Result<(), Error> {
        self.set_notification(mem, false)
    }

    pub fn needs_notification(&mut self, mem: &MemoryManager) -> Result<bool, Error> {
        let used_idx = self.next_used;

        // Complete all the writes in add_used() before reading the event.
        fence(Ordering::SeqCst);

        // The VRING_AVAIL_F_NO_INTERRUPT flag isn't supported yet.

        // When the `EVENT_IDX` feature is negotiated, the driver writes into `used_event`
        // a value that's used by the device to determine whether a notification must
        // be submitted after adding a descriptor chain to the used ring. According to the
        // standard, the notification must be sent when `next_used == used_event + 1`, but
        // various device model implementations rely on an inequality instead, most likely
        // to also support use cases where a bunch of descriptor chains are added to the used
        // ring first, and only afterwards the `needs_notification` logic is called. For example,
        // the approach based on `num_added` below is taken from the Linux Kernel implementation
        // (i.e. https://elixir.bootlin.com/linux/v5.15.35/source/drivers/virtio/virtio_ring.c#L661)

        // The `old` variable below is used to determine the value of `next_used` from when
        // `needs_notification` was called last (each `needs_notification` call resets `num_added`
        // to zero, while each `add_used` called increments it by one). Then, the logic below
        // uses wrapped arithmetic to see whether `used_event` can be found between `old` and
        // `next_used` in the circular sequence space of the used ring.
        if self.event_idx_enabled {
            let used_event = self.used_event(mem, Ordering::Relaxed)?;
            let old = used_idx - self.num_added;
            self.num_added = Wrapping(0);
            return Ok(used_idx - used_event - Wrapping(1) < used_idx - old);
        }

        Ok(true)
    }

    /// Goes back one position in the available descriptor chain offered by the driver.
    /// Rust does not support bidirectional iterators. This is the only way to revert the effect
    /// of an iterator increment on the queue.
    pub fn go_to_previous_position(&mut self) {
        self.next_avail -= Wrapping(1);
    }

    /// Fetch the available ring index (`virtq_avail->idx`) from guest memory.
    /// This is written by the driver, to indicate the next slot that will be filled in the avail
    /// ring.
    fn avail_idx(&self, mem: &MemoryManager, order: Ordering) -> Result<Wrapping<u16>, Error> {
        let addr = self
            .avail_ring
            .checked_add(2)
            .ok_or(Error::AddressOverflow)?;

        mem.load(addr, order)
            .map(Wrapping)
            .map_err(Error::GuestMemory)
    }
}