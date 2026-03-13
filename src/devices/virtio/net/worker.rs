use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::windows::io::AsRawHandle;
use std::process::{ChildStdin, ChildStdout};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use tracing::{error, trace};
use windows::Win32::Foundation::{HANDLE, WAIT_FAILED, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};
use windows::Win32::System::Pipes::PeekNamedPipe;

use crate::devices::event::WindowsEvent;
use crate::devices::virtio::descriptor_utils::{Reader, Writer};
use crate::devices::virtio::device::DeviceQueue;
use crate::devices::virtio::mmio::InterruptTransport;
use crate::devices::virtio::net::gvproxy::GvProxy;
use crate::memory::memory::MemoryManager;

// ── constants ───────────────────────────────────────────────────────────

/// Size of the virtio_net_hdr (modern, with `num_buffers`).
const NET_HDR_SIZE: usize = 12;

/// Maximum ethernet frame size (no TSO/UFO negotiated).
const MAX_FRAME_SIZE: usize = 1514;

/// Number of packets to read in a single batch. 
/// A batch of 32 or 64 packets usually fits well within the CPU's L1/L2 cache.
const BATCH_SIZE: usize = 32;

/// Build a default RX `virtio_net_hdr`.
///
/// Without VIRTIO_NET_F_GUEST_CSUM → flags = 0, gso_type = GSO_NONE.
/// Without VIRTIO_NET_F_MRG_RXBUF  → num_buffers = 1.
fn default_rx_header() -> [u8; NET_HDR_SIZE] {
    let mut hdr = [0u8; NET_HDR_SIZE];
    // num_buffers = 1 (le16 at offset 10)
    hdr[10] = 1;
    hdr
}

// ── worker ──────────────────────────────────────────────────────────────

pub struct NetWorker {
    /// receiveq — device writes incoming packets here for the guest.
    rx_queue: DeviceQueue,
    /// transmitq — guest places outgoing packets here for the device.
    tx_queue: DeviceQueue,
    interrupt: InterruptTransport,
    mem: MemoryManager,
    stop_event: Arc<WindowsEvent>,

    /// gvproxy process — kept alive so Drop kills the child.
    gvproxy: GvProxy,
    /// Stdin handle for writing TX frames to gvproxy.  Taken from gvproxy
    /// once in `run()` and reused on every TX event.
    gvproxy_stdin: Option<ChildStdin>,
    /// Frames read from gvproxy by the reader thread, pending delivery.
    rx_pending: Arc<Mutex<VecDeque<Vec<u8>>>>,
    /// Signaled by the reader thread when frames are available.
    rx_ready: Arc<WindowsEvent>,
}

impl NetWorker {
    pub fn new(
        rx_queue: DeviceQueue,
        tx_queue: DeviceQueue,
        interrupt: InterruptTransport,
        mem: MemoryManager,
        stop_event: Arc<WindowsEvent>,
        gvproxy: GvProxy,
    ) -> Self {
        Self {
            rx_queue,
            tx_queue,
            interrupt,
            mem,
            stop_event,
            gvproxy,
            gvproxy_stdin: None,
            rx_pending: Arc::new(Mutex::new(VecDeque::new())),
            rx_ready: Arc::new(WindowsEvent::new().unwrap()),
        }
    }

    fn read_frame(stdout: &mut ChildStdout) -> Result<Vec<u8>, io::Error> {
        // 2-byte little-endian length prefix.
        let mut len_buf = [0u8; 2];
        if let Err(e) = stdout.read_exact(&mut len_buf) {
            return Err(e);
        }

        let len = u16::from_le_bytes(len_buf) as usize;
        // this should never happen as gvproxy default mtu value is 1500
        if len == 0 || len > MAX_FRAME_SIZE {
            tracing::warn!("virtio_net rx reader: bad frame length {len}");
            // For a zero-length frame we can just skip.
            // For an oversized value the stream is likely corrupt — bail.
            // We negotiate a maximum frame size with the guest, so there is no reason to keep it.
            // it would be discarded anyway.
            if len > MAX_FRAME_SIZE {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length too large"));
            }
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length too small"));
        }

        let mut frame = vec![0u8; len];
        stdout.read_exact(&mut frame)?;
        Ok(frame)
    }

    /// Background loop: reads length-prefixed ethernet frames from gvproxy
    /// (2-byte LE length + frame) and queues them for the main worker.
    /// It uses an opportinistic batching strategy to optimizing performance.
    /// It reads the first frame (blocking) and then verifies if there are more frames available.
    /// It keeps looping until there are no more frames available or the batch size is reached.
    fn rx_reader_loop(
        mut stdout: ChildStdout,
        pending: Arc<Mutex<VecDeque<Vec<u8>>>>,
        ready: Arc<WindowsEvent>,
    ) {
        loop {
            let mut n_frame = 0;
            let frame = match Self::read_frame(&mut stdout) {
                Ok(frame) => frame,
                Err(e) => {
                    if e.kind() == io::ErrorKind::UnexpectedEof {
                        tracing::debug!("virtio_net rx reader: EOF");
                        break;
                    }
                    continue;
                }
            };

            n_frame += 1;

            trace!("virtio_net rx: received {} B frame from gvproxy", frame.len());
            pending.lock().unwrap().push_back(frame);

            // before signaling, we check if there are any more frames to read
            let mut bytes_available: u32 = 0;
            loop {
                unsafe {
                    let _ = PeekNamedPipe(HANDLE(stdout.as_raw_handle()), None, 0, None, Some(&mut bytes_available), None);
                }
                if bytes_available == 0 || n_frame >= BATCH_SIZE {
                    break;
                }

                let frame = match Self::read_frame(&mut stdout) {
                    Ok(frame) => frame,
                    Err(e) => {
                        if e.kind() == io::ErrorKind::UnexpectedEof {
                            tracing::debug!("virtio_net rx reader: EOF");
                            break;
                        }
                        continue;
                    }
                };

                trace!("virtio_net rx: received {} B frame from gvproxy", frame.len());
                pending.lock().unwrap().push_back(frame);

                n_frame += 1;
                bytes_available = 0;
                
            }

            let _ = ready.signal();
        }
    }

    pub fn run(mut self) -> JoinHandle<()> {
        // Take stdout out of gvproxy *before* moving self into the worker
        // closure — the reader thread gets stdout, the worker keeps gvproxy
        // (stdin) for TX writes.
        let stdout = self.gvproxy.stdout()
            .expect("gvproxy stdout already taken");
        self.gvproxy_stdin = Some(self.gvproxy.stdin()
            .expect("gvproxy stdin already taken"));
        let reader_pending = Arc::clone(&self.rx_pending);
        let reader_ready = Arc::clone(&self.rx_ready);

        let rx_reader_thread = thread::Builder::new()
            .name("net rx reader".into())
            .spawn(move || Self::rx_reader_loop(stdout, reader_pending, reader_ready))
            .unwrap();

        thread::Builder::new()
            .name("net worker".into())
            .spawn(move || self.work(rx_reader_thread))
            .unwrap()
    }

    fn work(mut self, rx_reader_thread: JoinHandle<()>) {
        let handles: [HANDLE; 3] = [
            HANDLE(self.tx_queue.event.as_raw_handle()),
            HANDLE(self.rx_ready.as_raw_handle()),
            HANDLE(self.stop_event.as_raw_handle()),
        ];

        loop {
            let result = unsafe {
                WaitForMultipleObjects(&handles, false, INFINITE)
            };

            match result {
                // tx queue event — guest transmitted packets
                r if r == WAIT_OBJECT_0 => {
                    self.process_tx_virtio_queue();
                }
                // rx ready — reader thread has frames for the guest
                r if r.0 == WAIT_OBJECT_0.0 + 1 => {
                    self.process_rx_virtio_queue();
                }
                // stop event
                r if r.0 == WAIT_OBJECT_0.0 + 2 => {
                    tracing::debug!("net: stopping worker thread");
                    break;
                }
                _ if result == WAIT_FAILED => {
                    let err = io::Error::last_os_error();
                    error!("net: worker loop wait failed: {err}");
                    break;
                }
                _ => {
                    tracing::warn!("net: unexpected wait result: {result:?}");
                }
            }
        }

        // Drop gvproxy (kills the process) so the blocking reader thread
        // sees EOF on stdout and exits.
        drop(self.gvproxy);
        let _ = rx_reader_thread.join();
    }

    // ── TX: guest → gvproxy ─────────────────────────────────────────

    /// Drain the transmit queue with notification suppression.
    fn process_tx_virtio_queue(&mut self) {
        let mem = self.mem.clone();
        loop {
            self.tx_queue.queue.disable_notification(&mem).unwrap();
            self.process_tx_queue(&mem);
            if !self.tx_queue.queue.enable_notification(&mem).unwrap() {
                break;
            }
        }
    }

    /// Process every pending descriptor chain on the transmit queue.
    ///
    /// Each chain carries a 12-byte `virtio_net_hdr` followed by the raw
    /// ethernet frame.  We strip the header and send the frame to gvproxy
    /// as a 2-byte LE length prefix + frame bytes.
    fn process_tx_queue(&mut self, mem: &MemoryManager) {
        let mut used_any = false;
        let Some(stdin) = self.gvproxy_stdin.as_mut() else {
            error!("virtio_net tx: gvproxy stdin not available");
            return;
        };
        while let Some(head) = self.tx_queue.queue.pop(mem) {
            let head_index = head.index;

            let mut reader = match Reader::new(mem, head) {
                Ok(r) => r,
                Err(e) => {
                    error!("virtio_net tx: invalid descriptor chain: {e:?}");
                    continue;
                }
            };

            let avail = reader.available_bytes();
            if avail > NET_HDR_SIZE {
                let mut buf = vec![0u8; avail];
                match reader.read(&mut buf) {
                    Ok(n) if n > NET_HDR_SIZE => {
                        let frame = &buf[NET_HDR_SIZE..n];
                        trace!(
                            "virtio_net tx: sending {} B frame to gvproxy",
                            frame.len()
                        );
                        let len_prefix = (frame.len() as u16).to_le_bytes();
                        if let Err(e) = stdin.write_all(&len_prefix)
                            .and_then(|_| stdin.write_all(frame))
                            .and_then(|_| stdin.flush())
                        {
                            error!("virtio_net tx: write to gvproxy failed: {e}");
                        }
                    }
                    Ok(n) => {
                        tracing::warn!(
                            "virtio_net tx: packet too short ({n} B, need > {NET_HDR_SIZE})"
                        );
                    }
                    Err(e) => {
                        error!("virtio_net tx: failed to read descriptor: {e:?}");
                    }
                }
            } else if avail > 0 {
                tracing::warn!("virtio_net tx: descriptor too small ({avail} B)");
            }

            // Return the descriptor; len = 0 (device writes nothing for tx).
            if let Err(e) = self.tx_queue.queue.add_used(mem, head_index, 0) {
                error!("virtio_net tx: failed to add used: {e:?}");
            }
            used_any = true;
        }

        if used_any {
            if let Err(e) = self.interrupt.try_signal_used_queue() {
                error!("virtio_net tx: failed to signal queue: {e:?}");
            }
        }
    }

    // ── RX: gvproxy → guest ─────────────────────────────────────────

    /// Deliver frames from the reader thread into the guest's receiveq.
    ///
    /// Each raw ethernet frame is prefixed with a 12-byte `virtio_net_hdr`
    /// (flags=0, gso_type=NONE, num_buffers=1) before being written into
    /// the guest buffer.
    fn process_rx_virtio_queue(&mut self) {
        let mut packets: VecDeque<Vec<u8>> = {
            self.rx_pending.lock().unwrap().drain(..).collect()
        };

        if packets.is_empty() {
            return;
        }

        let mem = &self.mem;
        let mut used_any = false;

        while let Some(frame) = packets.pop_front() {
            // this should never happen, but we check just in case.
            if frame.len() > MAX_FRAME_SIZE {
                tracing::warn!(
                    "virtio_net rx: dropping oversized frame ({} B, max {MAX_FRAME_SIZE})",
                    frame.len()
                );
                continue;
            }

            let Some(head) = self.rx_queue.queue.pop(mem) else {
                // Put the current frame back and re-queue everything still
                // pending so it can be delivered once the guest posts new
                // receive buffers.
                packets.push_front(frame);
                let requeued = packets.len();
                tracing::warn!(
                    "virtio_net rx: no buffers available, re-queuing {requeued} frame(s)",
                );
                self.rx_pending.lock().unwrap().extend(packets);
                break;
            };
            let head_index = head.index;

            let mut writer = match Writer::new(mem, head) {
                Ok(w) => w,
                Err(e) => {
                    error!("virtio_net rx: invalid descriptor chain: {e:?}");
                    continue;
                }
            };

            let total = NET_HDR_SIZE + frame.len();
            if writer.available_bytes() < total {
                tracing::warn!(
                    "virtio_net rx: buffer too small ({} < {total})",
                    writer.available_bytes()
                );
                if let Err(e) = self.rx_queue.queue.add_used(mem, head_index, 0) {
                    error!("virtio_net rx: failed to add used: {e:?}");
                }
                continue;
            }

            let header = default_rx_header();
            let mut written = 0u32;

            match writer.write_all(&header) {
                Ok(()) => written += NET_HDR_SIZE as u32,
                Err(e) => {
                    error!("virtio_net rx: failed to write header: {e:?}");
                    if let Err(e2) = self.rx_queue.queue.add_used(mem, head_index, 0) {
                        error!("virtio_net rx: failed to add used: {e2:?}");
                    }
                    continue;
                }
            }

            match writer.write_all(&frame) {
                Ok(()) => written += frame.len() as u32,
                Err(e) => {
                    error!("virtio_net rx: failed to write frame: {e:?}");
                }
            }

            if let Err(e) = self.rx_queue.queue.add_used(mem, head_index, written) {
                error!("virtio_net rx: failed to add used: {e:?}");
            }
            used_any = true;
        }

        if used_any && self.rx_queue.queue.needs_notification(mem).unwrap() {
            if let Err(e) = self.interrupt.try_signal_used_queue() {
                error!("virtio_net rx: failed to signal queue: {e:?}");
            }
        }
    }
}
