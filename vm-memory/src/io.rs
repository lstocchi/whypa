// Copyright 2023 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//! Module containing versions of the standard library's [`Read`](std::io::Read) and
//! [`Write`](std::io::Write) traits compatible with volatile memory accesses.

use crate::bitmap::BitmapSlice;
use crate::volatile_memory::copy_slice_impl::{copy_from_volatile_slice, copy_to_volatile_slice};
use crate::{VolatileMemoryError, VolatileSlice};
use std::io::{Cursor, ErrorKind, Stdout};
use std::os::windows::io::{AsRawHandle, AsRawSocket, RawHandle, RawSocket};

macro_rules! retry_eintr {
    ($io_call: expr) => {
        loop {
            let r = $io_call;

            if let Err(crate::VolatileMemoryError::IOError(ref err)) = r {
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
            }

            break r;
        }
    };
}

pub(crate) use retry_eintr;

/// A version of the standard library's [`Read`](std::io::Read) trait that operates on volatile
/// memory instead of slices
///
/// This trait is needed as rust slices (`&[u8]` and `&mut [u8]`) cannot be used when operating on
/// guest memory [1].
///
/// [1]: https://github.com/rust-vmm/vm-memory/pull/217
pub trait ReadVolatile {
    /// Tries to read some bytes into the given [`VolatileSlice`] buffer, returning how many bytes
    /// were read.
    ///
    /// The behavior of implementations should be identical to [`Read::read`](std::io::Read::read)
    fn read_volatile<B: BitmapSlice>(
        &mut self,
        buf: &mut VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError>;

    /// Tries to fill the given [`VolatileSlice`] buffer by reading from `self` returning an error
    /// if insufficient bytes could be read.
    ///
    /// The default implementation is identical to that of [`Read::read_exact`](std::io::Read::read_exact)
    fn read_exact_volatile<B: BitmapSlice>(
        &mut self,
        buf: &mut VolatileSlice<B>,
    ) -> Result<(), VolatileMemoryError> {
        // Implementation based on https://github.com/rust-lang/rust/blob/7e7483d26e3cec7a44ef00cf7ae6c9c8c918bec6/library/std/src/io/mod.rs#L465

        let mut partial_buf = buf.offset(0)?;

        while !partial_buf.is_empty() {
            match retry_eintr!(self.read_volatile(&mut partial_buf)) {
                Ok(0) => {
                    return Err(VolatileMemoryError::IOError(std::io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    )))
                }
                Ok(bytes_read) => partial_buf = partial_buf.offset(bytes_read)?,
                Err(err) => return Err(err),
            }
        }

        Ok(())
    }
}

/// A version of the standard library's [`Write`](std::io::Write) trait that operates on volatile
/// memory instead of slices.
///
/// This trait is needed as rust slices (`&[u8]` and `&mut [u8]`) cannot be used when operating on
/// guest memory [1].
///
/// [1]: https://github.com/rust-vmm/vm-memory/pull/217
pub trait WriteVolatile {
    /// Tries to write some bytes from the given [`VolatileSlice`] buffer, returning how many bytes
    /// were written.
    ///
    /// The behavior of implementations should be identical to [`Write::write`](std::io::Write::write)
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError>;

    /// Tries write the entire content of the given [`VolatileSlice`] buffer to `self` returning an
    /// error if not all bytes could be written.
    ///
    /// The default implementation is identical to that of [`Write::write_all`](std::io::Write::write_all)
    fn write_all_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<(), VolatileMemoryError> {
        // Based on https://github.com/rust-lang/rust/blob/7e7483d26e3cec7a44ef00cf7ae6c9c8c918bec6/library/std/src/io/mod.rs#L1570

        let mut partial_buf = buf.offset(0)?;

        while !partial_buf.is_empty() {
            match retry_eintr!(self.write_volatile(&partial_buf)) {
                Ok(0) => {
                    return Err(VolatileMemoryError::IOError(std::io::Error::new(
                        ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    )))
                }
                Ok(bytes_written) => partial_buf = partial_buf.offset(bytes_written)?,
                Err(err) => return Err(err),
            }
        }

        Ok(())
    }
}

// We explicitly implement our traits for [`std::fs::File`] and Windows handle types
// instead of providing blanket implementation for [`AsRawHandle`] due to trait coherence limitations: A
// blanket implementation would prevent us from providing implementations for `&mut [u8]` below, as
// "an upstream crate could implement AsRawHandle for &mut [u8]".

macro_rules! impl_read_write_volatile_for_handle {
    ($handle_ty:ty) => {
        impl ReadVolatile for $handle_ty {
            fn read_volatile<B: BitmapSlice>(
                &mut self,
                buf: &mut VolatileSlice<B>,
            ) -> Result<usize, VolatileMemoryError> {
                read_volatile_raw_handle(self.as_raw_handle(), buf)
            }
        }

        impl ReadVolatile for &$handle_ty {
            fn read_volatile<B: BitmapSlice>(
                &mut self,
                buf: &mut VolatileSlice<B>,
            ) -> Result<usize, VolatileMemoryError> {
                read_volatile_raw_handle(self.as_raw_handle(), buf)
            }
        }

        impl ReadVolatile for &mut $handle_ty {
            fn read_volatile<B: BitmapSlice>(
                &mut self,
                buf: &mut VolatileSlice<B>,
            ) -> Result<usize, VolatileMemoryError> {
                read_volatile_raw_handle(self.as_raw_handle(), buf)
            }
        }

        impl WriteVolatile for $handle_ty {
            fn write_volatile<B: BitmapSlice>(
                &mut self,
                buf: &VolatileSlice<B>,
            ) -> Result<usize, VolatileMemoryError> {
                write_volatile_raw_handle(self.as_raw_handle(), buf)
            }
        }

        impl WriteVolatile for &$handle_ty {
            fn write_volatile<B: BitmapSlice>(
                &mut self,
                buf: &VolatileSlice<B>,
            ) -> Result<usize, VolatileMemoryError> {
                write_volatile_raw_handle(self.as_raw_handle(), buf)
            }
        }

        impl WriteVolatile for &mut $handle_ty {
            fn write_volatile<B: BitmapSlice>(
                &mut self,
                buf: &VolatileSlice<B>,
            ) -> Result<usize, VolatileMemoryError> {
                write_volatile_raw_handle(self.as_raw_handle(), buf)
            }
        }
    };
}

impl WriteVolatile for Stdout {
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        write_volatile_raw_handle(self.as_raw_handle(), buf)
    }
}

impl WriteVolatile for &Stdout {
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        write_volatile_raw_handle(self.as_raw_handle(), buf)
    }
}

impl_read_write_volatile_for_handle!(std::fs::File);
impl_read_write_volatile_for_handle!(std::os::windows::io::OwnedHandle);
impl_read_write_volatile_for_handle!(std::os::windows::io::BorrowedHandle<'_>);

// TcpStream uses AsRawSocket instead of AsRawHandle on Windows
impl ReadVolatile for std::net::TcpStream {
    fn read_volatile<B: BitmapSlice>(
        &mut self,
        buf: &mut VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        // On Windows, RawSocket and RawHandle are both usize, so we can cast
        let socket: RawSocket = self.as_raw_socket();
        read_volatile_raw_socket(socket, buf)
    }
}

impl ReadVolatile for &std::net::TcpStream {
    fn read_volatile<B: BitmapSlice>(
        &mut self,
        buf: &mut VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let socket: RawSocket = self.as_raw_socket();
        read_volatile_raw_socket(socket, buf)
    }
}

impl ReadVolatile for &mut std::net::TcpStream {
    fn read_volatile<B: BitmapSlice>(
        &mut self,
        buf: &mut VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let socket: RawSocket = self.as_raw_socket();
        read_volatile_raw_socket(socket, buf)
    }
}

impl WriteVolatile for std::net::TcpStream {
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let socket: RawSocket = self.as_raw_socket();
        write_volatile_raw_socket(socket, buf)
    }
}

impl WriteVolatile for &std::net::TcpStream {
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let socket: RawSocket = self.as_raw_socket();
        write_volatile_raw_socket(socket, buf)
    }
}

impl WriteVolatile for &mut std::net::TcpStream {
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let socket: RawSocket = self.as_raw_socket();
        write_volatile_raw_socket(socket, buf)
    }
}

fn read_volatile_raw_handle(
    handle: RawHandle,
    buf: &mut VolatileSlice<impl BitmapSlice>,
) -> Result<usize, VolatileMemoryError> {
    use windows_sys::Win32::Storage::FileSystem::ReadFile;

    let mut bytes_read = 0u32;
    // Cap the request at u32::MAX to prevent truncation
    let len = buf.len().min(u32::MAX as usize) as u32;
    let guard = buf.ptr_guard_mut();
    let ptr = guard.as_ptr();

    let res = unsafe {
        ReadFile(
            handle,
            ptr as *mut _, // Pass raw pointer
            len,                 // Pass length explicitly
            &mut bytes_read,
            std::ptr::null_mut(),                // No overlapped I/O
        )
    };

    if res != 0 {
        let n = bytes_read as usize;
        buf.bitmap().mark_dirty(0, n);
        Ok(n)
    } else {
        Err(VolatileMemoryError::IOError(std::io::Error::last_os_error()))
    }
}

fn write_volatile_raw_handle(
    handle: RawHandle,
    buf: &VolatileSlice<impl BitmapSlice>,
) -> Result<usize, VolatileMemoryError> {
    use windows_sys::Win32::Storage::FileSystem::WriteFile;

    let mut bytes_written = 0u32;
    // Cap the request at u32::MAX to prevent truncation
    let len = buf.len().min(u32::MAX as usize) as u32;
    let guard = buf.ptr_guard();
    let ptr = guard.as_ptr();

    let res = unsafe {
        WriteFile(
            handle,
            ptr as *const _,
            len,
            &mut bytes_written,
            std::ptr::null_mut(),
        )
    };

    if res != 0 {
        Ok(bytes_written as usize)
    } else {
        Err(VolatileMemoryError::IOError(std::io::Error::last_os_error()))
    }
}

fn read_volatile_raw_socket(
    raw_socket: RawSocket,
    buf: &mut VolatileSlice<impl BitmapSlice>,
) -> Result<usize, VolatileMemoryError> {
    use windows_sys::Win32::Networking::WinSock::{WSARecv, WSABUF, SOCKET};

    let guard = buf.ptr_guard_mut();
    let mut wsa_buf = WSABUF {
        len: buf.len().min(u32::MAX as usize) as u32,
        buf: guard.as_ptr() as *mut u8,
    };
    let mut bytes_received = 0u32;
    let mut flags = 0u32;

    let res = unsafe {
        WSARecv(
            raw_socket as SOCKET,
            &mut wsa_buf,
            1,                  // lpBuffers count: 1 buffer
            &mut bytes_received,
            &mut flags,
            std::ptr::null_mut(),
            None,
        )
    };

    if res == 0 {
        let n = bytes_received as usize;
        buf.bitmap().mark_dirty(0, n);
        Ok(n)
    } else {
        Err(VolatileMemoryError::IOError(std::io::Error::last_os_error()))
    }
}

fn write_volatile_raw_socket(
    socket: RawSocket,
    buf: &VolatileSlice<impl BitmapSlice>,
) -> Result<usize, VolatileMemoryError> {
    use windows_sys::Win32::Networking::WinSock::{WSASend, WSABUF, SOCKET};

    let guard = buf.ptr_guard();
    
    // WSABUF expects a *mut i8 (CHAR*) on Windows for the buffer pointer,
    // even for sending constant data.
    let wsa_buf = WSABUF {
        len: buf.len().min(u32::MAX as usize) as u32,
        buf: guard.as_ptr() as *mut u8,
    };
    
    let mut bytes_sent = 0u32;

    // SAFETY: We pass the raw pointer from guest memory to the Win32 API.
    // We do not create a Rust slice (&[u8]), preserving volatile safety.
    let res = unsafe {
        WSASend(
            socket as SOCKET,
            &wsa_buf as *const WSABUF,
            1,
            &mut bytes_sent,
            0,
            std::ptr::null_mut(),
            None,
        )
    };

    if res == 0 {
        Ok(bytes_sent as usize)
    } else {
        // WSASend returns SOCKET_ERROR (-1) on failure.
        // We retrieve the specific error via last_os_error.
        Err(VolatileMemoryError::IOError(std::io::Error::last_os_error()))
    }
}

impl WriteVolatile for &mut [u8] {
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let total = buf.len().min(self.len());

        // SAFETY:
        // `buf` is contiguously allocated memory of length `total <= buf.len())` by the invariants
        // of `VolatileSlice`.
        // Furthermore, both source and destination of the call to copy_from_volatile_slice are valid
        // for reads and writes respectively of length `total` since total is the minimum of lengths
        // of the memory areas pointed to. The areas do not overlap, since the source is inside guest
        // memory, and the destination is a pointer derived from a slice (no slices to guest memory
        // are possible without violating rust's aliasing rules).
        let written = unsafe { copy_from_volatile_slice(self.as_mut_ptr(), buf, total) };

        // Advance the slice, just like the stdlib: https://doc.rust-lang.org/src/std/io/impls.rs.html#335
        *self = std::mem::take(self).split_at_mut(written).1;

        Ok(written)
    }

    fn write_all_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<(), VolatileMemoryError> {
        // Based on https://github.com/rust-lang/rust/blob/f7b831ac8a897273f78b9f47165cf8e54066ce4b/library/std/src/io/impls.rs#L376-L382
        if self.write_volatile(buf)? == buf.len() {
            Ok(())
        } else {
            Err(VolatileMemoryError::IOError(std::io::Error::new(
                ErrorKind::WriteZero,
                "failed to write whole buffer",
            )))
        }
    }
}

impl ReadVolatile for &[u8] {
    fn read_volatile<B: BitmapSlice>(
        &mut self,
        buf: &mut VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let total = buf.len().min(self.len());

        // SAFETY:
        // `buf` is contiguously allocated memory of length `total <= buf.len())` by the invariants
        // of `VolatileSlice`.
        // Furthermore, both source and destination of the call to copy_to_volatile_slice are valid
        // for reads and writes respectively of length `total` since total is the minimum of lengths
        // of the memory areas pointed to. The areas do not overlap, since the destination is inside
        // guest memory, and the source is a pointer derived from a slice (no slices to guest memory
        // are possible without violating rust's aliasing rules).
        let read = unsafe { copy_to_volatile_slice(buf, self.as_ptr(), total) };

        // Advance the slice, just like the stdlib: https://doc.rust-lang.org/src/std/io/impls.rs.html#232-310
        *self = self.split_at(read).1;

        Ok(read)
    }

    fn read_exact_volatile<B: BitmapSlice>(
        &mut self,
        buf: &mut VolatileSlice<B>,
    ) -> Result<(), VolatileMemoryError> {
        // Based on https://github.com/rust-lang/rust/blob/f7b831ac8a897273f78b9f47165cf8e54066ce4b/library/std/src/io/impls.rs#L282-L302
        if buf.len() > self.len() {
            return Err(VolatileMemoryError::IOError(std::io::Error::new(
                ErrorKind::UnexpectedEof,
                "failed to fill whole buffer",
            )));
        }

        self.read_volatile(buf).map(|_| ())
    }
}

// WriteVolatile implementation for Vec<u8> is based upon the Write impl for Vec, which
// defers to Vec::append_elements, after which the below functionality is modelled.
impl WriteVolatile for Vec<u8> {
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let count = buf.len();
        self.reserve(count);
        let len = self.len();

        // SAFETY: Calling Vec::reserve() above guarantees the the backing storage of the Vec has
        // length at least `len + count`. This means that self.as_mut_ptr().add(len) remains within
        // the same allocated object, the offset does not exceed isize (as otherwise reserve would
        // have panicked), and does not rely on address space wrapping around.
        // In particular, the entire `count` bytes after `self.as_mut_ptr().add(count)` is
        // contiguously allocated and valid for writes.
        // Lastly, `copy_to_volatile_slice` correctly initialized `copied_len` additional bytes
        // in the Vec's backing storage, and we assert this to be equal to `count`. Additionally,
        // `len + count` is at most the reserved capacity of the vector. Thus the call to `set_len`
        // is safe.
        unsafe {
            let copied_len = copy_from_volatile_slice(self.as_mut_ptr().add(len), buf, count);

            assert_eq!(copied_len, count);
            self.set_len(len + count);
        }
        Ok(count)
    }
}

// ReadVolatile and WriteVolatile implementations for Cursor<T> is modelled after the standard
// library's implementation (modulo having to inline `Cursor::remaining_slice`, as that's nightly only)
impl<T> ReadVolatile for Cursor<T>
where
    T: AsRef<[u8]>,
{
    fn read_volatile<B: BitmapSlice>(
        &mut self,
        buf: &mut VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let inner = self.get_ref().as_ref();
        let len = self.position().min(inner.len() as u64);
        let n = ReadVolatile::read_volatile(&mut &inner[(len as usize)..], buf)?;
        self.set_position(self.position() + n as u64);
        Ok(n)
    }

    fn read_exact_volatile<B: BitmapSlice>(
        &mut self,
        buf: &mut VolatileSlice<B>,
    ) -> Result<(), VolatileMemoryError> {
        let inner = self.get_ref().as_ref();
        let n = buf.len();
        let len = self.position().min(inner.len() as u64);
        ReadVolatile::read_exact_volatile(&mut &inner[(len as usize)..], buf)?;
        self.set_position(self.position() + n as u64);
        Ok(())
    }
}

impl WriteVolatile for Cursor<&mut [u8]> {
    fn write_volatile<B: BitmapSlice>(
        &mut self,
        buf: &VolatileSlice<B>,
    ) -> Result<usize, VolatileMemoryError> {
        let pos = self.position().min(self.get_ref().len() as u64);
        let n = WriteVolatile::write_volatile(&mut &mut self.get_mut()[(pos as usize)..], buf)?;
        self.set_position(self.position() + n as u64);
        Ok(n)
    }

    // no write_all provided in standard library, since our default for write_all is based on the
    // standard library's write_all, omitting it here as well will correctly mimic stdlib behavior.
}
