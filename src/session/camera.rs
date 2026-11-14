// SPDX-License-Identifier: GPL-3.0-or-later

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

struct DecoderState {
    decoder: Option<Decoder>,
    last_key_request: Option<Instant>,
}

fn decode_thread(
    rx: std_mpsc::Receiver<Dec>,
    pw_tx: pw::channel::Sender<Pw>,
    events: mpsc::UnboundedSender<Event>,
    dropped: &AtomicU64,
) {
    let mut cams: HashMap<u32, DecoderState> = HashMap::new();
    let mut seen_drops = 0;
    let key_frame = |id: u32, st: &mut DecoderState| {
        if st
            .last_key_request
            .is_none_or(|t| t.elapsed() >= KEY_REQUEST_INTERVAL)
        {
            st.last_key_request = Some(Instant::now());
            tracing::debug!(id, "key frame needed");
            let _ = events.send(Event::KeyFrame { id });
        }
    };
    for msg in rx {
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
                let st = cams.entry(id).or_insert(DecoderState {
                    decoder: None,
                    last_key_request: None,
                });
                st.decoder = None;
                st.last_key_request = None;
            }
            Dec::Frame { id, data } => {
                let st = cams.entry(id).or_insert(DecoderState {
                    decoder: None,
                    last_key_request: None,
                });
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
    /// The last picture, waiting for the next `process`.
    picture: Option<(u32, u32, Vec<u8>)>,
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
                    ChoiceEnum::Range {
                        default: 4,
                        min: 2,
                        max: 8,
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
        picture: None,
    }));
    let listener = {
        let (s1, s2, s3) = (state.clone(), state.clone(), state.clone());
        let events2 = events.clone();
        stream
            .add_local_listener_with_user_data(())
            .param_changed(move |stream, _, pid, param| {
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
                if let Some(pod) = spa::pod::Pod::from_bytes(&bytes)
                    && let Err(e) = stream.update_params(&mut [pod])
                {
                    tracing::warn!(id, "cannot set the camera buffers: {e}");
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
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let mut st = s3.borrow_mut();
                let expected = st.format.map(|(_, w, h)| (w, h));
                let picture = st.picture.take();
                let datas = buffer.datas_mut();
                let Some(data) = datas.first_mut() else {
                    return;
                };
                let mut written = 0;
                let mut stride = 0;
                if let Some((w, h, pixels)) = picture {
                    if Some((w, h)) != expected {
                        tracing::debug!(id = st.id, w, h, "picture size differs from the format");
                    } else if let Some(target) = data.data()
                        && target.len() >= pixels.len()
                    {
                        target[..pixels.len()].copy_from_slice(&pixels);
                        written = pixels.len();
                        stride = w as i32;
                    }
                }
                let chunk = data.chunk_mut();
                *chunk.offset_mut() = 0;
                *chunk.size_mut() = written as u32;
                *chunk.stride_mut() = stride;
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
    fn pods_serialize() {
        assert!(spa::pod::Pod::from_bytes(&enum_format(&(640, 480, 30, 1))).is_some());
        assert!(spa::pod::Pod::from_bytes(&buffers(640, 480)).is_some());
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
}
