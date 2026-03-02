// Copyright (C) 2019 Alibaba Cloud Computing. All rights reserved.
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

use std::borrow::Borrow;
use std::ops::Deref;
use std::result;
use std::sync::Arc;

use crate::address::Address;
use crate::bitmap::{Bitmap, BS};
use crate::guest_memory::{self, FileOffset, GuestAddress, GuestUsize, MemoryRegionAddress};
use crate::region::{
    GuestMemoryRegion, GuestMemoryRegionBytes, GuestRegionCollection, GuestRegionCollectionError,
};
use crate::volatile_memory::VolatileSlice;

// Focus strictly on Windows-specific implementation
mod windows;
pub use std::io::Error as MmapRegionError;
pub use windows::MmapRegion;

/// GuestMemoryRegion implementation for Windows memory mapping.
#[derive(Debug)]
pub struct GuestRegionMmap<B = ()> {
    mapping: Arc<MmapRegion<B>>,
    guest_base: GuestAddress,
}

impl<B> Deref for GuestRegionMmap<B> {
    type Target = MmapRegion<B>;

    fn deref(&self) -> &MmapRegion<B> {
        self.mapping.as_ref()
    }
}

impl<B: Bitmap> GuestRegionMmap<B> {
    pub fn new(mapping: MmapRegion<B>, guest_base: GuestAddress) -> Option<Self> {
        Self::with_arc(Arc::new(mapping), guest_base)
    }

    pub fn with_arc(mapping: Arc<MmapRegion<B>>, guest_base: GuestAddress) -> Option<Self> {
        guest_base
            .0
            .checked_add(mapping.size() as u64)
            .map(|_| Self {
                mapping,
                guest_base,
            })
    }

    pub fn get_mmap(&self) -> Arc<MmapRegion<B>> {
        Arc::clone(&self.mapping)
    }
}

impl<B: crate::bitmap::NewBitmap> GuestRegionMmap<B> {
    pub fn from_range(
        addr: GuestAddress,
        size: usize,
        file: Option<FileOffset>,
    ) -> result::Result<Self, FromRangesError> {
        // Windows uses specific MapViewOfFile logic inside MmapRegion
        let region = if let Some(f_off) = file {
            MmapRegion::from_file(f_off, size)?
        } else {
            MmapRegion::new(size)?
        };

        Self::new(region, addr).ok_or(FromRangesError::InvalidGuestRegion)
    }
}

impl<B: Bitmap> GuestMemoryRegion for GuestRegionMmap<B> {
    type B = B;

    fn len(&self) -> GuestUsize {
        self.mapping.size() as GuestUsize
    }

    fn start_addr(&self) -> GuestAddress {
        self.guest_base
    }

    fn bitmap(&self) -> BS<'_, Self::B> {
        self.mapping.bitmap().slice_at(0)
    }

    fn get_host_address(&self, addr: MemoryRegionAddress) -> guest_memory::Result<*mut u8> {
        self.check_address(addr)
            .ok_or(guest_memory::Error::InvalidBackendAddress)
            .map(|addr| {
                self.mapping
                    .as_ptr()
                    .wrapping_offset(addr.raw_value() as isize)
            })
    }

    fn file_offset(&self) -> Option<&FileOffset> {
        self.mapping.file_offset()
    }

    fn get_slice(
        &self,
        offset: MemoryRegionAddress,
        count: usize,
    ) -> guest_memory::Result<VolatileSlice<'_, BS<'_, B>>> {
        let slice = self.mapping.get_slice(offset.raw_value() as usize, count)?;
        Ok(slice)
    }
}

impl<B: Bitmap> GuestMemoryRegionBytes for GuestRegionMmap<B> {}

pub type GuestMemoryMmap<B = ()> = GuestRegionCollection<GuestRegionMmap<B>>;

#[derive(Debug, thiserror::Error)]
pub enum FromRangesError {
    #[error("Error constructing guest region collection: {0}")]
    Collection(#[from] GuestRegionCollectionError),
    #[error("Error setting up raw memory for guest region: {0}")]
    MmapRegion(#[from] MmapRegionError),
    #[error("Combination of guest address and region length invalid (would overflow)")]
    InvalidGuestRegion,
}

impl<B: crate::bitmap::NewBitmap> GuestMemoryMmap<B> {
    pub fn from_ranges(ranges: &[(GuestAddress, usize)]) -> result::Result<Self, FromRangesError> {
        Self::from_ranges_with_files(ranges.iter().map(|r| (r.0, r.1, None)))
    }

    pub fn from_ranges_with_files<A, T>(ranges: T) -> result::Result<Self, FromRangesError>
    where
        A: Borrow<(GuestAddress, usize, Option<FileOffset>)>,
        T: IntoIterator<Item = A>,
    {
        Self::from_regions(
            ranges
                .into_iter()
                .map(|x| {
                    let b = x.borrow();
                    GuestRegionMmap::from_range(b.0, b.1, b.2.clone())
                })
                .collect::<Result<Vec<_>, _>>()?,
        )
        .map_err(Into::into)
    }
}