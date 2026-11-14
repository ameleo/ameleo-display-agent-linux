// SPDX-License-Identifier: GPL-3.0-or-later

//! The session agent: runs as the user of a graphical session, connects to
//! the daemon ([`crate::ipc::SOCKET`]) and does what needs the user's
//! desktop: the clipboard, the monitor layout, files sent by the host,
//! desktop effects.

pub(crate) mod camera;
mod clipboard;
mod effects;
pub(crate) mod filelist;
mod filexfer;
mod gnome;
mod hostfiles;
mod wayland;

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::Result;
use proto::Error;
use tokio::sync::mpsc;
use zbus::proxy::CacheProperties;

use crate::ipc::{DaemonProxy, SESSION_PATH};
use wayland::Event;

/// Where the guest clipboard is.
#[derive(Clone)]
enum Clip {
    /// A data control protocol: types offered, data given on paste.
    Wayland(wayland::Handle),
    /// The GNOME Shell extension: data given with the selection.
    Gnome(gnome::Handle),
    None,
}

impl Clip {
    async fn read(&self, mime: &str) -> Result<Vec<u8>, String> {
        match self {
            Clip::Wayland(w) => w.read(mime).await,
            Clip::Gnome(g) => g.read(mime).await,
            Clip::None => Err("no clipboard access on this desktop".into()),
        }
    }
}

struct State {
    daemon: OnceLock<DaemonProxy<'static>>,
    clip: Clip,
    /// The host's clipboard grab: its serial and types.
    host: Mutex<Option<(u32, Vec<String>)>>,
    /// The types of the guest selection, when a guest app owns it.
    guest: Mutex<Option<Vec<String>>>,
    serial: AtomicU32,
    xfer: Mutex<filexfer::FileXfer>,
    /// Files copied in the guest, for the host.
    files: Mutex<filelist::GuestFiles>,
    open_file: filelist::OpenFile,
    /// Host files pasted in the guest; unset when FUSE is not available.
    host_files: OnceLock<hostfiles::HostFiles>,
    /// The host grab whose files are in `host_files`, and their paths.
    /// Held while the list is fetched: a paste in several types fetches it
    /// once.
    pasted: tokio::sync::Mutex<Option<(u32, Vec<PathBuf>)>>,
    /// The next `LockClipData` id for the host's lists.
    next_lock: AtomicU32,
    /// The client cameras; unset without PipeWire.
    cameras: OnceLock<camera::Cameras>,
}

impl State {
    fn new(clip: Clip, guest: Option<Vec<String>>) -> Self {
        Self {
            daemon: OnceLock::new(),
            clip,
            host: Mutex::new(None),
            guest: Mutex::new(guest),
            serial: AtomicU32::new(0),
            xfer: Mutex::new(filexfer::FileXfer::default()),
            files: Mutex::new(filelist::GuestFiles::default()),
            open_file: Mutex::new(None),
            host_files: OnceLock::new(),
            pasted: tokio::sync::Mutex::new(None),
            next_lock: AtomicU32::new(0),
            cameras: OnceLock::new(),
        }
    }

    fn daemon(&self) -> &DaemonProxy<'static> {
        self.daemon.get().expect("daemon proxy set at connection")
    }

    /// Tells the host what the guest clipboard holds now.
    async fn announce(&self) {
        let guest = self.guest.lock().unwrap().clone();
        let exported = guest
            .as_deref()
            .map(clipboard::exportable)
            .unwrap_or_default();
        let res = if exported.is_empty() {
            self.daemon().clipboard_released().await
        } else {
            let serial = self.serial.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            tracing::info!(serial, ?exported, "guest clipboard grabbed");
            // A new guest copy: the previous file list is not current
            // anymore, and its last file need not stay open.
            self.files.lock().unwrap().clipboard_changed(serial);
            *self.open_file.lock().unwrap() = None;
            self.daemon().clipboard_grabbed(serial, exported).await
        };
        if let Err(e) = res {
            tracing::warn!("cannot tell the daemon about the clipboard: {e}");
        }
    }

    /// Host data for a guest app pasting `guest_mime`, converted.
    async fn host_data(&self, guest_mime: &str) -> Result<Vec<u8>, String> {
        let (serial, host) = self
            .host
            .lock()
            .unwrap()
            .clone()
            .ok_or("the host does not own the clipboard")?;
        let types = clipboard::host_types_for(guest_mime, &host);
        if types.is_empty() {
            return Err(format!("the host has nothing for {guest_mime}"));
        }
        if clipboard::is_file_list(guest_mime) {
            let paths = self.host_file_paths(serial).await?;
            return Ok(clipboard::file_list(guest_mime, &paths));
        }
        let (mime, data) = self
            .daemon()
            .host_clipboard_request(serial, types)
            .await
            .map_err(|e| e.to_string())?;
        clipboard::convert(&mime, guest_mime, data)
    }

    /// The host files of grab `serial`, in the FUSE file system: locks the
    /// host's content, fetches its list, mounts it.
    async fn host_file_paths(&self, serial: u32) -> Result<Vec<PathBuf>, String> {
        let mut pasted = self.pasted.lock().await;
        if let Some((s, paths)) = &*pasted
            && *s == serial
        {
            return Ok(paths.clone());
        }
        let files = self
            .host_files
            .get()
            .ok_or("host files cannot be pasted: no FUSE")?;
        // The lock first: it freezes the content the list is about.
        let lock = self
            .next_lock
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let daemon = self.daemon();
        daemon
            .host_lock_clip_data(lock)
            .await
            .map_err(|e| e.to_string())?;
        let res = async {
            let (_, data) = daemon
                .host_clipboard_request(serial, vec![proto::mime::FILE_LIST.into()])
                .await
                .map_err(|e| e.to_string())?;
            filelist::decode(&data)
        }
        .await;
        let entries = match res {
            Ok(e) => e,
            Err(e) => {
                let _ = daemon.host_unlock_clip_data(lock).await;
                return Err(e);
            }
        };
        let paths = files.add(lock, &entries);
        tracing::info!(
            serial,
            lock,
            files = entries.len(),
            "host files ready to paste"
        );
        *pasted = Some((serial, paths.clone()));
        Ok(paths)
    }

    /// The CLIPRDR_FILELIST of the files copied in the guest, built once
    /// per guest copy (big trees take a while).
    async fn guest_file_list(&self, source: &str) -> Result<Vec<u8>, Error> {
        let serial = self.serial.load(Ordering::Relaxed);
        let cached = self.files.lock().unwrap().cached(serial);
        let entries = match cached {
            Some(e) => e,
            None => {
                let uris = self.clip.read(source).await.map_err(Error::NoData)?;
                let paths = filelist::parse_uri_list(&uris);
                if paths.is_empty() {
                    return Err(Error::NoData("no local files on the clipboard".into()));
                }
                let entries = tokio::task::spawn_blocking(move || filelist::walk(&paths))
                    .await
                    .map_err(|e| Error::Failed(e.to_string()))??;
                tracing::info!(serial, files = entries.len(), "guest file list built");
                let entries = Arc::new(entries);
                self.files
                    .lock()
                    .unwrap()
                    .set_current(serial, entries.clone());
                entries
            }
        };
        Ok(filelist::encode(&entries))
    }

    /// GNOME: the host copied, put its data on the clipboard now.
    async fn set_gnome(&self, gnome: &gnome::Handle, serial: u32, host: &[String]) {
        let files =
            self.host_files.get().is_some() && host.iter().any(|m| m == proto::mime::FILE_LIST);
        let mime = if files {
            clipboard::GUEST_FILES[1]
        } else if host.iter().any(|m| clipboard::is_text(m)) {
            clipboard::GUEST_TEXT[0]
        } else {
            "image/png"
        };
        match self.host_data(mime).await {
            // A newer grab came meanwhile: it sets its own.
            Ok(_) if self.host.lock().unwrap().as_ref().map(|h| h.0) != Some(serial) => {}
            Ok(data) => {
                if let Err(e) = gnome.set(mime, data).await {
                    tracing::warn!("cannot set the GNOME clipboard: {e}");
                }
            }
            Err(e) => tracing::warn!("cannot fetch the host clipboard: {e}"),
        }
    }
}

struct SessionIface(Arc<State>);

#[zbus::interface(name = "fr.ameleo.DisplayAgent.Session1")]
impl SessionIface {
    fn clipboard_grab(&self, serial: u32, mimes: Vec<String>) {
        tracing::info!(serial, ?mimes, "host clipboard grab");
        *self.0.host.lock().unwrap() = Some((serial, mimes.clone()));
        match &self.0.clip {
            Clip::Wayland(w) => {
                let offer = clipboard::offer_for(&mimes, self.0.host_files.get().is_some());
                w.offer((!offer.is_empty()).then_some(offer));
            }
            Clip::Gnome(g) => {
                let (state, g) = (self.0.clone(), g.clone());
                tokio::spawn(async move { state.set_gnome(&g, serial, &mimes).await });
            }
            Clip::None => {}
        }
    }

    async fn clipboard_release(&self) {
        if self.0.host.lock().unwrap().take().is_none() {
            return;
        }
        match &self.0.clip {
            Clip::Wayland(w) => w.offer(None),
            Clip::Gnome(g) => g.clear().await,
            Clip::None => {}
        }
    }

    async fn clipboard_request(&self, mimes: Vec<String>) -> proto::Result<(String, Vec<u8>)> {
        let guest = self
            .0
            .guest
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| Error::NoData("the guest clipboard is empty".into()))?;
        for wanted in &mimes {
            let Some(source) = clipboard::guest_source(wanted, &guest) else {
                continue;
            };
            if wanted == proto::mime::FILE_LIST {
                let data = self.0.guest_file_list(&source).await?;
                return Ok((wanted.clone(), data));
            }
            let data = self.0.clip.read(&source).await.map_err(Error::NoData)?;
            let data = tokio::task::spawn_blocking({
                let (source, wanted) = (source.clone(), wanted.clone());
                move || clipboard::convert(&source, &wanted, data)
            })
            .await
            .map_err(|e| Error::Failed(e.to_string()))?
            .map_err(Error::Failed)?;
            tracing::debug!(wanted, source, bytes = data.len(), "guest clipboard read");
            return Ok((wanted.clone(), data));
        }
        Err(Error::NoData(format!("none of {mimes:?} in {guest:?}")))
    }

    async fn announce(&self) {
        self.0.announce().await;
    }

    fn lock_clip_data(&self, clip_data_id: u32) -> proto::Result<()> {
        self.0.files.lock().unwrap().lock(clip_data_id)
    }

    fn unlock_clip_data(&self, clip_data_id: u32) {
        self.0.files.lock().unwrap().unlock(clip_data_id);
    }

    async fn file_contents(
        &self,
        lindex: i32,
        flags: u32,
        position: u64,
        requested: u32,
        clip_data_id: u32,
        has_clip_data_id: bool,
    ) -> proto::Result<Vec<u8>> {
        let entries = self
            .0
            .files
            .lock()
            .unwrap()
            .entries(has_clip_data_id.then_some(clip_data_id))?;
        let state = self.0.clone();
        tokio::task::spawn_blocking(move || {
            filelist::contents(
                &entries,
                &state.open_file,
                lindex,
                flags,
                position,
                requested,
            )
        })
        .await
        .map_err(|e| Error::Failed(e.to_string()))?
    }

    fn set_effects(&self, no_wallpaper: bool, no_font_smoothing: bool, no_animation: bool) {
        tokio::spawn(effects::apply((
            no_wallpaper,
            no_font_smoothing,
            no_animation,
        )));
    }

    fn file_start(&self, name: &str, size: u64) -> proto::Result<u32> {
        self.0.xfer.lock().unwrap().start(name, size)
    }

    fn file_write(&self, id: u32, data: Vec<u8>) -> proto::Result<()> {
        self.0.xfer.lock().unwrap().write(id, &data)
    }

    fn file_finish(&self, id: u32) -> proto::Result<String> {
        let (path, name) = self.0.xfer.lock().unwrap().finish(id)?;
        tracing::info!("file received: {}", path.display());
        tokio::spawn(show_file(path));
        Ok(name)
    }

    fn file_cancel(&self, id: u32) -> proto::Result<()> {
        self.0.xfer.lock().unwrap().cancel(id)
    }

    async fn reset(&self) {
        tracing::info!("host session reset");
        self.0.xfer.lock().unwrap().reset();
        self.0.files.lock().unwrap().reset();
        *self.0.open_file.lock().unwrap() = None;
        *self.0.pasted.lock().await = None;
        if let Some(files) = self.0.host_files.get() {
            files.clear();
        }
        if let Some(cameras) = self.0.cameras.get() {
            cameras.remove_all();
        }
        self.clipboard_release().await;
    }
}

struct CameraIface(Arc<State>);

/// `spawn = false`: calls are handled in arrival order, so frames never
/// overtake each other. Only `CameraAdd` waits, briefly, for PipeWire.
#[zbus::interface(name = "fr.ameleo.DisplayAgent.Camera1", spawn = false)]
impl CameraIface {
    async fn camera_add(
        &self,
        id: u32,
        name: &str,
        formats: Vec<(u32, u32, u32, u32)>,
    ) -> proto::Result<()> {
        let cameras = self
            .0
            .cameras
            .get()
            .ok_or_else(|| Error::Failed("no PipeWire in this session".into()))?;
        cameras
            .add(id, name.to_string(), formats)
            .await
            .map_err(Error::Failed)
    }

    fn camera_remove(&self, id: u32) {
        if let Some(cameras) = self.0.cameras.get() {
            cameras.remove(id);
        }
    }

    fn camera_remove_all(&self) {
        if let Some(cameras) = self.0.cameras.get() {
            cameras.remove_all();
        }
    }

    fn camera_frame(&self, id: u32, data: Vec<u8>) {
        if let Some(cameras) = self.0.cameras.get() {
            cameras.push(id, data);
        }
    }
}

#[zbus::proxy(
    interface = "org.freedesktop.FileManager1",
    default_service = "org.freedesktop.FileManager1",
    default_path = "/org/freedesktop/FileManager1",
    gen_blocking = false
)]
trait FileManager {
    fn show_items(&self, uris: Vec<String>, startup_id: &str) -> zbus::Result<()>;
}

/// Shows the received file in the file manager, selected.
async fn show_file(path: std::path::PathBuf) {
    let uri = filelist::file_uri(&path);
    let res = async {
        let conn = zbus::Connection::session().await?;
        FileManagerProxy::new(&conn)
            .await?
            .show_items(vec![uri], "")
            .await
    }
    .await;
    if let Err(e) = res {
        tracing::warn!("cannot show {} in the file manager: {e}", path.display());
    }
}

/// Writes host data to a guest app pasting it.
async fn paste(state: Arc<State>, mime: String, fd: std::os::fd::OwnedFd) {
    let data = match state.host_data(&mime).await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(mime, "guest paste of host data failed: {e}");
            return; // closing the pipe: the app gets nothing
        }
    };
    let res = tokio::task::spawn_blocking(move || std::fs::File::from(fd).write_all(&data)).await;
    if let Ok(Err(e)) = res {
        tracing::debug!(mime, "the pasting app closed the pipe: {e}");
    }
}

async fn connect_daemon() -> tokio::net::UnixStream {
    let path = crate::ipc::socket();
    let mut warned = false;
    loop {
        match tokio::net::UnixStream::connect(&path).await {
            Ok(s) => return s,
            Err(e) => {
                if !warned {
                    tracing::info!("waiting for the daemon on {}: {e}", path.display());
                    warned = true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

/// Mounts the file system for host files; without it, host files cannot
/// be pasted, the rest works.
fn mount_host_files(state: &State, daemon: DaemonProxy<'static>) {
    let Some(runtime) = dirs::runtime_dir() else {
        tracing::warn!("no XDG_RUNTIME_DIR: host files cannot be pasted");
        return;
    };
    let fetch_daemon = daemon.clone();
    let fetch: hostfiles::Fetch = Arc::new(move |clip_data_id, lindex, position, len| {
        let daemon = fetch_daemon.clone();
        Box::pin(async move {
            daemon
                .host_file_contents(
                    lindex,
                    proto::file_contents::RANGE,
                    position,
                    len,
                    clip_data_id,
                    true,
                )
                .await
                .map_err(|e| e.to_string())
        })
    });
    let unlock: hostfiles::Unlock = Arc::new(move |clip_data_id| {
        let daemon = daemon.clone();
        tokio::spawn(async move {
            let _ = daemon.host_unlock_clip_data(clip_data_id).await;
        });
    });
    let mount = runtime.join("ameleo-display-agent").join("clipboard");
    match hostfiles::HostFiles::mount(mount, fetch, unlock) {
        Ok(files) => {
            let _ = state.host_files.set(files);
        }
        Err(e) => tracing::warn!("cannot mount the host files ({e}): they cannot be pasted"),
    }
}

fn is_gnome() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .any(|d| d.eq_ignore_ascii_case("GNOME"))
}

pub async fn run() -> Result<()> {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        // X11 sessions are not supported: stop for good, systemd does not
        // restart a clean exit.
        tracing::error!("not a Wayland session (no WAYLAND_DISPLAY): the session agent stops");
        return Ok(());
    }
    let (events_tx, mut events) = mpsc::unbounded_channel();
    let wl = wayland::spawn(events_tx.clone())?;
    let clip = if wl.clipboard {
        Clip::Wayland(wl)
    } else if is_gnome() {
        Clip::Gnome(gnome::connect(events_tx.clone()).await?)
    } else {
        tracing::warn!("no clipboard: the compositor has no data control protocol");
        Clip::None
    };
    let state = Arc::new(State::new(clip, None));
    let (camera_tx, mut camera_events) = mpsc::unbounded_channel();
    match camera::Cameras::spawn(camera_tx) {
        Ok(c) => {
            let _ = state.cameras.set(c);
        }
        Err(e) => tracing::warn!("no client cameras: {e:#}"),
    }

    let stream = connect_daemon().await;
    let conn = zbus::connection::Builder::unix_stream(stream)
        .p2p()
        .serve_at(SESSION_PATH, SessionIface(state.clone()))?
        .serve_at(SESSION_PATH, CameraIface(state.clone()))?
        .build()
        .await?;
    let daemon = DaemonProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let _ = state.daemon.set(daemon.clone());
    tracing::info!("connected to the daemon");
    mount_host_files(&state, daemon);

    let mut streams = CameraStreams::default();
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            Some(ev) = events.recv() => match ev {
                Event::Layout(heads) => {
                    tracing::info!(?heads, "monitor layout");
                    if let Err(e) = state.daemon().set_layout(heads).await {
                        tracing::warn!("cannot send the layout: {e}");
                    }
                }
                Event::Selection(mimes) => {
                    tracing::debug!(?mimes, "guest selection");
                    *state.guest.lock().unwrap() = mimes;
                    let state = state.clone();
                    tokio::spawn(async move { state.announce().await });
                }
                Event::Send { mime, fd } => {
                    tokio::spawn(paste(state.clone(), mime, fd));
                }
            },
            Some(ev) = camera_events.recv() => match ev {
                camera::Event::Stream { id, format } => {
                    streams.change(id, format, &state);
                }
                camera::Event::KeyFrame { id } => {
                    if let Err(e) = state.daemon().camera_key_frame(id).await {
                        tracing::warn!("cannot send a camera event: {e}");
                    }
                }
            },
            _ = conn.closed() => anyhow::bail!("the daemon closed the connection"),
            _ = sigterm.recv() => {
                tracing::info!("stopping");
                return Ok(());
            }
        }
    }
}

/// How long a camera stays started after its app paused it: apps pause and
/// start again while they settle on a format, the host need not know.
const STREAM_STOP_DELAY: Duration = Duration::from_millis(500);

/// The camera stream states told to the host, with a delay on stops.
#[derive(Default)]
struct CameraStreams {
    /// The format the host was told, per camera.
    told: Arc<Mutex<std::collections::HashMap<u32, u32>>>,
    /// Stops not told yet.
    pending: std::collections::HashMap<u32, tokio::task::JoinHandle<()>>,
}

impl CameraStreams {
    fn change(&mut self, id: u32, format: Option<u32>, state: &Arc<State>) {
        if let Some(stop) = self.pending.remove(&id) {
            stop.abort();
        }
        let told = self.told.clone();
        let state = state.clone();
        match format {
            Some(f) => {
                if told.lock().unwrap().insert(id, f) == Some(f) {
                    return; // started again in the same format
                }
                tokio::spawn(async move {
                    tracing::info!(id, format = f, "camera stream started");
                    if let Err(e) = state.daemon().camera_stream(id, f as i32).await {
                        tracing::warn!("cannot send a camera event: {e}");
                    }
                });
            }
            None => {
                let stop = tokio::spawn(async move {
                    tokio::time::sleep(STREAM_STOP_DELAY).await;
                    if told.lock().unwrap().remove(&id).is_none() {
                        return;
                    }
                    tracing::info!(id, "camera stream stopped");
                    if let Err(e) = state.daemon().camera_stream(id, -1).await {
                        tracing::warn!("cannot send a camera event: {e}");
                    }
                });
                self.pending.insert(id, stop);
            }
        }
    }
}

/// A session agent without a desktop on `stream`, for the daemon tests:
/// its guest clipboard holds `guest` types it cannot read.
#[cfg(test)]
pub async fn test_agent(
    stream: tokio::net::UnixStream,
    guest: Option<Vec<String>>,
) -> zbus::Connection {
    let state = Arc::new(State::new(Clip::None, guest));
    let conn = zbus::connection::Builder::unix_stream(stream)
        .p2p()
        .serve_at(SESSION_PATH, SessionIface(state.clone()))
        .unwrap()
        .build()
        .await
        .unwrap();
    let daemon = DaemonProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .unwrap();
    let _ = state.daemon.set(daemon);
    conn
}

#[cfg(test)]
mod tests {
    #[test]
    fn uris() {
        assert_eq!(
            super::filelist::file_uri(std::path::Path::new("/home/a/Téléchargements/x y.pdf")),
            "file:///home/a/T%C3%A9l%C3%A9chargements/x%20y.pdf"
        );
    }
}
