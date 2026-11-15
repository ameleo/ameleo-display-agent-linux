// SPDX-License-Identifier: GPL-3.0-only

//! The Wayland side of the session agent, on its own thread:
//!
//! * the monitors (`wl_output` with `xdg_output`): where the compositor put
//!   each virtio-gpu head, for the pointer;
//! * the clipboard through a data control protocol (`ext-data-control-v1`,
//!   or `wlr-data-control-unstable-v1`), which KWin and the wlroots
//!   compositors offer to clients without a window. Mutter has neither: on
//!   GNOME the clipboard goes through the Shell extension
//!   ([`super::gnome`]).
//!
//! The selection we set carries [`MARKER`], so that the agent does not take
//! its own selection for a guest copy.

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::sync::mpsc;

use rustix::event::{PollFd, PollFlags, poll};
use tokio::sync::{mpsc as tmpsc, oneshot};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

use crate::ipc::Head;

/// Offered with every selection the agent sets.
pub const MARKER: &str = "application/x-ameleo-display-agent";
/// Largest guest clipboard read.
const MAX_READ: usize = 256 * 1024 * 1024;

/// What the Wayland thread reports.
pub enum Event {
    /// The monitors changed.
    Layout(Vec<Head>),
    /// A guest app copied: the MIME types it offers; `None`: the clipboard
    /// was emptied.
    Selection(Option<Vec<String>>),
    /// A guest app pastes our selection: write `mime` to `fd`.
    Send { mime: String, fd: OwnedFd },
}

enum Cmd {
    /// Set our selection with these types (the marker is added), or drop
    /// it if we own the clipboard.
    Offer(Option<Vec<String>>),
    /// Read the current selection as `mime`.
    Read {
        mime: String,
        reply: oneshot::Sender<Result<Vec<u8>, String>>,
    },
}

#[derive(Clone)]
pub struct Handle {
    tx: mpsc::Sender<Cmd>,
    wake: std::sync::Arc<OwnedFd>,
    /// A data control protocol is there: the clipboard works.
    pub clipboard: bool,
}

impl Handle {
    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            let _ = rustix::io::write(self.wake.as_fd(), &[1]);
        }
    }

    pub fn offer(&self, mimes: Option<Vec<String>>) {
        self.send(Cmd::Offer(mimes));
    }

    pub async fn read(&self, mime: &str) -> Result<Vec<u8>, String> {
        let (reply, rx) = oneshot::channel();
        self.send(Cmd::Read {
            mime: mime.to_string(),
            reply,
        });
        rx.await.map_err(|_| "Wayland thread gone".to_string())?
    }
}

/// Connects to the compositor and starts the thread.
pub fn spawn(events: tmpsc::UnboundedSender<Event>) -> anyhow::Result<Handle> {
    let conn = Connection::connect_to_env()?;
    let mut queue: EventQueue<State> = conn.new_event_queue();
    let qh = queue.handle();
    conn.display().get_registry(&qh, ());
    let mut state = State {
        events: Some(events),
        ..State::default()
    };
    // Globals, then the objects bound on them, then their first events.
    queue.roundtrip(&mut state)?;
    state.setup(&qh);
    queue.roundtrip(&mut state)?;
    queue.roundtrip(&mut state)?;
    let clipboard = state.device.is_some();
    match &state.device {
        Some(Device::Ext(_)) => tracing::info!("clipboard: ext-data-control-v1"),
        Some(Device::Wlr(_)) => tracing::info!("clipboard: wlr-data-control-unstable-v1"),
        None => tracing::info!("the compositor has no data control protocol"),
    }
    state.layout_changed();

    let (tx, rx) = mpsc::channel();
    let (wake_rx, wake_tx) = rustix::pipe::pipe_with(
        rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
    )?;
    std::thread::Builder::new()
        .name("wayland".into())
        .spawn(move || {
            if let Err(e) = run(conn, queue, state, rx, wake_rx) {
                tracing::error!("Wayland connection lost: {e}");
                // The compositor is gone, so is the session.
                std::process::exit(1);
            }
        })?;
    Ok(Handle {
        tx,
        wake: std::sync::Arc::new(wake_tx),
        clipboard,
    })
}

fn run(
    conn: Connection,
    mut queue: EventQueue<State>,
    mut state: State,
    rx: mpsc::Receiver<Cmd>,
    wake: OwnedFd,
) -> anyhow::Result<()> {
    let qh = queue.handle();
    loop {
        queue.dispatch_pending(&mut state)?;
        for cmd in rx.try_iter() {
            state.command(cmd, &qh);
        }
        conn.flush()?;
        let Some(guard) = queue.prepare_read() else {
            continue; // events already queued
        };
        let (wl_ready, wake_ready) = {
            let wl_fd = guard.connection_fd();
            let mut fds = [
                PollFd::new(&wl_fd, PollFlags::IN),
                PollFd::new(&wake, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
            (!fds[0].revents().is_empty(), !fds[1].revents().is_empty())
        };
        if wl_ready {
            guard.read()?;
        } else {
            drop(guard);
        }
        if wake_ready {
            let mut buf = [0u8; 64];
            while matches!(rustix::io::read(&wake, &mut buf), Ok(n) if n > 0) {}
        }
    }
}

#[derive(Default)]
struct Output {
    wl: Option<wl_output::WlOutput>,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    global: u32,
    /// Order of appearance, the head when the name says nothing.
    order: u32,
    name: Option<String>,
    position: (i32, i32),
    mode: (u32, u32),
    scale: i32,
    rotated: bool,
    logical: Option<(i32, i32, u32, u32)>,
}

impl Output {
    /// virtio-gpu connectors are `Virtual-1`, `Virtual-2`...: scanout 0, 1...
    fn head(&self) -> u32 {
        self.name
            .as_deref()
            .and_then(|n| n.strip_prefix("Virtual-"))
            .and_then(|n| n.parse::<u32>().ok())
            .map(|n| n.saturating_sub(1))
            .unwrap_or(self.order)
    }

    fn to_head(&self) -> Option<Head> {
        let (mw, mh) = if self.rotated {
            (self.mode.1, self.mode.0)
        } else {
            self.mode
        };
        if mw == 0 || mh == 0 {
            return None;
        }
        let (x, y, w, h) = self.logical.unwrap_or_else(|| {
            let s = self.scale.max(1) as u32;
            (self.position.0, self.position.1, mw / s, mh / s)
        });
        // The host's pixels are the mode's, unrotated.
        Some((self.head(), x, y, w, h, self.mode.0, self.mode.1))
    }
}

// The two data control protocols are the same protocol under two names.
macro_rules! data_control {
    ($module:ident, $variant:ident, $base:path,
     $manager:ident :: $Manager:ident, $device:ident :: $Device:ident,
     $source:ident :: $Source:ident, $offer:ident :: $Offer:ident) => {
        mod $module {
            pub use $base::{$device, $manager, $offer, $source};
        }

        impl Dispatch<$module::$manager::$Manager, ()> for State {
            fn event(
                _: &mut Self,
                _: &$module::$manager::$Manager,
                _: <$module::$manager::$Manager as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }

        impl Dispatch<$module::$device::$Device, ()> for State {
            fn event(
                state: &mut Self,
                _: &$module::$device::$Device,
                event: $module::$device::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                use $module::$device::Event as E;
                match event {
                    E::DataOffer { id } => {
                        state.offers.insert(id.id(), (Offer::$variant(id), Vec::new()));
                    }
                    E::Selection { id } => state.selection(id.map(|o| o.id())),
                    // The primary selection is not shared with the host.
                    E::PrimarySelection { id: Some(o) } => {
                        if let Some((offer, _)) = state.offers.remove(&o.id()) {
                            offer.destroy();
                        }
                    }
                    E::Finished => {
                        tracing::warn!("the data control device went away");
                        state.device = None;
                    }
                    _ => {}
                }
            }

            wayland_client::event_created_child!(State, $module::$device::$Device, [
                $module::$device::EVT_DATA_OFFER_OPCODE => ($module::$offer::$Offer, ()),
            ]);
        }

        impl Dispatch<$module::$offer::$Offer, ()> for State {
            fn event(
                state: &mut Self,
                offer: &$module::$offer::$Offer,
                event: $module::$offer::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let $module::$offer::Event::Offer { mime_type } = event {
                    if let Some((_, mimes)) = state.offers.get_mut(&offer.id()) {
                        mimes.push(mime_type);
                    }
                }
            }
        }

        impl Dispatch<$module::$source::$Source, ()> for State {
            fn event(
                state: &mut Self,
                source: &$module::$source::$Source,
                event: $module::$source::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                use $module::$source::Event as E;
                match event {
                    E::Send { mime_type, fd } => {
                        state.send(Event::Send {
                            mime: mime_type,
                            fd,
                        });
                    }
                    E::Cancelled => {
                        source.destroy();
                        if state.source.as_ref().is_some_and(|s| s.id() == source.id()) {
                            state.source = None;
                        }
                    }
                    _ => {}
                }
            }
        }
    };
}

data_control!(
    ext,
    Ext,
    wayland_protocols::ext::data_control::v1::client,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1
);
data_control!(
    wlr,
    Wlr,
    wayland_protocols_wlr::data_control::v1::client,
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1
);

enum Manager {
    Ext(ext::ext_data_control_manager_v1::ExtDataControlManagerV1),
    Wlr(wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1),
}

enum Device {
    Ext(ext::ext_data_control_device_v1::ExtDataControlDeviceV1),
    Wlr(wlr::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1),
}

enum Source {
    Ext(ext::ext_data_control_source_v1::ExtDataControlSourceV1),
    Wlr(wlr::zwlr_data_control_source_v1::ZwlrDataControlSourceV1),
}

enum Offer {
    Ext(ext::ext_data_control_offer_v1::ExtDataControlOfferV1),
    Wlr(wlr::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1),
}

impl Source {
    fn id(&self) -> ObjectId {
        match self {
            Source::Ext(s) => s.id(),
            Source::Wlr(s) => s.id(),
        }
    }
    fn offer(&self, mime: String) {
        match self {
            Source::Ext(s) => s.offer(mime),
            Source::Wlr(s) => s.offer(mime),
        }
    }
    fn destroy(&self) {
        match self {
            Source::Ext(s) => s.destroy(),
            Source::Wlr(s) => s.destroy(),
        }
    }
}

impl Offer {
    fn receive(&self, mime: String, fd: std::os::fd::BorrowedFd<'_>) {
        match self {
            Offer::Ext(o) => o.receive(mime, fd),
            Offer::Wlr(o) => o.receive(mime, fd),
        }
    }
    fn destroy(&self) {
        match self {
            Offer::Ext(o) => o.destroy(),
            Offer::Wlr(o) => o.destroy(),
        }
    }
}

#[derive(Default)]
struct State {
    events: Option<tmpsc::UnboundedSender<Event>>,
    seat: Option<wl_seat::WlSeat>,
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    manager: Option<Manager>,
    device: Option<Device>,
    outputs: Vec<Output>,
    next_order: u32,
    layout: Vec<Head>,
    /// Offers announced, not yet the selection.
    offers: HashMap<ObjectId, (Offer, Vec<String>)>,
    /// The current selection, when another client owns it.
    selection: Option<(Offer, Vec<String>)>,
    /// Our selection.
    source: Option<Source>,
}

impl State {
    fn send(&self, event: Event) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    fn setup(&mut self, qh: &QueueHandle<Self>) {
        if let (Some(manager), Some(seat)) = (&self.manager, &self.seat) {
            self.device = Some(match manager {
                Manager::Ext(m) => Device::Ext(m.get_data_device(seat, qh, ())),
                Manager::Wlr(m) => Device::Wlr(m.get_data_device(seat, qh, ())),
            });
        }
        for i in 0..self.outputs.len() {
            self.bind_xdg(i, qh);
        }
    }

    fn bind_xdg(&mut self, i: usize, qh: &QueueHandle<Self>) {
        let (Some(manager), Some(wl)) = (&self.xdg_manager, &self.outputs[i].wl) else {
            return;
        };
        if self.outputs[i].xdg.is_none() {
            let global = self.outputs[i].global;
            self.outputs[i].xdg = Some(manager.get_xdg_output(wl, qh, global));
        }
    }

    fn output(&mut self, global: u32) -> Option<&mut Output> {
        self.outputs.iter_mut().find(|o| o.global == global)
    }

    fn layout_changed(&mut self) {
        let mut heads: Vec<Head> = self.outputs.iter().filter_map(Output::to_head).collect();
        heads.sort();
        if heads != self.layout {
            self.layout = heads.clone();
            self.send(Event::Layout(heads));
        }
    }

    fn selection(&mut self, id: Option<ObjectId>) {
        if let Some((old, _)) = self.selection.take() {
            old.destroy();
        }
        let Some(id) = id else {
            self.send(Event::Selection(None));
            return;
        };
        let Some((offer, mimes)) = self.offers.remove(&id) else {
            return;
        };
        // Offers that never became the selection are dead now.
        for (_, (o, _)) in self.offers.drain() {
            o.destroy();
        }
        if mimes.iter().any(|m| m == MARKER) {
            offer.destroy(); // our own selection
            return;
        }
        self.send(Event::Selection(Some(mimes.clone())));
        self.selection = Some((offer, mimes));
    }

    fn command(&mut self, cmd: Cmd, qh: &QueueHandle<Self>) {
        match cmd {
            Cmd::Offer(Some(mimes)) => {
                let (Some(manager), Some(device)) = (&self.manager, &self.device) else {
                    return;
                };
                let source = match manager {
                    Manager::Ext(m) => Source::Ext(m.create_data_source(qh, ())),
                    Manager::Wlr(m) => Source::Wlr(m.create_data_source(qh, ())),
                };
                for m in mimes {
                    source.offer(m);
                }
                source.offer(MARKER.into());
                match (device, &source) {
                    (Device::Ext(d), Source::Ext(s)) => d.set_selection(Some(s)),
                    (Device::Wlr(d), Source::Wlr(s)) => d.set_selection(Some(s)),
                    _ => unreachable!("one protocol at a time"),
                }
                if let Some(old) = self.source.replace(source) {
                    old.destroy();
                }
            }
            Cmd::Offer(None) => {
                // Only when the clipboard is still ours.
                if let Some(source) = self.source.take() {
                    match &self.device {
                        Some(Device::Ext(d)) => d.set_selection(None),
                        Some(Device::Wlr(d)) => d.set_selection(None),
                        None => {}
                    }
                    source.destroy();
                }
            }
            Cmd::Read { mime, reply } => {
                let Some((offer, mimes)) = &self.selection else {
                    let _ = reply.send(Err("the guest clipboard is empty".into()));
                    return;
                };
                if !mimes.contains(&mime) {
                    let _ = reply.send(Err(format!("{mime} is not offered")));
                    return;
                }
                let (read, write) = match rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)
                {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = reply.send(Err(e.to_string()));
                        return;
                    }
                };
                offer.receive(mime, write.as_fd());
                drop(write); // the owner has its copy, sent at the flush
                std::thread::spawn(move || {
                    let mut data = Vec::new();
                    let res = std::fs::File::from(read)
                        .take(MAX_READ as u64 + 1)
                        .read_to_end(&mut data)
                        .map_err(|e| e.to_string())
                        .and_then(|n| {
                            if n > MAX_READ {
                                Err("guest clipboard data too large".into())
                            } else {
                                Ok(data)
                            }
                        });
                    let _ = reply.send(res);
                });
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_seat" if state.seat.is_none() => {
                    state.seat = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_output" => {
                    let wl = registry.bind(name, version.min(4), qh, name);
                    let order = state.next_order;
                    state.next_order += 1;
                    state.outputs.push(Output {
                        wl: Some(wl),
                        global: name,
                        order,
                        scale: 1,
                        ..Output::default()
                    });
                    let i = state.outputs.len() - 1;
                    state.bind_xdg(i, qh);
                }
                "zxdg_output_manager_v1" => {
                    state.xdg_manager = Some(registry.bind(name, version.min(3), qh, ()));
                }
                "ext_data_control_manager_v1" => {
                    state.manager = Some(Manager::Ext(registry.bind(name, 1, qh, ())));
                }
                "zwlr_data_control_manager_v1"
                    if !matches!(state.manager, Some(Manager::Ext(_))) =>
                {
                    state.manager = Some(Manager::Wlr(registry.bind(name, version.min(2), qh, ())));
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(i);
                    if let Some(x) = o.xdg {
                        x.destroy();
                    }
                    if let Some(wl) = o.wl {
                        wl.release();
                    }
                    state.layout_changed();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output(*global) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                o.position = (x, y);
                o.rotated = matches!(
                    transform,
                    WEnum::Value(
                        wl_output::Transform::_90
                            | wl_output::Transform::_270
                            | wl_output::Transform::Flipped90
                            | wl_output::Transform::Flipped270
                    )
                );
            }
            wl_output::Event::Mode {
                flags,
                width,
                height,
                ..
            } => {
                if let WEnum::Value(f) = flags
                    && f.contains(wl_output::Mode::Current)
                {
                    o.mode = (width.max(0) as u32, height.max(0) as u32);
                }
            }
            wl_output::Event::Scale { factor } => o.scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            wl_output::Event::Done => state.layout_changed(),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output(*global) else {
            return;
        };
        let mut l = o.logical.unwrap_or_default();
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                l.0 = x;
                l.1 = y;
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                l.2 = width.max(0) as u32;
                l.3 = height.max(0) as u32;
            }
            zxdg_output_v1::Event::Name { name } => {
                if o.name.is_none() {
                    o.name = Some(name);
                }
                return;
            }
            // Version 3 sends wl_output.done instead.
            zxdg_output_v1::Event::Done => {
                state.layout_changed();
                return;
            }
            _ => return,
        }
        o.logical = Some(l);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heads_from_connector_names() {
        let o = |name: &str, order| Output {
            name: Some(name.into()),
            order,
            mode: (1920, 1080),
            scale: 1,
            ..Output::default()
        };
        assert_eq!(o("Virtual-1", 5).head(), 0);
        assert_eq!(o("Virtual-3", 0).head(), 2);
        assert_eq!(o("HDMI-A-1", 4).head(), 4);
    }

    #[test]
    fn logical_size_without_xdg_output() {
        let o = Output {
            name: Some("Virtual-2".into()),
            position: (1920, 0),
            mode: (2560, 1440),
            scale: 2,
            ..Output::default()
        };
        assert_eq!(o.to_head(), Some((1, 1920, 0, 1280, 720, 2560, 1440)));
        let mut r = o;
        r.rotated = true;
        r.logical = Some((0, 0, 720, 1280));
        assert_eq!(r.to_head(), Some((1, 0, 0, 720, 1280, 2560, 1440)));
    }
}
