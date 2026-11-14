// SPDX-License-Identifier: GPL-3.0-or-later

//! The system daemon: owns the virtio-serial port, serves the host's D-Bus
//! session on it, injects the pointer, and hands the rest to the session
//! agent of the user in front of the screen.

mod agents;
mod host;
mod logind;
mod mouse;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use proto::AGENT_PATH;
use tokio::sync::mpsc;
use zbus::{AuthMechanism, Connection};

use crate::channel::port::{Port, PortEvent};
use crate::channel::transport;
use agents::Agent;
use host::{
    AgentIface, ClipboardIface, DisplayIface, FileClipboardIface, FileTransferIface, MouseIface,
};
use mouse::MouseHandle;

/// Longest wait for one call to the host or to a session agent.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(55);

const STATE_DIR: &str = "/var/lib/ameleo-display-agent";

/// Desktop effects the host turned off, `true` meaning disabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Effects {
    pub no_wallpaper: bool,
    pub no_font_smoothing: bool,
    pub no_animation: bool,
}

impl Effects {
    fn path() -> std::path::PathBuf {
        std::path::Path::new(STATE_DIR).join("effects")
    }

    /// Kept across reboots, applied again at every logon.
    fn load() -> Self {
        let text = std::fs::read_to_string(Self::path()).unwrap_or_default();
        let on = |key: &str| text.lines().any(|l| l.trim() == key);
        Self {
            no_wallpaper: on("no-wallpaper"),
            no_font_smoothing: on("no-font-smoothing"),
            no_animation: on("no-animation"),
        }
    }

    fn save(self) {
        let mut text = String::new();
        for (on, key) in [
            (self.no_wallpaper, "no-wallpaper"),
            (self.no_font_smoothing, "no-font-smoothing"),
            (self.no_animation, "no-animation"),
        ] {
            if on {
                text.push_str(key);
                text.push('\n');
            }
        }
        let res =
            std::fs::create_dir_all(STATE_DIR).and_then(|()| std::fs::write(Self::path(), text));
        if let Err(e) = res {
            tracing::warn!("cannot save the desktop effects: {e}");
        }
    }
}

/// Clipboard requests to the active session agent, in the host's order.
enum ClipCmd {
    Grab {
        serial: u32,
        mimes: Vec<String>,
    },
    Release,
    /// The active agent changed.
    Activated {
        old: Option<Arc<Agent>>,
        new: Option<Arc<Agent>>,
    },
}

/// A host file transfer, forwarded to one session agent.
struct Transfer {
    agent: Arc<Agent>,
    id: u32,
}

pub struct Shared {
    pub mouse: MouseHandle,
    /// The active session is locked, or no user session is active.
    pub locked: AtomicBool,
    pub max_clipboard: AtomicI32,
    /// The seat0 session in front of the screen.
    active: Mutex<logind::Active>,
    logind: Option<Arc<logind::Logind>>,
    /// Connected session agents, oldest first.
    agents: Mutex<Vec<Arc<Agent>>>,
    /// The agent that gets the host's requests.
    current: Mutex<Option<Arc<Agent>>>,
    /// The host D-Bus session, once established.
    host: Mutex<Option<Connection>>,
    /// The host's clipboard grab, given again to a new active agent.
    host_grab: Mutex<Option<(u32, Vec<String>)>>,
    /// The last guest clipboard signal sent to the host was `Released`:
    /// another one would say nothing new. Cleared when the host session or
    /// the active agent changes, so that a real change is always sent.
    guest_released: AtomicBool,
    effects: Mutex<Effects>,
    transfers: Mutex<HashMap<u32, Transfer>>,
    next_transfer: AtomicU32,
    clip: mpsc::UnboundedSender<ClipCmd>,
}

impl Shared {
    fn new(
        mouse: MouseHandle,
        logind: Option<Arc<logind::Logind>>,
    ) -> (Arc<Self>, mpsc::UnboundedReceiver<ClipCmd>) {
        let (clip, clip_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Self {
            mouse,
            locked: AtomicBool::new(true),
            max_clipboard: AtomicI32::new(-1),
            active: Mutex::new(logind::Active {
                locked: true,
                ..Default::default()
            }),
            logind,
            agents: Mutex::new(Vec::new()),
            current: Mutex::new(None),
            host: Mutex::new(None),
            host_grab: Mutex::new(None),
            guest_released: AtomicBool::new(false),
            effects: Mutex::new(Effects::load()),
            transfers: Mutex::new(HashMap::new()),
            next_transfer: AtomicU32::new(0),
            clip,
        });
        (shared, clip_rx)
    }

    pub fn host(&self) -> Option<Connection> {
        self.host.lock().unwrap().clone()
    }

    pub fn current(&self) -> Option<Arc<Agent>> {
        self.current.lock().unwrap().clone()
    }

    /// Per host session state goes away with the D-Bus session or the
    /// client.
    pub fn reset(&self) {
        *self.host_grab.lock().unwrap() = None;
        self.transfers.lock().unwrap().clear();
        self.max_clipboard.store(-1, Ordering::Relaxed);
        self.mouse.send(mouse::Cmd::ReleaseAll);
        for agent in self.agents.lock().unwrap().iter() {
            let agent = agent.clone();
            tokio::spawn(async move {
                if let Err(e) = agent.session().await.reset().await {
                    tracing::debug!(agent = agent.id, "reset not delivered: {e}");
                }
            });
        }
    }

    /// The client went away: also lock the user's session.
    pub fn reset_client(&self) {
        self.reset();
        let active = self.active.lock().unwrap().clone();
        if active.locked || active.id.is_empty() {
            return;
        }
        if let Some(logind) = self.logind.clone() {
            tokio::spawn(async move {
                match logind.lock(&active.id).await {
                    Ok(()) => tracing::info!(session = active.id, "session locked"),
                    Err(e) => tracing::warn!("cannot lock session {}: {e}", active.id),
                }
            });
        }
    }

    /// Picks the agent of the active user (the newest one) and tells
    /// whoever needs to know when it changed.
    fn refresh_current(&self) {
        let uid = self.active.lock().unwrap().uid;
        let new = self
            .agents
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|a| Some(a.uid) == uid)
            .cloned();
        let old = {
            let mut current = self.current.lock().unwrap();
            if current.as_ref().map(|a| a.id) == new.as_ref().map(|a| a.id) {
                return;
            }
            std::mem::replace(&mut *current, new.clone())
        };
        tracing::info!(
            old = old.as_ref().map(|a| a.id),
            new = new.as_ref().map(|a| a.id),
            "active session agent"
        );
        self.mouse.send(mouse::Cmd::Layout(
            new.as_ref()
                .map(|a| a.layout.lock().unwrap().clone())
                .unwrap_or_default(),
        ));
        self.guest_released.store(false, Ordering::Relaxed);
        let _ = self.clip.send(ClipCmd::Activated { old, new });
    }

    fn set_effects(&self, effects: Effects) {
        *self.effects.lock().unwrap() = effects;
        effects.save();
        for agent in self.agents.lock().unwrap().iter() {
            agent.send_effects(effects);
        }
    }

    fn add_transfer(&self, agent: Arc<Agent>, id: u32) -> u32 {
        let ours = self
            .next_transfer
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        self.transfers
            .lock()
            .unwrap()
            .insert(ours, Transfer { agent, id });
        ours
    }

    fn transfer(&self, id: u32) -> Option<(Arc<Agent>, u32)> {
        self.transfers
            .lock()
            .unwrap()
            .get(&id)
            .map(|t| (t.agent.clone(), t.id))
    }

    /// Tells the host that `SessionLocked` changed.
    async fn emit_locked(&self) {
        let Some(conn) = self.host() else { return };
        if let Ok(iface) = conn
            .object_server()
            .interface::<_, AgentIface>(AGENT_PATH)
            .await
        {
            let _ = iface
                .get()
                .await
                .session_locked_changed(iface.signal_emitter())
                .await;
        }
    }

    /// Sends the guest clipboard signals to the host.
    async fn emit_clipboard(&self, grab: Option<(u32, Vec<String>)>) {
        let Some(conn) = self.host() else {
            tracing::debug!("guest clipboard change not sent: no host session");
            return;
        };
        let Ok(iface) = conn
            .object_server()
            .interface::<_, ClipboardIface>(AGENT_PATH)
            .await
        else {
            return;
        };
        let released = grab.is_none();
        if self.guest_released.swap(released, Ordering::Relaxed) && released {
            tracing::debug!("guest clipboard still empty: Released not sent again");
            return;
        }
        let res = match grab {
            Some((serial, mimes)) => {
                tracing::info!(serial, ?mimes, "signal Grabbed sent to the host");
                ClipboardIface::grabbed(iface.signal_emitter(), serial, mimes).await
            }
            None => {
                tracing::info!("signal Released sent to the host");
                ClipboardIface::released(iface.signal_emitter()).await
            }
        };
        if let Err(e) = res {
            tracing::warn!("cannot send the clipboard signal: {e}");
        }
    }
}

/// Forwards the host's clipboard grabs to the active agent, one at a time,
/// so that a release never overtakes its grab.
async fn clip_forwarder(shared: Arc<Shared>, mut rx: mpsc::UnboundedReceiver<ClipCmd>) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            ClipCmd::Grab { serial, mimes } => {
                if let Some(agent) = shared.current() {
                    let res = agent.session().await.clipboard_grab(serial, mimes).await;
                    if let Err(e) = res {
                        tracing::warn!(agent = agent.id, "clipboard grab not delivered: {e}");
                    }
                }
            }
            ClipCmd::Release => {
                if let Some(agent) = shared.current() {
                    let _ = agent.session().await.clipboard_release().await;
                }
            }
            ClipCmd::Activated { old, new } => {
                if let Some(old) = old {
                    let _ = old.session().await.clipboard_release().await;
                }
                let Some(new) = new else { continue };
                let grab = shared.host_grab.lock().unwrap().clone();
                let session = new.session().await;
                if let Some((serial, mimes)) = grab {
                    let _ = session.clipboard_grab(serial, mimes).await;
                }
                if shared.host().is_some() {
                    let _ = session.announce().await;
                }
            }
        }
    }
}

struct HostSession {
    generation: u64,
    data: mpsc::UnboundedSender<Vec<u8>>,
    established: bool,
}

pub async fn run() -> Result<()> {
    let mouse =
        mouse::spawn().map_err(|e| anyhow::anyhow!("cannot create the uinput pointer: {e}"))?;
    let logind = match logind::Logind::connect().await {
        Ok(l) => Some(Arc::new(l)),
        Err(e) => {
            tracing::warn!("no logind, every session counts as locked: {e}");
            None
        }
    };
    let (shared, clip_rx) = Shared::new(mouse, logind.clone());
    tokio::spawn(clip_forwarder(shared.clone(), clip_rx));
    agents::listen(shared.clone(), &crate::ipc::socket())?;

    let (active_tx, mut active_rx) = mpsc::unbounded_channel();
    if let Some(logind) = logind {
        logind::watch(logind, active_tx);
    }

    let (port, mut events, out) = loop {
        match Port::open(proto::PORT_NAME) {
            Ok(p) => break p,
            Err(e) => {
                tracing::warn!(
                    "cannot open {}: {e}, retrying",
                    crate::channel::port::path(proto::PORT_NAME).display()
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    };
    tracing::info!("port {} opened", proto::PORT_NAME);

    let (conn_tx, mut conn_rx) = mpsc::unbounded_channel::<(u64, zbus::Result<Connection>)>();
    let mut session: Option<HostSession> = None;
    let mut generation = 0u64;
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(PortEvent::NewSession) => {
                    generation += 1;
                    port.generation.store(generation, Ordering::Release);
                    if session.is_some() {
                        tracing::info!("new D-Bus handshake {generation}, dropping the previous session");
                    } else {
                        tracing::info!("D-Bus handshake {generation} from the host");
                    }
                    *shared.host.lock().unwrap() = None;
                    shared.reset();
                    let (split, data) = transport::session(generation, out.clone());
                    session = Some(HostSession { generation, data, established: false });
                    let shared = shared.clone();
                    let conn_tx = conn_tx.clone();
                    let g = generation;
                    tokio::spawn(async move {
                        let _ = conn_tx.send((g, build(split, shared).await));
                    });
                }
                Some(PortEvent::Data(bytes)) => {
                    if let Some(s) = &session {
                        let _ = s.data.send(bytes);
                    }
                }
                Some(PortEvent::HostDisconnected) => {
                    tracing::info!("host disconnected, session {generation} closed");
                    generation += 1;
                    port.generation.store(generation, Ordering::Release);
                    *shared.host.lock().unwrap() = None;
                    if session.take().is_some() {
                        shared.reset();
                    }
                }
                None => anyhow::bail!("port reader stopped"),
            },
            Some((g, res)) = conn_rx.recv() => {
                match (&mut session, res) {
                    (Some(s), Ok(conn)) if s.generation == g => {
                        tracing::info!("D-Bus session {g} established");
                        s.established = true;
                        *shared.host.lock().unwrap() = Some(conn);
                        shared.guest_released.store(false, Ordering::Relaxed);
                        // The guest clipboard, for the new host.
                        if let Some(agent) = shared.current() {
                            tokio::spawn(async move {
                                let _ = agent.session().await.announce().await;
                            });
                        }
                    }
                    (Some(s), Err(e)) if s.generation == g => {
                        tracing::warn!("D-Bus handshake {g} failed: {e}");
                        session = None;
                    }
                    _ => {} // stale
                }
            },
            Some(active) = active_rx.recv() => {
                let locked = active.locked;
                *shared.active.lock().unwrap() = active;
                if shared.locked.swap(locked, Ordering::Relaxed) != locked {
                    tracing::info!(locked, "session lock state");
                    let shared = shared.clone();
                    tokio::spawn(async move { shared.emit_locked().await });
                }
                shared.refresh_current();
            },
            _ = sigterm.recv() => {
                tracing::info!("stopping");
                shared.mouse.send(mouse::Cmd::ReleaseAll);
                // Let the pointer thread release the buttons.
                tokio::time::sleep(Duration::from_millis(50)).await;
                return Ok(());
            },
        }
    }
}

async fn build(split: transport::Split, shared: Arc<Shared>) -> zbus::Result<Connection> {
    zbus::connection::Builder::socket(split)
        .server(zbus::Guid::generate())?
        .p2p()
        .auth_mechanism(AuthMechanism::Anonymous)
        .serve_at(AGENT_PATH, AgentIface(shared.clone()))?
        .serve_at(AGENT_PATH, MouseIface(shared.clone()))?
        .serve_at(AGENT_PATH, ClipboardIface(shared.clone()))?
        .serve_at(AGENT_PATH, FileClipboardIface(shared.clone()))?
        .serve_at(AGENT_PATH, DisplayIface(shared.clone()))?
        .serve_at(AGENT_PATH, FileTransferIface(shared))?
        .build()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::framer::{FrameOut, Framer};

    /// A host client talking to the daemon through the framer and the
    /// transport, as on the real port.
    /// The host's clipboard: always `text`.
    struct FakeHost(String);

    #[zbus::interface(name = "fr.ameleo.Host.Clipboard1")]
    impl FakeHost {
        fn request(&self, _serial: u32, mimes: Vec<String>) -> (String, Vec<u8>) {
            if mimes[0] == proto::mime::FILE_LIST {
                // One file, `hello.txt`, whose content is the text.
                let entry = crate::session::filelist::Entry {
                    name: "hello.txt".into(),
                    attributes: 0x20,
                    size: self.0.len() as u64,
                    write_time: 0,
                    path: Default::default(),
                };
                return (mimes[0].clone(), crate::session::filelist::encode(&[entry]));
            }
            (mimes[0].clone(), self.0.as_bytes().to_vec())
        }
    }

    /// The host's files: the text of [`FakeHost`], as file 0.
    struct FakeHostFiles(String);

    #[zbus::interface(name = "fr.ameleo.Host.FileClipboard1")]
    impl FakeHostFiles {
        fn lock_clip_data(&self, _id: u32) {}
        fn unlock_clip_data(&self, _id: u32) {}
        fn file_contents(
            &self,
            _lindex: i32,
            _flags: u32,
            position: u64,
            requested: u32,
            _clip_data_id: u32,
            _has_clip_data_id: bool,
        ) -> Vec<u8> {
            let data = self.0.as_bytes();
            let start = (position as usize).min(data.len());
            let end = (start + requested as usize).min(data.len());
            data[start..end].to_vec()
        }
    }

    async fn host_client(shared: Arc<Shared>) -> Connection {
        let (g_out_tx, mut g_out_rx) = mpsc::channel(64);
        let (c_out_tx, mut c_out_rx) = mpsc::channel(64);
        let (gsplit, g_in) = transport::session(1, g_out_tx);
        let (csplit, c_in) = transport::session(1, c_out_tx);
        tokio::spawn(async move {
            let mut framer = Framer::default();
            while let Some((_, bytes)) = c_out_rx.recv().await {
                framer.feed(&bytes, |o| {
                    if let FrameOut::Bytes(b) = o {
                        let _ = g_in.send(b.to_vec());
                    }
                });
            }
        });
        tokio::spawn(async move {
            while let Some((_, bytes)) = g_out_rx.recv().await {
                let _ = c_in.send(bytes);
            }
        });
        let server = tokio::spawn(build(gsplit, shared.clone()));
        let client = zbus::connection::Builder::socket(csplit)
            .p2p()
            .auth_mechanism(AuthMechanism::Anonymous)
            .serve_at(proto::HOST_PATH, FakeHost("hello from the host".into()))
            .unwrap()
            .serve_at(
                proto::HOST_PATH,
                FakeHostFiles("hello from the host".into()),
            )
            .unwrap()
            .build()
            .await
            .unwrap();
        let conn = server.await.unwrap().unwrap();
        *shared.host.lock().unwrap() = Some(conn);
        client
    }

    #[tokio::test]
    async fn host_to_session_agent() {
        let (shared, clip_rx) = Shared::new(MouseHandle::detached(), None);
        tokio::spawn(clip_forwarder(shared.clone(), clip_rx));
        let host = host_client(shared.clone()).await;
        let cache = zbus::proxy::CacheProperties::No;

        let agent = proto::AgentProxy::builder(&host)
            .cache_properties(cache)
            .build()
            .await
            .unwrap();
        assert_eq!(agent.version().await.unwrap(), env!("CARGO_PKG_VERSION"));
        assert!(
            agent
                .channels()
                .await
                .unwrap()
                .contains(&proto::iface::FILE_TRANSFER.to_string())
        );
        // No user session yet: locked.
        assert!(agent.session_locked().await.unwrap());

        let cb = proto::ClipboardProxy::builder(&host)
            .cache_properties(cache)
            .build()
            .await
            .unwrap();
        cb.set_max_size(4096).await.unwrap();
        assert_eq!(cb.max_size().await.unwrap(), 4096);
        let err = cb
            .request(vec![proto::mime::TEXT.into()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("SessionLocked"), "{err}");

        let ft = proto::FileTransferProxy::builder(&host)
            .cache_properties(cache)
            .build()
            .await
            .unwrap();
        let err = ft.write(42, vec![0; 10]).await.unwrap_err();
        assert!(err.to_string().contains("NotFound"), "{err}");

        // The user logs in, its session agent connects.
        let uid = rustix::process::getuid().as_raw();
        *shared.active.lock().unwrap() = logind::Active {
            id: "1".into(),
            uid: Some(uid),
            locked: false,
        };
        shared.locked.store(false, Ordering::Relaxed);
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        tokio::spawn(agents::serve(shared.clone(), 1, a));
        let _agent_conn = crate::session::test_agent(b, Some(vec!["text/plain".into()])).await;
        for _ in 0..100 {
            if shared.current().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(shared.current().is_some(), "agent not active");

        // The request reaches the agent, whose error kind comes back.
        let err = cb
            .request(vec![proto::mime::TEXT.into()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("NoData"), "{err}");
        assert!(err.to_string().contains("no clipboard access"), "{err}");

        // Bad names are refused by the agent, as InvalidArgument.
        let err = ft.start("../x", 1).await.unwrap_err();
        assert!(err.to_string().contains("InvalidArgument"), "{err}");
    }

    #[tokio::test]
    async fn released_is_not_repeated() {
        use futures_util::StreamExt;
        let (shared, _clip_rx) = Shared::new(MouseHandle::detached(), None);
        let host = host_client(shared.clone()).await;
        let cb = proto::ClipboardProxy::builder(&host)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        let mut released = cb.receive_released().await.unwrap();
        let mut grabbed = cb.receive_grabbed().await.unwrap();
        shared.emit_clipboard(None).await;
        shared.emit_clipboard(None).await;
        shared
            .emit_clipboard(Some((1, vec![proto::mime::TEXT.into()])))
            .await;
        shared.emit_clipboard(None).await;
        // A new active agent says it again, even if empty.
        shared.guest_released.store(false, Ordering::Relaxed);
        shared.emit_clipboard(None).await;
        let wait = Duration::from_millis(200);
        let mut count = 0;
        while tokio::time::timeout(wait, released.next()).await.is_ok() {
            count += 1;
        }
        assert_eq!(count, 3);
        assert!(tokio::time::timeout(wait, grabbed.next()).await.is_ok());
    }

    /// The real session agent on this desktop: `cargo build && cargo test -- --ignored`
    /// in a Wayland session with wl-clipboard. Changes the clipboard.
    #[tokio::test]
    #[ignore]
    async fn live_session_agent() {
        use futures_util::StreamExt;
        let (shared, clip_rx) = Shared::new(MouseHandle::detached(), None);
        tokio::spawn(clip_forwarder(shared.clone(), clip_rx));
        let host = host_client(shared.clone()).await;
        *shared.active.lock().unwrap() = logind::Active {
            id: "1".into(),
            uid: Some(rustix::process::getuid().as_raw()),
            locked: false,
        };
        shared.locked.store(false, Ordering::Relaxed);
        let sock = std::env::temp_dir().join(format!("ada-live-{}.sock", std::process::id()));
        agents::listen(shared.clone(), &sock).unwrap();
        let runtime = std::env::temp_dir().join(format!("ada-live-run-{}", std::process::id()));
        std::fs::create_dir_all(&runtime).unwrap();
        let wayland = std::path::Path::new(&std::env::var_os("XDG_RUNTIME_DIR").unwrap())
            .join(std::env::var_os("WAYLAND_DISPLAY").unwrap());
        let mut child = tokio::process::Command::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/target/debug/ameleo-display-agent"
        ))
        .arg("session")
        .env("AMELEO_DISPLAY_AGENT_SOCKET", &sock)
        // Its own runtime directory, for its FUSE mount; the compositor
        // by its full path.
        .env("XDG_RUNTIME_DIR", &runtime)
        .env("WAYLAND_DISPLAY", &wayland)
        .env("AMELEO_DISPLAY_AGENT_LOG", "debug")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
        for _ in 0..300 {
            if shared
                .current()
                .is_some_and(|a| !a.layout.lock().unwrap().is_empty())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let agent = shared.current().expect("session agent connected");
        let layout = agent.layout.lock().unwrap().clone();
        eprintln!("layout: {layout:?}");
        assert!(!layout.is_empty());

        // Host to guest: the host copies, a guest app pastes.
        let cb = proto::ClipboardProxy::builder(&host)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        let mut grabbed = cb.receive_grabbed().await.unwrap();
        cb.grab(7, vec![proto::mime::TEXT.into()]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let out = tokio::process::Command::new("wl-paste")
            .args(["-n", "-t", "text/plain;charset=utf-8"])
            .output()
            .await
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello from the host");

        // Guest to host: a guest app copies, the host is told and reads it.
        // wl-copy stays in the background to serve the copy: no pipes.
        let status = tokio::process::Command::new("wl-copy")
            .arg("hello from the guest")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let sig = tokio::time::timeout(Duration::from_secs(3), grabbed.next())
            .await
            .expect("Grabbed signal")
            .unwrap();
        eprintln!("grabbed: {:?}", sig.args().unwrap().mimes);
        let (mime, data) = cb.request(vec![proto::mime::TEXT.into()]).await.unwrap();
        assert_eq!(mime, proto::mime::TEXT);
        assert_eq!(String::from_utf8_lossy(&data), "hello from the guest");

        // Host files pasted in the guest, through FUSE.
        cb.grab(8, vec![proto::mime::FILE_LIST.into()])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let out = tokio::process::Command::new("wl-paste")
            .args(["-n", "-t", "text/uri-list"])
            .output()
            .await
            .unwrap();
        let paths = crate::session::filelist::parse_uri_list(&out.stdout);
        eprintln!("pasted: {paths:?}");
        assert_eq!(paths.len(), 1);
        assert!(paths[0].starts_with(&runtime));
        let content = tokio::fs::read_to_string(&paths[0]).await.unwrap();
        assert_eq!(content, "hello from the host");
        // Writable once copied: file managers keep the mode.
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&paths[0]).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644);

        // Guest files copied for the host.
        let dir = std::env::temp_dir().join(format!("ada-live-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("g.txt"), b"guest file").unwrap();
        let uri = crate::session::filelist::file_uri(&dir.join("g.txt"));
        let status = tokio::process::Command::new("wl-copy")
            .args(["-t", "text/uri-list", &uri])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let sig = tokio::time::timeout(Duration::from_secs(3), grabbed.next())
            .await
            .expect("Grabbed signal")
            .unwrap();
        assert!(
            sig.args()
                .unwrap()
                .mimes
                .contains(&proto::mime::FILE_LIST.to_string())
        );
        let (_, list) = cb
            .request(vec![proto::mime::FILE_LIST.into()])
            .await
            .unwrap();
        let entries = crate::session::filelist::decode(&list).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "g.txt");
        let fc = proto::FileClipboardProxy::builder(&host)
            .cache_properties(zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        fc.lock_clip_data(1).await.unwrap();
        let data = fc
            .file_contents(0, proto::file_contents::RANGE, 6, 100, 1, true)
            .await
            .unwrap();
        assert_eq!(data, b"file");
        fc.unlock_clip_data(1).await.unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let _ = child.kill().await;
        let _ = child.wait().await;
        let _ = std::fs::remove_file(&sock);
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z"])
            .arg(runtime.join("ameleo-display-agent/clipboard"))
            .status();
        let _ = std::fs::remove_dir_all(&runtime);
        let _ = std::process::Command::new("wl-copy")
            .arg("--clear")
            .status();
    }
}
