// SPDX-License-Identifier: GPL-3.0-only

//! Client cameras as PipeWire cameras.
//!
//! Each client camera becomes a PipeWire node of class `Video/Source` and
//! role `Camera`, named after the client camera: the apps that use cameras
//! through PipeWire or the camera portal (browsers, OBS, GNOME Snapshot)
//! list it. The node offers the formats of `AddCamera` as raw I420 video.
//! When an app starts it, the agent tells the host which format it chose
//! (`StreamRequested`); when it stops, `StreamStopped`.
//!
//! The host's H.264 stream is decoded in the guest by the system's Cisco
//! OpenH264 library (`libopenh264`, the `openh264` package), loaded at run
//! time: no codec is built into the agent. Each decoded picture goes to the
//! PipeWire thread, which drives the node at the client's frame rate. When
//! the decoder lost track (it started on a frame that is not an IDR, or a
//! frame was dropped), the agent asks for a key frame, at most once a
//! second.
//!
//! Two threads: PipeWire's main loop, which owns the nodes, and the
//! decoder, so that a slow decode never stalls PipeWire.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::formats::YUVSource;
use pipewire as pw;
use pw::spa;
use tokio::sync::{mpsc, oneshot};

/// `(width, height, fps numerator, fps denominator)`, as in `AddCamera`.
pub type Format = (u32, u32, u32, u32);

/// Frames waiting for the decoder. A slow decoder drops the rest and asks
/// for an IDR to start over cleanly.
const FRAME_QUEUE: usize = 8;
/// At most one key frame request per camera in this interval.
const KEY_REQUEST_INTERVAL: Duration = Duration::from_secs(1);
/// The names the system's OpenH264 library goes by.
const OPENH264: &[&str] = &["libopenh264.so.8", "libopenh264.so.7", "libopenh264.so"];

/// What the cameras tell the host.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// An app started camera `id` in `format` (index into its formats), or
    /// the last app stopped it (`None`).
    Stream { id: u32, format: Option<u32> },
    /// The decoder needs an IDR.
    KeyFrame { id: u32 },
    /// An app changed a control: MS-RDPECAM numbers, see
    /// `proto::camera_control`.
    Control {
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    },
}

/// To the PipeWire thread.
enum Pw {
    Add {
        id: u32,
        name: String,
        formats: Vec<Format>,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Remove {
        id: u32,
    },
    RemoveAll,
    /// The client's controls of camera `id`, replacing the previous ones.
    Controls {
        id: u32,
        controls: Vec<Control>,
    },
    /// What a control is now.
    ControlValue {
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    },
    /// A decoded picture, I420, packed.
    Picture {
        id: u32,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
}

/// To the decoder thread.
enum Dec {
    Frame {
        id: u32,
        data: Vec<u8>,
    },
    /// A stream (re)started: start over from the next IDR.
    Restart {
        id: u32,
    },
    Remove {
        id: u32,
    },
}

#[derive(Clone)]
pub struct Cameras {
    pw: pw::channel::Sender<Pw>,
    dec: std_mpsc::SyncSender<Dec>,
    /// Frames dropped so far: the decoders must restart from an IDR.
    dropped: Arc<AtomicU64>,
    /// The OpenH264 library could not be loaded.
    no_decoder: Option<String>,
}

impl Cameras {
    /// Starts the PipeWire and decoder threads.
    pub fn spawn(events: mpsc::UnboundedSender<Event>) -> anyhow::Result<Self> {
        let no_decoder = load_openh264().err();
        if let Some(e) = &no_decoder {
            tracing::warn!("no H.264 decoder, client cameras disabled: {e}");
        }
        let (pw_tx, pw_rx) = pw::channel::channel();
        let (dec_tx, dec_rx) = std_mpsc::sync_channel(FRAME_QUEUE);
        let dropped = Arc::new(AtomicU64::new(0));
        let (ready_tx, ready_rx) = std_mpsc::channel();
        {
            let events = events.clone();
            let dec_tx = dec_tx.clone();
            std::thread::Builder::new()
                .name("pipewire".into())
                .spawn(move || {
                    if let Err(e) = pw_thread(pw_rx, dec_tx, events, &ready_tx) {
                        let _ = ready_tx.send(Err(e.to_string()));
                    }
                })?;
        }
        ready_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("PipeWire thread gone"))?
            .map_err(|e| anyhow::anyhow!("PipeWire: {e}"))?;
        {
            let pw_tx = pw_tx.clone();
            let dropped = dropped.clone();
            std::thread::Builder::new()
                .name("camera-decode".into())
                .spawn(move || decode_thread(dec_rx, pw_tx, events, &dropped))?;
        }
        Ok(Self {
            pw: pw_tx,
            dec: dec_tx,
            dropped,
            no_decoder,
        })
    }

    pub async fn add(&self, id: u32, name: String, formats: Vec<Format>) -> Result<(), String> {
        if let Some(e) = &self.no_decoder {
            return Err(format!("no H.264 decoder (install openh264): {e}"));
        }
        let (reply, rx) = oneshot::channel();
        self.pw
            .send(Pw::Add {
                id,
                name,
                formats,
                reply,
            })
            .map_err(|_| "PipeWire thread gone".to_string())?;
        rx.await.map_err(|_| "PipeWire thread gone".to_string())?
    }

    pub fn remove(&self, id: u32) {
        let _ = self.pw.send(Pw::Remove { id });
        let _ = self.dec.send(Dec::Remove { id });
    }

    pub fn remove_all(&self) {
        let _ = self.pw.send(Pw::RemoveAll);
    }

    pub fn set_controls(&self, id: u32, controls: Vec<Control>) {
        let _ = self.pw.send(Pw::Controls { id, controls });
    }

    pub fn control_value(&self, id: u32, set: u8, property: u8, mode: u8, value: i32) {
        let _ = self.pw.send(Pw::ControlValue {
            id,
            set,
            property,
            mode,
            value,
        });
    }

    /// Never blocks: a full queue drops the frame.
    pub fn push(&self, id: u32, data: Vec<u8>) {
        if self.dec.try_send(Dec::Frame { id, data }).is_err() {
            self.dropped.fetch_add(1, Ordering::AcqRel);
            tracing::debug!(id, "camera frame dropped (decoder behind)");
        }
    }
}

pub(crate) fn load_openh264() -> Result<OpenH264API, String> {
    let mut last = String::new();
    for name in OPENH264 {
        // The distribution's library: its build differs from the ones
        // whose hashes the crate knows.
        match unsafe { OpenH264API::from_blob_path_unchecked(name) } {
            Ok(api) => return Ok(api),
            Err(e) => last = format!("{name}: {e}"),
        }
    }
    Err(last)
}

/// Statistics line per streaming camera.
const STATS_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Default)]
struct DecoderState {
    decoder: Option<Decoder>,
    last_key_request: Option<Instant>,
    stats: Stats,
    /// `AMELEO_DISPLAY_AGENT_CAMERA_DUMP`: the H.264 stream as received.
    dump: Option<std::fs::File>,
}

/// Counters over the last [`STATS_INTERVAL`], logged while frames come.
#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    received: u64,
    bytes: u64,
    pictures: u64,
    errors: u64,
    key_requests: u64,
    first_error: Option<String>,
    size: Option<(usize, usize)>,
}

impl Stats {
    fn flush(&mut self, id: u32) {
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() < STATS_INTERVAL {
            return;
        }
        if self.received > 0 {
            tracing::info!(
                id,
                received = self.received,
                kbytes = self.bytes / 1024,
                pictures = self.pictures,
                errors = self.errors,
                key_requests = self.key_requests,
                size = ?self.size,
                first_error = self.first_error.as_deref().unwrap_or(""),
                "camera stats (5 s)"
            );
        }
        *self = Stats {
            since: Some(Instant::now()),
            ..Default::default()
        };
    }
}

fn decode_thread(
    rx: std_mpsc::Receiver<Dec>,
    pw_tx: pw::channel::Sender<Pw>,
    events: mpsc::UnboundedSender<Event>,
    dropped: &AtomicU64,
) {
    let mut cams: HashMap<u32, DecoderState> = HashMap::new();
    let mut seen_drops = 0;
    let dump_dir =
        std::env::var_os("AMELEO_DISPLAY_AGENT_CAMERA_DUMP").map(std::path::PathBuf::from);
    let key_frame = |id: u32, st: &mut DecoderState| {
        if st
            .last_key_request
            .is_none_or(|t| t.elapsed() >= KEY_REQUEST_INTERVAL)
        {
            st.last_key_request = Some(Instant::now());
            st.stats.key_requests += 1;
            tracing::debug!(id, "key frame needed");
            let _ = events.send(Event::KeyFrame { id });
        }
    };
    loop {
        let msg = match rx.recv_timeout(STATS_INTERVAL) {
            Ok(m) => Some(m),
            Err(std_mpsc::RecvTimeoutError::Timeout) => None,
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        };
        for (&id, st) in cams.iter_mut() {
            st.stats.flush(id);
        }
        let Some(msg) = msg else { continue };
        let drops = dropped.load(Ordering::Acquire);
        if drops != seen_drops {
            // Frames are missing: every picture until the next IDR would
            // be wrong.
            seen_drops = drops;
            for (&id, st) in cams.iter_mut() {
                st.decoder = None;
                key_frame(id, st);
            }
        }
        match msg {
            Dec::Remove { id } => {
                cams.remove(&id);
            }
            Dec::Restart { id } => {
                // The decoder is kept: apps pause and start again while
                // they negotiate, and a reset after the host's IDR would
                // break the stream until the next one. A new host stream
                // starts with an IDR and its parameter sets anyway.
                let st = cams.entry(id).or_default();
                st.last_key_request = None;
                // One file per camera and agent run: a restart appends.
                if let Some(dir) = dump_dir.as_ref().filter(|_| st.dump.is_none()) {
                    let path = dir.join(format!("camera-{id}.h264"));
                    match std::fs::File::create(&path) {
                        Ok(f) => {
                            tracing::info!("camera stream dumped to {}", path.display());
                            st.dump = Some(f);
                        }
                        Err(e) => tracing::warn!("cannot dump to {}: {e}", path.display()),
                    }
                }
            }
            Dec::Frame { id, data } => {
                let st = cams.entry(id).or_default();
                st.stats.received += 1;
                st.stats.bytes += data.len() as u64;
                if let Some(f) = &mut st.dump {
                    use std::io::Write;
                    let _ = f.write_all(&data);
                }
                if st.decoder.is_none() {
                    let created = load_openh264().and_then(|api| {
                        Decoder::with_api_config(api, DecoderConfig::new())
                            .map_err(|e| e.to_string())
                    });
                    match created {
                        Ok(d) => st.decoder = Some(d),
                        Err(e) => {
                            tracing::warn!(id, "cannot create the H.264 decoder: {e}");
                            continue;
                        }
                    }
                }
                let decoder = st.decoder.as_mut().expect("just created");
                match decoder.decode(&data) {
                    Ok(Some(yuv)) => {
                        let (w, h) = yuv.dimensions();
                        st.stats.pictures += 1;
                        st.stats.size = Some((w, h));
                        let picture = Pw::Picture {
                            id,
                            width: w as u32,
                            height: h as u32,
                            data: pack_i420(&yuv),
                        };
                        if pw_tx.send(picture).is_err() {
                            return;
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(id, "H.264 decode error: {e}");
                        st.stats.errors += 1;
                        st.stats.first_error.get_or_insert_with(|| e.to_string());
                        st.decoder = None;
                        key_frame(id, st);
                    }
                }
            }
        }
    }
}

/// The picture as packed I420: Y, then U, then V, without row padding.
fn pack_i420(yuv: &impl YUVSource) -> Vec<u8> {
    let (w, h) = yuv.dimensions();
    let (sy, su, sv) = yuv.strides();
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut out = Vec::with_capacity(w * h + 2 * cw * ch);
    for (plane, stride, pw_, ph) in [
        (yuv.y(), sy, w, h),
        (yuv.u(), su, cw, ch),
        (yuv.v(), sv, cw, ch),
    ] {
        for row in 0..ph {
            let start = row * stride;
            out.extend_from_slice(&plane[start..start + pw_]);
        }
    }
    out
}

/// The size of a packed I420 picture.
fn i420_size(width: u32, height: u32) -> usize {
    let (w, h) = (width as usize, height as usize);
    w * h + 2 * w.div_ceil(2) * h.div_ceil(2)
}

/// The index of the client format an app chose.
fn chosen(formats: &[Format], width: u32, height: u32, num: u32, denom: u32) -> Option<usize> {
    let same_rate =
        |f: &Format| u64::from(f.2) * u64::from(denom) == u64::from(num) * u64::from(f.3);
    formats
        .iter()
        .position(|f| f.0 == width && f.1 == height && same_rate(f))
        .or_else(|| formats.iter().position(|f| f.0 == width && f.1 == height))
}

/// One camera's state, shared by its stream's callbacks and the loop.
struct CamState {
    id: u32,
    formats: Vec<Format>,
    /// The format the app chose, and its size.
    format: Option<(usize, u32, u32)>,
    streaming: bool,
    size_warned: bool,
    /// The sequence number of the next buffer.
    seq: u64,
    /// The graph's position, which the node drives: PipeWire's area,
    /// valid while set, used on this thread only.
    position: *mut spa::sys::spa_io_position,
    /// Buffers given to PipeWire since the last log: with a picture,
    /// without one, and the times no buffer was free.
    filled: u64,
    empty: u64,
    no_buffer: u64,
    held: u64,
    last_log: Option<Instant>,
    /// The last picture, waiting for the next `process`.
    picture: Option<(u32, u32, Vec<u8>)>,
    /// The client's controls, with their current mode and value.
    controls: Vec<Control>,
}

impl CamState {
    /// Moves the graph's clock on, as a driver must: the apps' clocks
    /// follow it, and nothing else updates it for a stream.
    ///
    /// The position is in nanoseconds of `CLOCK_MONOTONIC`, the time base
    /// of the pictures' timestamps. Counted in pictures instead, the clock
    /// would fall behind the timestamps with every picture the client
    /// skips or the app does not take, until the app waits forever for
    /// pictures it thinks are early, holding all our buffers.
    fn advance_clock(&mut self) {
        if self.position.is_null() {
            return;
        }
        let (num, den) = self
            .format
            .and_then(|(i, _, _)| self.formats.get(i))
            .map_or((30, 1), |f| (f.2.max(1), f.3.max(1)));
        let now = monotonic_ns() as u64;
        let period = 1_000_000_000u64 * u64::from(den) / u64::from(num);
        // SAFETY: PipeWire's area, given by io_changed and not removed
        // since; the callbacks and this run on the same loop.
        let clock = unsafe { &mut (*self.position).clock };
        clock.nsec = now;
        clock.rate = spa::sys::spa_fraction {
            num: 1,
            denom: 1_000_000_000,
        };
        clock.position = now;
        clock.duration = period;
        clock.delay = 0;
        clock.rate_diff = 1.0;
        clock.next_nsec = now + period;
    }
}

struct Cam {
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<()>,
    state: Rc<RefCell<CamState>>,
}

fn pw_thread(
    rx: pw::channel::Receiver<Pw>,
    dec: std_mpsc::SyncSender<Dec>,
    events: mpsc::UnboundedSender<Event>,
    ready: &std_mpsc::Sender<Result<(), String>>,
) -> Result<(), pw::Error> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_rc(None)?;
    let cams: Rc<RefCell<HashMap<u32, Cam>>> = Rc::default();
    let _attached = {
        let cams = cams.clone();
        rx.attach(mainloop.loop_(), move |msg| match msg {
            Pw::Add {
                id,
                name,
                formats,
                reply,
            } => {
                let old = cams.borrow_mut().remove(&id);
                if let Some(old) = old {
                    stopped(&old, &events);
                }
                let res = create(&core, id, &name, formats, dec.clone(), events.clone());
                let res = match res {
                    Ok(cam) => {
                        tracing::info!(id, name, "client camera added");
                        cams.borrow_mut().insert(id, cam);
                        Ok(())
                    }
                    Err(e) => {
                        tracing::warn!(id, "cannot create the PipeWire camera: {e}");
                        Err(e.to_string())
                    }
                };
                let _ = reply.send(res);
            }
            Pw::Remove { id } => {
                let old = cams.borrow_mut().remove(&id);
                if let Some(old) = old {
                    stopped(&old, &events);
                    tracing::info!(id, "client camera removed");
                }
            }
            Pw::RemoveAll => {
                let all: Vec<Cam> = cams.borrow_mut().drain().map(|(_, c)| c).collect();
                for cam in all {
                    stopped(&cam, &events);
                    let _ = dec.send(Dec::Remove {
                        id: cam.state.borrow().id,
                    });
                }
            }
            Pw::Controls { id, controls } => {
                let cams = cams.borrow();
                let Some(cam) = cams.get(&id) else { return };
                tracing::info!(id, count = controls.len(), "camera controls");
                cam.state.borrow_mut().controls = controls;
                publish_controls(cam);
            }
            Pw::ControlValue {
                id,
                set,
                property,
                mode,
                value,
            } => {
                let cams = cams.borrow();
                let Some(cam) = cams.get(&id) else { return };
                let changed = {
                    let mut st = cam.state.borrow_mut();
                    match st
                        .controls
                        .iter_mut()
                        .find(|c| c.0 == set && c.1 == property)
                    {
                        Some(c) if (c.7, c.8) != (mode, value) => {
                            (c.7, c.8) = (mode, value);
                            true
                        }
                        _ => false,
                    }
                };
                tracing::debug!(id, set, property, mode, value, "camera control value");
                if changed {
                    publish_controls(cam);
                }
            }
            Pw::Picture {
                id,
                width,
                height,
                data,
            } => {
                let cams = cams.borrow();
                let Some(cam) = cams.get(&id) else { return };
                let streaming = {
                    let mut st = cam.state.borrow_mut();
                    st.picture = Some((width, height, data));
                    if st.streaming {
                        st.advance_clock();
                    }
                    st.streaming
                };
                if streaming && let Err(e) = cam.stream.trigger_process() {
                    tracing::debug!(id, "cannot trigger the camera node: {e}");
                }
            }
        })
    };
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

/// The host is told when a streaming camera goes away.
fn stopped(cam: &Cam, events: &mpsc::UnboundedSender<Event>) {
    let (id, was) = {
        let mut st = cam.state.borrow_mut();
        (st.id, std::mem::take(&mut st.streaming))
    };
    if was {
        let _ = events.send(Event::Stream { id, format: None });
    }
    // Calls the stream's callbacks at once: the state is not borrowed.
    let _ = cam.stream.disconnect();
}

fn serialize(obj: spa::pod::Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(obj),
    )
    .expect("pod serialization into memory")
    .0
    .into_inner()
}

/// One `EnumFormat` per client format, raw I420 at its size and rate.
fn enum_format(f: &Format) -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    serialize(spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::I420),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            spa::utils::Rectangle {
                width: f.0,
                height: f.1
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            spa::utils::Fraction {
                num: f.2,
                denom: f.3
            }
        ),
    ))
}

/// The buffers for pictures of `width` x `height`.
fn buffers(width: u32, height: u32) -> Vec<u8> {
    use spa::pod::{ChoiceValue, Property, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let int = |key: u32, v: i32| Property::new(key, Value::Int(v));
    serialize(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    // Apps keep the last pictures they show (GTK keeps
                    // them as textures on our memory): with few buffers,
                    // none is left for the next picture.
                    ChoiceEnum::Range {
                        default: 16,
                        min: 8,
                        max: 32,
                    },
                ))),
            ),
            int(spa::sys::SPA_PARAM_BUFFERS_blocks, 1),
            int(
                spa::sys::SPA_PARAM_BUFFERS_size,
                i420_size(width, height) as i32,
            ),
            int(spa::sys::SPA_PARAM_BUFFERS_stride, width as i32),
        ],
    })
}

/// Asks for a header on each buffer, for the picture's time.
fn header_meta() -> Vec<u8> {
    use spa::pod::{Property, Value};
    serialize(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(spa::utils::Id(spa::sys::SPA_META_Header)),
            ),
            Property::new(
                spa::sys::SPA_PARAM_META_size,
                Value::Int(std::mem::size_of::<spa::sys::spa_meta_header>() as i32),
            ),
        ],
    })
}

/// The header of `buf`, when it has one.
///
/// # Safety
/// `buf` is a buffer PipeWire gave and that is not queued back yet.
unsafe fn header<'a>(buf: *mut spa::sys::spa_buffer) -> Option<&'a mut spa::sys::spa_meta_header> {
    let size = std::mem::size_of::<spa::sys::spa_meta_header>() as u32;
    unsafe {
        if buf.is_null() || (*buf).metas.is_null() {
            return None;
        }
        let metas = std::slice::from_raw_parts((*buf).metas, (*buf).n_metas as usize);
        metas
            .iter()
            .find(|m| m.type_ == spa::sys::SPA_META_Header && m.size >= size && !m.data.is_null())
            .map(|m| &mut *m.data.cast::<spa::sys::spa_meta_header>())
    }
}

/// `CLOCK_MONOTONIC`, PipeWire's clock, in nanoseconds.
fn monotonic_ns() -> i64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    t.tv_sec
        .saturating_mul(1_000_000_000)
        .saturating_add(t.tv_nsec)
}

pub use proto::camera_control::Control;

/// The node property of a control's value and of its automatic mode, and
/// their names: V4L2's controls where they match (PipeWire's own ids for
/// the common ones, `SPA_PROP_START_CUSTOM` + the V4L2 id for the others,
/// as PipeWire's V4L2 cameras have them), so that `pw-v4l2` gives them to
/// V4L2 apps. The rest get ids in V4L2's private range.
fn control_props(set: u8, property: u8) -> (u32, u32, String, String) {
    use proto::camera_control as cc;
    use spa::sys::{
        SPA_PROP_START_CUSTOM, SPA_PROP_brightness, SPA_PROP_contrast, SPA_PROP_exposure,
        SPA_PROP_hue,
    };
    let v4l2 = |cid: u32| SPA_PROP_START_CUSTOM + cid;
    let private =
        |auto: u32| v4l2(0x0800_0000 + ((u32::from(set) << 8 | u32::from(property)) << 1 | auto));
    let (value, auto, name): (u32, Option<u32>, &str) = match (set, property) {
        (cc::VIDEO_PROC_AMP, cc::BRIGHTNESS) => {
            (SPA_PROP_brightness, Some(v4l2(0x0098_0920)), "Brightness")
        }
        (cc::VIDEO_PROC_AMP, cc::CONTRAST) => (SPA_PROP_contrast, None, "Contrast"),
        (cc::VIDEO_PROC_AMP, cc::HUE) => (SPA_PROP_hue, Some(v4l2(0x0098_0919)), "Hue"),
        (cc::VIDEO_PROC_AMP, cc::WHITE_BALANCE) => (
            v4l2(0x0098_091a),
            Some(v4l2(0x0098_090c)),
            "White Balance Temperature",
        ),
        (cc::VIDEO_PROC_AMP, cc::BACKLIGHT_COMPENSATION) => {
            (v4l2(0x0098_091c), None, "Backlight Compensation")
        }
        (cc::CAMERA_CONTROL, cc::EXPOSURE) => (SPA_PROP_exposure, None, "Exposure"),
        (cc::CAMERA_CONTROL, cc::FOCUS) => (v4l2(0x009a_090a), Some(v4l2(0x009a_090c)), "Focus"),
        (cc::CAMERA_CONTROL, cc::PAN) => (v4l2(0x009a_0908), None, "Pan"),
        (cc::CAMERA_CONTROL, cc::TILT) => (v4l2(0x009a_0909), None, "Tilt"),
        (cc::CAMERA_CONTROL, cc::ZOOM) => (v4l2(0x009a_090d), None, "Zoom"),
        (cc::CAMERA_CONTROL, cc::ROLL) => (private(0), None, "Roll"),
        _ => (private(0), None, ""),
    };
    let name = if name.is_empty() {
        format!("Control {set}.{property}")
    } else {
        name.to_string()
    };
    let auto_name = format!("{name}, Automatic");
    (value, auto.unwrap_or_else(|| private(1)), name, auto_name)
}

/// What the node offers of the controls: a `PropInfo` per value and per
/// automatic mode, then the current values (`Props`).
fn control_params(controls: &[Control]) -> Vec<Vec<u8>> {
    use proto::camera_control::{AUTO, MANUAL};
    use spa::pod::{ChoiceValue, Object, Property, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id, SpaTypes};
    let info = |id: u32, name: &str, type_: ChoiceValue| {
        serialize(Object {
            type_: SpaTypes::ObjectParamPropInfo.as_raw(),
            id: spa::param::ParamType::PropInfo.as_raw(),
            properties: vec![
                Property::new(spa::sys::SPA_PROP_INFO_id, Value::Id(Id(id))),
                Property::new(
                    spa::sys::SPA_PROP_INFO_description,
                    Value::String(name.into()),
                ),
                Property::new(spa::sys::SPA_PROP_INFO_type, Value::Choice(type_)),
            ],
        })
    };
    let mut params = Vec::new();
    let mut values = Vec::new();
    for &(set, property, caps, min, max, step, default, mode, value) in controls {
        let (value_id, auto_id, name, auto_name) = control_props(set, property);
        if caps & MANUAL != 0 {
            params.push(info(
                value_id,
                &name,
                ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Step {
                        default: default.clamp(min.min(max), max.max(min)),
                        min: min.min(max),
                        max: max.max(min),
                        step: step.max(1),
                    },
                )),
            ));
            // In automatic mode, some clients give a value outside the
            // range (0 for the white balance): shown within it.
            values.push(Property::new(
                value_id,
                Value::Int(value.clamp(min.min(max), max.max(min))),
            ));
        }
        if caps & AUTO != 0 {
            let def = caps & MANUAL == 0;
            params.push(info(
                auto_id,
                &auto_name,
                ChoiceValue::Bool(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: def,
                        alternatives: vec![false, true],
                    },
                )),
            ));
            values.push(Property::new(auto_id, Value::Bool(mode & AUTO != 0)));
        }
    }
    if !values.is_empty() {
        params.push(serialize(Object {
            type_: SpaTypes::ObjectParamProps.as_raw(),
            id: spa::param::ParamType::Props.as_raw(),
            properties: values,
        }));
    }
    params
}

/// The controls an app's `Props` asks to change: `(set, property, mode,
/// value)`.
fn requested(controls: &[Control], props: &[u8]) -> Vec<(u8, u8, u8, i32)> {
    use proto::camera_control::{AUTO, MANUAL};
    use spa::pod::Value;
    let Ok((_, Value::Object(obj))) =
        spa::pod::deserialize::PodDeserializer::deserialize_any_from(props)
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for prop in obj.properties {
        let number = match prop.value {
            Value::Int(v) => Some(v),
            Value::Long(v) => Some(v as i32),
            Value::Float(v) => Some(v.round() as i32),
            Value::Double(v) => Some(v.round() as i32),
            Value::Bool(v) => Some(v as i32),
            _ => None,
        };
        let Some(number) = number else { continue };
        for &(set, property, caps, min, max, _, _, mode, value) in controls {
            let (value_id, auto_id, _, _) = control_props(set, property);
            if prop.key == value_id && caps & MANUAL != 0 {
                out.push((
                    set,
                    property,
                    MANUAL,
                    number.clamp(min.min(max), max.max(min)),
                ));
            } else if prop.key == auto_id && caps & AUTO != 0 {
                let auto = number != 0;
                if auto != (mode & AUTO != 0) {
                    out.push((set, property, if auto { AUTO } else { MANUAL }, value));
                }
            }
        }
    }
    out
}

/// Gives the node's controls to PipeWire.
fn publish_controls(cam: &Cam) {
    let params = control_params(&cam.state.borrow().controls);
    let mut pods: Vec<&spa::pod::Pod> = params
        .iter()
        .filter_map(|b| spa::pod::Pod::from_bytes(b))
        .collect();
    if pods.is_empty() {
        return;
    }
    // May call back: the state is not borrowed.
    if let Err(e) = cam.stream.update_params(&mut pods) {
        tracing::warn!("cannot give the camera controls to PipeWire: {e}");
    }
}

fn create(
    core: &pw::core::CoreRc,
    id: u32,
    name: &str,
    formats: Vec<Format>,
    dec: std_mpsc::SyncSender<Dec>,
    events: mpsc::UnboundedSender<Event>,
) -> Result<Cam, pw::Error> {
    let node_name = format!("ameleo-camera-{id}");
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        &node_name,
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_ROLE => "Camera",
            *pw::keys::MEDIA_CLASS => "Video/Source",
            *pw::keys::NODE_NAME => node_name.as_str(),
            *pw::keys::NODE_DESCRIPTION => name,
            *pw::keys::NODE_NICK => name,
        },
    )?;
    let state = Rc::new(RefCell::new(CamState {
        id,
        formats: formats.clone(),
        format: None,
        streaming: false,
        size_warned: false,
        seq: 0,
        position: std::ptr::null_mut(),
        filled: 0,
        empty: 0,
        no_buffer: 0,
        held: 0,
        last_log: None,
        picture: None,
        controls: Vec::new(),
    }));
    let listener = {
        let (s1, s2, s3, s4) = (state.clone(), state.clone(), state.clone(), state.clone());
        let events2 = events.clone();
        let events3 = events.clone();
        stream
            .add_local_listener_with_user_data(())
            .param_changed(move |stream, _, pid, param| {
                if pid == spa::param::ParamType::Props.as_raw() {
                    // An app sets controls: asked of the client, shown
                    // when it answers (`ControlValue`).
                    let st = s1.borrow();
                    for (set, property, mode, value) in param
                        .map(|p| requested(&st.controls, p.as_bytes()))
                        .unwrap_or_default()
                    {
                        tracing::info!(
                            id = st.id,
                            set,
                            property,
                            mode,
                            value,
                            "camera control asked"
                        );
                        let _ = events3.send(Event::Control {
                            id: st.id,
                            set,
                            property,
                            mode,
                            value,
                        });
                    }
                    return;
                }
                if pid != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let mut st = s1.borrow_mut();
                let Some(param) = param else {
                    st.format = None;
                    return;
                };
                let mut info = spa::param::video::VideoInfoRaw::default();
                if info.parse(param).is_err() {
                    return;
                }
                let (size, rate) = (info.size(), info.framerate());
                let index = chosen(&st.formats, size.width, size.height, rate.num, rate.denom);
                tracing::info!(
                    id = st.id,
                    width = size.width,
                    height = size.height,
                    fps = format!("{}/{}", rate.num, rate.denom),
                    ?index,
                    "camera format chosen by an app"
                );
                st.format = index.map(|i| (i, size.width, size.height));
                let id = st.id;
                // update_params may call back: the state is not borrowed.
                drop(st);
                let bytes = buffers(size.width, size.height);
                let meta = header_meta();
                let pods = [&bytes, &meta].map(|b| spa::pod::Pod::from_bytes(b));
                if let [Some(b), Some(m)] = pods
                    && let Err(e) = stream.update_params(&mut [b, m])
                {
                    tracing::warn!(id, "cannot set the camera buffers: {e}");
                }
            })
            .io_changed(move |_, _, id, area, _size| {
                if id == spa::sys::SPA_IO_Position {
                    s4.borrow_mut().position = area.cast();
                }
            })
            .state_changed(move |_, _, old, new| {
                let mut st = s2.borrow_mut();
                tracing::debug!(id = st.id, ?old, ?new, "camera node state");
                let streaming = matches!(new, pw::stream::StreamState::Streaming);
                if streaming == st.streaming {
                    return;
                }
                st.streaming = streaming;
                st.picture = None;
                let format = if streaming {
                    let _ = dec.send(Dec::Restart { id: st.id });
                    st.format.map(|(i, _, _)| i as u32)
                } else {
                    None
                };
                if streaming && format.is_none() {
                    tracing::warn!(
                        id = st.id,
                        "camera started in a format the client did not offer"
                    );
                    return;
                }
                let _ = events2.send(Event::Stream { id: st.id, format });
            })
            .process(move |stream, _| {
                let mut st = s3.borrow_mut();
                let since = *st.last_log.get_or_insert_with(Instant::now);
                if since.elapsed() >= STATS_INTERVAL {
                    tracing::info!(
                        id = st.id,
                        filled = st.filled,
                        empty = st.empty,
                        no_buffer = st.no_buffer,
                        held = st.held,
                        "camera node stats (5 s)"
                    );
                    st.held = 0;
                    st.filled = 0;
                    st.empty = 0;
                    st.no_buffer = 0;
                    st.last_log = Some(Instant::now());
                }
                // One buffer at a time in the graph: a slow app (GTK takes
                // its time to show a picture) would otherwise leave our
                // buffers piling up in the queue, until none is free. The
                // picture waits for the next cycle; a newer one replaces it.
                if st.picture.is_some() && stream.time().is_ok_and(|t| t.queued_buffers() > 0) {
                    st.held += 1;
                    return;
                }
                // The raw buffer: its header is written too, which the
                // safe wrapper does not allow.
                let raw = unsafe { stream.dequeue_raw_buffer() };
                if raw.is_null() {
                    st.no_buffer += 1;
                    return;
                }
                let spa_buf = unsafe { (*raw).buffer };
                // Each picture's time: without it, apps take the graph's
                // clock, which a driver triggered by hand does not move,
                // and drop every picture after the first as a duplicate.
                if let Some(h) = unsafe { header(spa_buf) } {
                    h.pts = monotonic_ns();
                    h.dts_offset = 0;
                    h.flags = 0;
                    h.seq = st.seq;
                    st.seq = st.seq.wrapping_add(1);
                }
                let data = unsafe {
                    if (*spa_buf).n_datas == 0 || (*spa_buf).datas.is_null() {
                        stream.queue_raw_buffer(raw);
                        return;
                    }
                    &mut *(*spa_buf).datas
                };
                let expected = st.format.map(|(_, w, h)| (w, h));
                let picture = st.picture.take();
                let mut written = 0;
                let mut stride = 0;
                if let Some((w, h, pixels)) = picture {
                    if Some((w, h)) != expected {
                        if !st.size_warned {
                            st.size_warned = true;
                            tracing::warn!(
                                id = st.id,
                                w,
                                h,
                                ?expected,
                                "decoded pictures differ from the format the app chose"
                            );
                        }
                    } else if !data.data.is_null() && data.maxsize as usize >= pixels.len() {
                        // MAP_BUFFERS: the memory is mapped, `maxsize` long.
                        let target = unsafe {
                            std::slice::from_raw_parts_mut(data.data.cast::<u8>(), pixels.len())
                        };
                        target.copy_from_slice(&pixels);
                        written = pixels.len();
                        stride = w as i32;
                    }
                }
                if written > 0 {
                    st.filled += 1;
                } else {
                    st.empty += 1;
                }
                if let Some(chunk) = unsafe { data.chunk.as_mut() } {
                    chunk.offset = 0;
                    chunk.size = written as u32;
                    chunk.stride = stride;
                    chunk.flags = 0;
                }
                unsafe { stream.queue_raw_buffer(raw) };
            })
            .register()?
    };
    let pods: Vec<Vec<u8>> = formats.iter().map(enum_format).collect();
    let mut params: Vec<&spa::pod::Pod> = pods
        .iter()
        .filter_map(|b| spa::pod::Pod::from_bytes(b))
        .collect();
    stream.connect(
        spa::utils::Direction::Output,
        None,
        pw::stream::StreamFlags::DRIVER | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;
    Ok(Cam {
        stream,
        _listener: listener,
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_choice() {
        let f = [
            (1280, 720, 30, 1),
            (1280, 720, 15, 1),
            (640, 480, 30000, 1001),
        ];
        assert_eq!(chosen(&f, 1280, 720, 15, 1), Some(1));
        assert_eq!(chosen(&f, 1280, 720, 30, 1), Some(0));
        assert_eq!(chosen(&f, 640, 480, 30000, 1001), Some(2));
        // Same size at another rate: the first one of that size.
        assert_eq!(chosen(&f, 1280, 720, 60, 1), Some(0));
        assert_eq!(chosen(&f, 320, 240, 30, 1), None);
    }

    struct Fake {
        dims: (usize, usize),
        y: Vec<u8>,
        u: Vec<u8>,
        v: Vec<u8>,
        strides: (usize, usize, usize),
    }

    impl YUVSource for Fake {
        fn dimensions(&self) -> (usize, usize) {
            self.dims
        }
        fn strides(&self) -> (usize, usize, usize) {
            self.strides
        }
        fn y(&self) -> &[u8] {
            &self.y
        }
        fn u(&self) -> &[u8] {
            &self.u
        }
        fn v(&self) -> &[u8] {
            &self.v
        }
    }

    #[test]
    fn i420_without_padding() {
        // 4x2 picture, rows padded to 8 (luma) and 4 (chroma).
        let f = Fake {
            dims: (4, 2),
            y: vec![1, 2, 3, 4, 0, 0, 0, 0, 5, 6, 7, 8, 0, 0, 0, 0],
            u: vec![9, 10, 0, 0],
            v: vec![11, 12, 0, 0],
            strides: (8, 4, 4),
        };
        let out = pack_i420(&f);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert_eq!(out.len(), i420_size(4, 2));
    }

    #[test]
    fn controls_to_props_and_back() {
        use proto::camera_control::*;
        let controls: Vec<Control> = vec![
            // Brightness: manual, at 10 of -64..64.
            (
                VIDEO_PROC_AMP,
                BRIGHTNESS,
                MANUAL,
                -64,
                64,
                1,
                0,
                MANUAL,
                10,
            ),
            // Focus: manual or auto, auto now.
            (CAMERA_CONTROL, FOCUS, MANUAL | AUTO, 0, 250, 5, 0, AUTO, 30),
        ];
        let params = control_params(&controls);
        // Two values, one auto, the Props.
        assert_eq!(params.len(), 4);
        assert!(
            params
                .iter()
                .all(|p| spa::pod::Pod::from_bytes(p).is_some())
        );
        let (focus, focus_auto, name, auto_name) = control_props(CAMERA_CONTROL, FOCUS);
        assert_eq!(focus, spa::sys::SPA_PROP_START_CUSTOM + 0x009a_090a);
        assert_eq!(
            (name.as_str(), auto_name.as_str()),
            ("Focus", "Focus, Automatic")
        );
        let props = serialize(spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamProps.as_raw(),
            id: spa::param::ParamType::Props.as_raw(),
            properties: vec![
                spa::pod::Property::new(spa::sys::SPA_PROP_brightness, spa::pod::Value::Int(99)),
                spa::pod::Property::new(focus_auto, spa::pod::Value::Bool(false)),
                spa::pod::Property::new(focus, spa::pod::Value::Float(120.0)),
            ],
        });
        assert_eq!(
            requested(&controls, &props),
            [
                (VIDEO_PROC_AMP, BRIGHTNESS, MANUAL, 64),
                (CAMERA_CONTROL, FOCUS, MANUAL, 30),
                (CAMERA_CONTROL, FOCUS, MANUAL, 120),
            ]
        );
        // Unknown controls still get distinct ids.
        assert_ne!(control_props(9, 1).0, control_props(9, 2).0);
        assert_ne!(control_props(9, 1).0, control_props(9, 1).1);
    }

    #[test]
    fn pods_serialize() {
        assert!(spa::pod::Pod::from_bytes(&enum_format(&(640, 480, 30, 1))).is_some());
        assert!(spa::pod::Pod::from_bytes(&buffers(640, 480)).is_some());
        assert!(spa::pod::Pod::from_bytes(&header_meta()).is_some());
    }

    /// Creates a camera on the session's PipeWire: needs a running
    /// PipeWire and libopenh264.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn camera_node_appears() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let cams = Cameras::spawn(tx).unwrap();
        cams.add(42, "ameleo test camera".into(), vec![(640, 480, 30, 1)])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        // AMELEO_HOLD=<seconds>: keep the camera, to look at it.
        if let Some(secs) = std::env::var("AMELEO_HOLD")
            .ok()
            .and_then(|s| s.parse().ok())
        {
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
        let out = tokio::process::Command::new("pw-cli")
            .args(["ls", "Node"])
            .output()
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("ameleo-camera-42"), "{text}");
        cams.remove(42);
    }

    /// The controls of a camera, as an app sees and sets them through
    /// PipeWire (`pw-cli` here): needs a running PipeWire and libopenh264.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn camera_controls() {
        use proto::camera_control::*;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cams = Cameras::spawn(tx).unwrap();
        cams.add(43, "ameleo control camera".into(), vec![(640, 480, 30, 1)])
            .await
            .unwrap();
        cams.set_controls(
            43,
            vec![
                (
                    VIDEO_PROC_AMP,
                    BRIGHTNESS,
                    MANUAL,
                    -64,
                    64,
                    1,
                    0,
                    MANUAL,
                    10,
                ),
                (CAMERA_CONTROL, FOCUS, MANUAL | AUTO, 0, 250, 5, 0, AUTO, 30),
            ],
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        let pw_cli = |args: Vec<String>| async move {
            let out = tokio::process::Command::new("pw-cli")
                .args(&args)
                .output()
                .await
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let nodes = pw_cli(vec!["ls".into(), "Node".into()]).await;
        let id = nodes
            .split("\tid ")
            .find(|n| n.contains("ameleo-camera-43"))
            .and_then(|n| n.split(',').next())
            .expect("camera node")
            .trim()
            .to_string();
        let info = pw_cli(vec!["enum-params".into(), id.clone(), "PropInfo".into()]).await;
        eprintln!("{info}");
        for name in ["Brightness", "Focus", "Focus, Automatic"] {
            assert!(info.contains(&format!("\"{name}\"")), "{name} missing");
        }
        // An app turns autofocus off and sets the brightness.
        let focus_auto = control_props(CAMERA_CONTROL, FOCUS).1;
        pw_cli(vec![
            "set-param".into(),
            id.clone(),
            "Props".into(),
            format!("{{ brightness: 20, {focus_auto}: false }}"),
        ])
        .await;
        let mut asked = Vec::new();
        while asked.len() < 2 {
            match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
                Ok(Some(Event::Control {
                    set,
                    property,
                    mode,
                    value,
                    ..
                })) => asked.push((set, property, mode, value)),
                Ok(Some(_)) => {}
                _ => panic!("control requests: {asked:?}"),
            }
        }
        asked.sort();
        assert_eq!(
            asked,
            [
                (CAMERA_CONTROL, FOCUS, MANUAL, 30),
                (VIDEO_PROC_AMP, BRIGHTNESS, MANUAL, 20)
            ]
        );
        // The client took it: the node shows it.
        cams.control_value(43, VIDEO_PROC_AMP, BRIGHTNESS, MANUAL, 20);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let props = pw_cli(vec!["enum-params".into(), id, "Props".into()]).await;
        eprintln!("{props}");
        assert!(props.contains("Int 20"), "{props}");
        cams.remove(43);
    }

    /// Replays a dumped H.264 stream (`AMELEO_REPLAY=<file>`, 1080p30)
    /// to a clock-synced consumer, and checks the pictures it gets.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn replay() {
        // AMELEO_REPLAY_OUT: where the test app writes what it gets (the
        // last picture is then checked); without it, the app only syncs.
        let out = std::env::var("AMELEO_REPLAY_OUT").ok();
        let d = match std::env::var("AMELEO_REPLAY") {
            Ok(path) => std::fs::read(path).unwrap(),
            // A moving picture, through the same library.
            Err(_) => {
                let mut enc = openh264::encoder::Encoder::with_api_config(
                    load_openh264().unwrap(),
                    openh264::encoder::EncoderConfig::new(),
                )
                .unwrap();
                let mut stream = Vec::new();
                for n in 0..120usize {
                    let mut yuv = vec![128u8; 1920 * 1080 * 3 / 2];
                    for (i, p) in yuv[..1920 * 1080].iter_mut().enumerate() {
                        *p = ((i % 1920 + n * 8) % 256) as u8;
                    }
                    let buf = openh264::formats::YUVBuffer::from_vec(yuv, 1920, 1080);
                    stream.extend(enc.encode(&buf).unwrap().to_vec());
                }
                stream
            }
        };
        // Access units: SPS + PPS + IDR, then one NAL each.
        let mut starts = Vec::new();
        let mut i = 0;
        while let Some(j) = d[i..].windows(3).position(|w| w == [0, 0, 1]) {
            let s = i + j;
            let s = if s > 0 && d[s - 1] == 0 { s - 1 } else { s };
            starts.push(s);
            i = i + j + 3;
        }
        // A new unit at each slice; SPS and PPS go with the slice after.
        let mut merged: Vec<Vec<u8>> = Vec::new();
        let mut current: Vec<u8> = Vec::new();
        let mut has_slice = false;
        for (k, &s) in starts.iter().enumerate() {
            let e = starts.get(k + 1).copied().unwrap_or(d.len());
            let nal = &d[s..e];
            let ty = nal[nal.iter().position(|&b| b == 1).unwrap() + 1] & 0x1f;
            let slice = ty == 1 || ty == 5;
            if (slice || ty == 7) && has_slice {
                merged.push(std::mem::take(&mut current));
                has_slice = false;
            }
            current.extend_from_slice(nal);
            has_slice |= slice;
        }
        merged.push(current);
        eprintln!("{} access units", merged.len());
        // AMELEO_REPLAY_INTERVAL_MS: the client's pace (33 by default),
        // AMELEO_REPLAY_FRAMES: the pictures the test app waits for (40).
        let env = |name: &str, default: u64| {
            std::env::var(name)
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default)
        };
        let interval = Duration::from_millis(env("AMELEO_REPLAY_INTERVAL_MS", 33));
        let frames = env("AMELEO_REPLAY_FRAMES", 40);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let cams = Cameras::spawn(tx).unwrap();
        cams.add(42, "replay".into(), vec![(1920, 1080, 30, 1)])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        // AMELEO_REPLAY_SECS: no test app, serve whatever app opens the
        // camera for that long (to try real apps).
        if let Some(secs) = std::env::var("AMELEO_REPLAY_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
        {
            eprintln!("camera ready for {secs} s");
            let end = tokio::time::Instant::now() + Duration::from_secs(secs);
            let mut started = false;
            for au in merged.iter().cycle() {
                if tokio::time::Instant::now() >= end {
                    break;
                }
                while let Ok(ev) = rx.try_recv() {
                    eprintln!("event {ev:?}");
                    if let Event::Stream { format, .. } = ev {
                        started = format.is_some();
                    }
                }
                if started {
                    cams.push(42, au.clone());
                }
                tokio::time::sleep(interval).await;
            }
            cams.remove(42);
            return;
        }
        let mut app = tokio::process::Command::new("gst-launch-1.0")
            .args([
                "-v",
                "pipewiresrc",
                "target-object=ameleo-camera-42",
                "stream-properties=props,media.type=Video,media.category=Capture,media.role=Camera",
                &format!("num-buffers={frames}"),
                "!",
                "identity",
                "silent=false",
                "!",
                "queue",
                "!",
                if out.is_some() {
                    "filesink"
                } else {
                    "fakesink"
                },
                "sync=true",
                &out.as_ref()
                    .map_or("silent=true".into(), |o| format!("location={o}")),
            ])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
            {
                Some(Event::Stream {
                    format: Some(_), ..
                }) => break,
                other => eprintln!("event {other:?}"),
            }
        }
        let push = async {
            for au in merged.iter().cycle() {
                cams.push(42, au.clone());
                tokio::time::sleep(interval).await;
            }
        };
        let status = tokio::select! {
            s = app.wait() => s.unwrap(),
            _ = push => unreachable!(),
            _ = tokio::time::sleep(interval * frames as u32 * 2 + Duration::from_secs(5)) => panic!("the app stopped taking pictures"),
        };
        assert!(status.success());
        let Some(out) = out else {
            cams.remove(42);
            return;
        };
        let got = std::fs::read(&out).unwrap();
        let frame = 1920 * 1080 * 3 / 2;
        eprintln!(
            "got {} bytes = {} frames",
            got.len(),
            got.len() as f64 / frame as f64
        );
        let last = &got[got.len() - frame..got.len() - frame + 1920 * 1080];
        let mean = last.iter().map(|&b| b as f64).sum::<f64>() / last.len() as f64;
        let var = last.iter().map(|&b| (b as f64 - mean).powi(2)).sum::<f64>() / last.len() as f64;
        eprintln!("last picture luma mean {mean:.1} variance {var:.1}");
        cams.remove(42);
    }
}
