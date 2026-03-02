use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows::Win32::Security::SECURITY_ATTRIBUTES;

pub struct WindowsEvent {
    handle: HANDLE,
}

// Safety: Windows handles are safe to transfer between threads 
// and can be signaled from multiple threads simultaneously.
unsafe impl Send for WindowsEvent {}
unsafe impl Sync for WindowsEvent {}

impl WindowsEvent {
    pub fn new() -> io::Result<Self> {
        let handle = unsafe { 
            CreateEventW(
                None, // SECURITY_ATTRIBUTES pointer, or None
                false,         // bManualReset: auto-reset
                false,         // bInitialState: non-signaled
                None           // lpName: unnamed
            )
        }.map_err(|e| io::Error::from_raw_os_error(e.code().0))?;
        if handle.is_invalid() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { handle })
    }

    pub fn signal(&self) -> io::Result<()> {
        unsafe { SetEvent(self.handle) }.map_err(|_| io::Error::last_os_error())
    }

    pub fn wait(&self, timeout_ms: u32) -> bool {
        let res = unsafe { WaitForSingleObject(self.handle, timeout_ms) };
        res == WAIT_OBJECT_0
    }
}

impl Drop for WindowsEvent {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

// 1. Implement AsRawHandle for your struct
impl AsRawHandle for WindowsEvent {
    fn as_raw_handle(&self) -> RawHandle {
        self.handle.0 as RawHandle
    }
}