// SPDX-License-Identifier: GPL-3.0-only

//! D-Bus framing of the client to server direction of the port stream.
//!
//! The port is one byte stream that outlives host connections. When the host
//! side reconnects (host process restart, new `Chardev.Register`), a new
//! D-Bus session starts on the same stream with a fresh SASL handshake.
//!
//! * [`Framer`] follows the D-Bus framing (SASL lines, then message headers).
//!   A NUL byte where a new message would start is the first byte of a new
//!   handshake: that is a new session, whatever happened before.
//! * [`Resync`] spots a host that left in the middle of a message and came
//!   back before the port reported it.

use std::borrow::Cow;
use std::time::Duration;

/// Largest D-Bus message (spec limit).
const MAX_MESSAGE: usize = 128 * 1024 * 1024;
/// Silence before a read that a new handshake in the middle of a message
/// needs to be taken as one: a message is written at once, the rest of a
/// message cut by a disconnect never comes.
const HANDSHAKE_GAP: Duration = Duration::from_millis(10);

/// `chunk`, a whole read, starts like a client's handshake: the NUL byte
/// followed by `AUTH `. A client that writes the NUL on its own is told
/// from a lone zero byte of a message body by [`Resync`].
pub(crate) fn starts_handshake(chunk: &[u8]) -> bool {
    chunk.starts_with(b"\0AUTH ")
}

/// Spots a host that left in the middle of a message and came back between
/// two polls of the monitor: its new handshake is not the rest of the old
/// message. It must follow a silence and start a read. A read of a lone NUL
/// is held until the next one tells a handshake (`AUTH `) from body bytes.
#[derive(Default)]
pub(crate) struct Resync {
    held_nul: bool,
}

impl Resync {
    /// What to feed `framer` with for `chunk`, read after `gap`; `None`
    /// holds it.
    pub(crate) fn filter<'a>(
        &mut self,
        framer: &mut Framer,
        gap: Duration,
        chunk: &'a [u8],
    ) -> Option<Cow<'a, [u8]>> {
        if std::mem::take(&mut self.held_nul) {
            if chunk.starts_with(b"AUTH ") {
                tracing::warn!(
                    "new D-Bus handshake in the middle of a message: the host reconnected"
                );
                *framer = Framer::default();
            }
            let mut joined = Vec::with_capacity(chunk.len() + 1);
            joined.push(0);
            joined.extend_from_slice(chunk);
            return Some(Cow::Owned(joined));
        }
        if gap >= HANDSHAKE_GAP && !framer.at_boundary() {
            // Held only while the message needs more bytes anyway: its
            // last byte is never delayed.
            if chunk == [0] && framer.needs_more_than(1) {
                self.held_nul = true;
                return None;
            }
            if starts_handshake(chunk) {
                tracing::warn!(
                    "new D-Bus handshake in the middle of a message: the host reconnected"
                );
                *framer = Framer::default();
            }
        }
        Some(Cow::Borrowed(chunk))
    }
}

pub(crate) enum FrameOut<'a> {
    NewSession,
    Bytes(&'a [u8]),
}

/// Follows the client to server direction of a D-Bus stream.
#[derive(Default)]
pub(crate) enum Framer {
    /// Out of sync: discard until a NUL byte starts a handshake.
    #[default]
    Idle,
    /// SASL lines, until `BEGIN`.
    Auth { line: Vec<u8> },
    /// Collecting the 16 byte fixed header of the next message.
    Header { buf: [u8; 16], have: usize },
    /// Inside a message body.
    Body { remaining: usize },
}

impl Framer {
    pub(crate) fn feed<'a>(&mut self, mut data: &'a [u8], mut out: impl FnMut(FrameOut<'a>)) {
        while !data.is_empty() {
            match self {
                Framer::Idle => match data.iter().position(|&b| b == 0) {
                    Some(i) => {
                        data = &data[i..];
                        self.start_session(&mut data, &mut out);
                    }
                    None => return,
                },
                Framer::Auth { line } => {
                    // Process up to and including the next '\n'.
                    let end = data.iter().position(|&b| b == b'\n').map(|i| i + 1);
                    let take = end.unwrap_or(data.len());
                    if line.is_empty() && data[0] == 0 {
                        self.start_session(&mut data, &mut out);
                        continue;
                    }
                    line.extend_from_slice(&data[..take]);
                    out(FrameOut::Bytes(&data[..take]));
                    data = &data[take..];
                    if end.is_some() {
                        let begin = line.as_slice() == b"BEGIN\r\n";
                        line.clear();
                        if begin {
                            *self = Framer::Header {
                                buf: [0; 16],
                                have: 0,
                            };
                        }
                    } else if line.len() > 4096 {
                        tracing::warn!("SASL line too long, resyncing");
                        *self = Framer::Idle;
                    }
                }
                Framer::Header { buf, have } => {
                    if *have == 0 && data[0] == 0 {
                        self.start_session(&mut data, &mut out);
                        continue;
                    }
                    let take = (16 - *have).min(data.len());
                    buf[*have..*have + take].copy_from_slice(&data[..take]);
                    *have += take;
                    out(FrameOut::Bytes(&data[..take]));
                    data = &data[take..];
                    if *have == 16 {
                        *self = match message_len(buf) {
                            Some(total) => Framer::Body {
                                remaining: total - 16,
                            },
                            None => {
                                tracing::warn!("invalid D-Bus header, resyncing");
                                Framer::Idle
                            }
                        };
                    }
                }
                Framer::Body { remaining } => {
                    let take = (*remaining).min(data.len());
                    out(FrameOut::Bytes(&data[..take]));
                    data = &data[take..];
                    *remaining -= take;
                    if *remaining == 0 {
                        *self = Framer::Header {
                            buf: [0; 16],
                            have: 0,
                        };
                    }
                }
            }
        }
    }

    /// Between two messages or SASL lines, or out of sync: a NUL byte there
    /// is a handshake anyway.
    /// More than `n` bytes are still needed to end the current message or
    /// SASL line.
    fn needs_more_than(&self, n: usize) -> bool {
        match self {
            Framer::Idle => false,
            Framer::Auth { line } => !line.is_empty(), // a line ends with CRLF
            Framer::Header { .. } => true,             // a body follows
            Framer::Body { remaining } => *remaining > n,
        }
    }

    pub(crate) fn at_boundary(&self) -> bool {
        match self {
            Framer::Idle => true,
            Framer::Auth { line } => line.is_empty(),
            Framer::Header { have, .. } => *have == 0,
            Framer::Body { .. } => false,
        }
    }

    /// `data[0]` is the NUL that opens a handshake.
    fn start_session<'a>(&mut self, data: &mut &'a [u8], out: &mut impl FnMut(FrameOut<'a>)) {
        out(FrameOut::NewSession);
        out(FrameOut::Bytes(&data[..1]));
        *data = &data[1..];
        *self = Framer::Auth { line: Vec::new() };
    }
}

/// Total length of a D-Bus message from its fixed header.
fn message_len(h: &[u8; 16]) -> Option<usize> {
    let read = |b: [u8; 4]| match h[0] {
        b'l' => Some(u32::from_le_bytes(b)),
        b'B' => Some(u32::from_be_bytes(b)),
        _ => None,
    };
    if h[3] != 1 {
        return None; // protocol version
    }
    let body = read(h[4..8].try_into().unwrap())? as usize;
    let fields = read(h[12..16].try_into().unwrap())? as usize;
    let header = (16 + fields + 7) & !7;
    let total = header.checked_add(body)?;
    (total <= MAX_MESSAGE).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(framer: &mut Framer, data: &[u8]) -> (usize, Vec<u8>) {
        let mut sessions = 0;
        let mut bytes = Vec::new();
        framer.feed(data, |o| match o {
            FrameOut::NewSession => sessions += 1,
            FrameOut::Bytes(b) => bytes.extend_from_slice(b),
        });
        (sessions, bytes)
    }

    fn message(body: usize) -> Vec<u8> {
        // 'l', type, flags, version, body len, serial, fields len (8)
        let mut m = vec![b'l', 1, 0, 1];
        m.extend_from_slice(&(body as u32).to_le_bytes());
        m.extend_from_slice(&1u32.to_le_bytes());
        m.extend_from_slice(&8u32.to_le_bytes());
        m.extend_from_slice(&[0xAA; 8]); // fields
        m.extend(std::iter::repeat_n(0xBB, body));
        m
    }

    const HELLO: &[u8] = b"\0AUTH ANONYMOUS 6162\r\nBEGIN\r\n";

    #[test]
    fn handshake_then_messages() {
        let mut f = Framer::default();
        let mut stream = b"garbage".to_vec();
        stream.extend_from_slice(HELLO);
        stream.extend(message(5));
        stream.extend(message(0));
        let (sessions, bytes) = run(&mut f, &stream);
        assert_eq!(sessions, 1);
        assert_eq!(bytes, stream[7..]);
        assert!(matches!(f, Framer::Header { have: 0, .. }));
    }

    #[test]
    fn new_handshake_between_messages() {
        let mut f = Framer::default();
        let mut stream = HELLO.to_vec();
        stream.extend(message(3));
        stream.extend_from_slice(HELLO);
        stream.extend(message(1));
        let (sessions, _) = run(&mut f, &stream);
        assert_eq!(sessions, 2);
    }

    #[test]
    fn handshake_after_a_cut_message() {
        // The host left after half of a message and came back: what the
        // reader does with a read that starts a handshake after a silence.
        let mut f = Framer::default();
        let mut stream = HELLO.to_vec();
        let m = message(100);
        stream.extend_from_slice(&m[..60]);
        assert_eq!(run(&mut f, &stream).0, 1);
        assert!(!f.at_boundary());
        assert!(starts_handshake(HELLO));
        assert!(!starts_handshake(b"\0"));
        assert!(!starts_handshake(&m[..10]));
        f = Framer::default();
        let (sessions, bytes) = run(&mut f, HELLO);
        assert_eq!(sessions, 1);
        assert_eq!(bytes, HELLO);
        // Without the reset, the handshake is swallowed by the old body.
        let mut g = Framer::default();
        run(&mut g, &stream);
        assert_eq!(run(&mut g, HELLO).0, 0);
    }

    #[test]
    fn lone_nul_after_a_silence() {
        let silence = HANDSHAKE_GAP * 2;
        let mut m = message(100); // 124 bytes
        m[60] = 0;
        m[123] = 0;
        let mid = |f: &mut Framer| {
            let mut stream = HELLO.to_vec();
            stream.extend_from_slice(&m[..60]);
            run(f, &stream);
        };

        // A zero byte of the body, read alone: held, then fed with the
        // rest of the message, which still ends.
        let mut f = Framer::default();
        let mut r = Resync::default();
        mid(&mut f);
        assert!(r.filter(&mut f, silence, &[0]).is_none());
        let rest = r.filter(&mut f, Duration::ZERO, &m[61..]).unwrap();
        assert_eq!(rest[0], 0);
        assert_eq!(run(&mut f, &rest).0, 0);
        assert!(f.at_boundary());

        // A client writing its NUL alone, then AUTH: a new session.
        let mut f = Framer::default();
        let mut r = Resync::default();
        mid(&mut f);
        assert!(r.filter(&mut f, silence, &HELLO[..1]).is_none());
        let next = r.filter(&mut f, Duration::ZERO, &HELLO[1..]).unwrap();
        assert_eq!(run(&mut f, &next).0, 1);

        // The last byte of a message is never held.
        let mut f = Framer::default();
        let mut r = Resync::default();
        mid(&mut f);
        run(&mut f, &m[60..123]);
        let last = r.filter(&mut f, silence, &m[123..]).unwrap();
        run(&mut f, &last);
        assert!(f.at_boundary());
    }

    #[test]
    fn byte_by_byte() {
        let mut f = Framer::default();
        let mut stream = HELLO.to_vec();
        stream.extend(message(9));
        stream.extend_from_slice(HELLO);
        let mut sessions = 0;
        for b in &stream {
            sessions += run(&mut f, std::slice::from_ref(b)).0;
        }
        assert_eq!(sessions, 2);
    }
}
