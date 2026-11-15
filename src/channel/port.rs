// SPDX-License-Identifier: GPL-3.0-only

//! virtio-serial port I/O: `/dev/virtio-ports/<name>`.
//!
//! The Linux virtio console driver reports the host's side of the port:
//! while the host is not connected, `poll` returns `POLLHUP` and `read`
//! returns 0 (end of file), and a read blocked while the host goes away
//! returns 0 too. Writes block (here: `EAGAIN`) until the host comes back.
//! So the reader sees a disconnect as the first end of file after data, and
//! resets the [`Framer`]: a message cut in the middle cannot swallow the next
//! handshake. [`Resync`] covers a host that came back between two reads.
//!
//! The port can be opened once only, so the reader and the writer threads
//! share one non-blocking descriptor.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{Mode, OFlags, open};
use rustix::io::Errno;
use tokio::sync::mpsc;

use crate::channel::framer::{FrameOut, Framer, Resync};

const READ_CHUNK: usize = 64 * 1024;
/// How often a disconnected port is looked at again, and how long a blocked
/// write waits before it checks that its session still exists.
const IDLE_PERIOD: Duration = Duration::from_millis(200);

/// What the reader thread reports to the session manager.
#[derive(Debug)]
pub enum PortEvent {
    /// A new SASL handshake started: drop the previous session.
    NewSession,
    /// Bytes for the current session.
    Data(Vec<u8>),
    /// The host closed its end of the port.
    HostDisconnected,
}

/// Bytes the current session wants to write, tagged with its generation so
/// a late write from a dead session is never sent to the next one.
pub type Outgoing = (u64, Vec<u8>);

pub struct Port {
    fd: OwnedFd,
    /// Generation of the session allowed to write.
    pub generation: AtomicU64,
}

pub fn path(name: &str) -> PathBuf {
    PathBuf::from("/dev/virtio-ports").join(name)
}

impl Port {
    /// Opens the port and starts the reader and writer threads.
    pub fn open(
        name: &str,
    ) -> io::Result<(
        Arc<Port>,
        mpsc::UnboundedReceiver<PortEvent>,
        mpsc::Sender<Outgoing>,
    )> {
        let fd = open(
            path(name),
            OFlags::RDWR | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )?;
        let port = Arc::new(Port {
            fd,
            generation: AtomicU64::new(0),
        });
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::channel::<Outgoing>(64);
        {
            let port = port.clone();
            std::thread::Builder::new()
                .name("port-reader".into())
                .spawn(move || port.reader(ev_tx))?;
        }
        {
            let port = port.clone();
            std::thread::Builder::new()
                .name("port-writer".into())
                .spawn(move || port.writer(out_rx))?;
        }
        Ok((port, ev_rx, out_tx))
    }

    /// Waits for `flags` on the port, at most `timeout`. Returns the
    /// events seen (empty on timeout).
    fn wait(&self, flags: PollFlags, timeout: Duration) -> io::Result<PollFlags> {
        let mut fds = [PollFd::new(&self.fd, flags)];
        let ts = Timespec::try_from(timeout).unwrap_or(Timespec {
            tv_sec: 0,
            tv_nsec: 200_000_000,
        });
        match poll(&mut fds, Some(&ts)) {
            Ok(_) => Ok(fds[0].revents()),
            Err(Errno::INTR) => Ok(PollFlags::empty()),
            Err(e) => Err(e.into()),
        }
    }

    fn reader(&self, events: mpsc::UnboundedSender<PortEvent>) {
        let mut buf = vec![0u8; READ_CHUNK];
        let mut framer = Framer::default();
        let mut resync = Resync::default();
        let mut connected = false;
        let mut last_read = Instant::now();
        loop {
            if events.is_closed() {
                return;
            }
            match rustix::io::read(self.fd.as_fd(), &mut buf) {
                Ok(0) => {
                    // End of file: the host is not connected.
                    if connected {
                        connected = false;
                        tracing::info!("host disconnected from the port");
                        framer = Framer::default();
                        resync = Resync::default();
                        if events.send(PortEvent::HostDisconnected).is_err() {
                            return;
                        }
                    }
                    // POLLHUP stays set while the host is away: poll would
                    // return at once, sleep instead.
                    std::thread::sleep(IDLE_PERIOD);
                }
                Ok(n) => {
                    if !connected {
                        connected = true;
                        tracing::info!("host connected to the port");
                    }
                    let gap = last_read.elapsed();
                    last_read = Instant::now();
                    let Some(chunk) = resync.filter(&mut framer, gap, &buf[..n]) else {
                        continue;
                    };
                    let mut pending: Vec<u8> = Vec::new();
                    framer.feed(&chunk, |out| match out {
                        FrameOut::NewSession => {
                            if !pending.is_empty() {
                                let _ = events.send(PortEvent::Data(std::mem::take(&mut pending)));
                            }
                            let _ = events.send(PortEvent::NewSession);
                        }
                        FrameOut::Bytes(b) => pending.extend_from_slice(b),
                    });
                    if !pending.is_empty() && events.send(PortEvent::Data(pending)).is_err() {
                        return;
                    }
                }
                Err(Errno::AGAIN) | Err(Errno::INTR) => {
                    if let Err(e) = self.wait(PollFlags::IN, IDLE_PERIOD) {
                        tracing::error!("virtio-serial poll failed: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
                Err(e) => {
                    tracing::error!("virtio-serial read failed: {e}");
                    std::thread::sleep(Duration::from_secs(1));
                }
            }
        }
    }

    fn writer(&self, mut out: mpsc::Receiver<Outgoing>) {
        while let Some((generation, data)) = out.blocking_recv() {
            let mut pos = 0;
            while pos < data.len() {
                if generation != self.generation.load(Ordering::Acquire) {
                    break; // the session that queued this is gone
                }
                match rustix::io::write(self.fd.as_fd(), &data[pos..]) {
                    Ok(n) => pos += n,
                    Err(Errno::AGAIN) | Err(Errno::INTR) => {
                        // Queue full or host away: wait, then check the
                        // generation again.
                        match self.wait(PollFlags::OUT, IDLE_PERIOD) {
                            // POLLHUP returns at once while the host is away.
                            Ok(ev) if ev.contains(PollFlags::HUP) => {
                                std::thread::sleep(IDLE_PERIOD)
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!("virtio-serial poll failed: {e}");
                                std::thread::sleep(IDLE_PERIOD);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("virtio-serial write failed: {e}");
                        break;
                    }
                }
            }
        }
    }
}
