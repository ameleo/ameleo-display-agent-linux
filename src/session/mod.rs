// SPDX-License-Identifier: GPL-3.0-or-later

//! The session agent: runs as the user of a graphical session, connects to
//! the daemon ([`crate::ipc::SOCKET`]) and does what needs the user's
//! desktop: the clipboard, the monitor layout, files sent by the host,
//! desktop effects.

mod clipboard;
mod effects;
mod filexfer;
mod gnome;
mod wayland;

use std::io::Write;
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
}

impl State {
    fn daemon(&self) -> &DaemonProxy<'static> {
        self.daemon.get().expect("daemon proxy set at connection")
    }

    /// Tells the host what the guest clipboard holds now.
    async fn announce(&self) {
        let guest = self.guest.lock().unwrap().clone();
        let exported = guest.as_deref().map(clipboard::exportable).unwrap_or_default();
        let res = if exported.is_empty() {
            self.daemon().clipboard_released().await
        } else {
            let serial = self.serial.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            tracing::info!(serial, ?exported, "guest clipboard grabbed");
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
        let (mime, data) = self
            .daemon()
            .host_clipboard_request(serial, types)
            .await
            .map_err(|e| e.to_string())?;
        clipboard::convert(&mime, guest_mime, data)
    }

    /// GNOME: the host copied, put its data on the clipboard now.
    async fn set_gnome(&self, gnome: &gnome::Handle, serial: u32, host: &[String]) {
        let mime = if host.iter().any(|m| clipboard::is_text(m)) {
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
                let offer = clipboard::offer_for(&mimes);
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

    fn set_effects(&self, no_wallpaper: bool, no_font_smoothing: bool, no_animation: bool) {
        tokio::spawn(effects::apply((no_wallpaper, no_font_smoothing, no_animation)));
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
        self.clipboard_release().await;
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
    let uri = file_uri(&path);
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

fn file_uri(path: &std::path::Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
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
    let state = Arc::new(State {
        daemon: OnceLock::new(),
        clip,
        host: Mutex::new(None),
        guest: Mutex::new(None),
        serial: AtomicU32::new(0),
        xfer: Mutex::new(filexfer::FileXfer::default()),
    });

    let stream = connect_daemon().await;
    let conn = zbus::connection::Builder::unix_stream(stream)
        .p2p()
        .serve_at(SESSION_PATH, SessionIface(state.clone()))?
        .build()
        .await?;
    let daemon = DaemonProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let _ = state.daemon.set(daemon);
    tracing::info!("connected to the daemon");

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
            _ = conn.closed() => anyhow::bail!("the daemon closed the connection"),
            _ = sigterm.recv() => {
                tracing::info!("stopping");
                return Ok(());
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
    let state = Arc::new(State {
        daemon: OnceLock::new(),
        clip: Clip::None,
        host: Mutex::new(None),
        guest: Mutex::new(guest),
        serial: AtomicU32::new(0),
        xfer: Mutex::new(filexfer::FileXfer::default()),
    });
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
            super::file_uri(std::path::Path::new("/home/a/Téléchargements/x y.pdf")),
            "file:///home/a/T%C3%A9l%C3%A9chargements/x%20y.pdf"
        );
    }
}
