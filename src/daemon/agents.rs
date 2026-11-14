// SPDX-License-Identifier: GPL-3.0-or-later

//! The session agents: one per graphical session, connected to
//! [`crate::ipc::SOCKET`]. The daemon serves `fr.ameleo.DisplayAgent.Daemon1`
//! to each of them and calls their `fr.ameleo.DisplayAgent.Session1`.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use proto::{Error, HostClipboardProxy, HostFileClipboardProxy, Result};
use tokio::net::UnixListener;
use zbus::Connection;
use zbus::proxy::CacheProperties;

use super::{CALL_TIMEOUT, Effects, Shared};
use crate::ipc::{DAEMON_PATH, Head, SessionProxy};

pub struct Agent {
    pub id: u64,
    pub uid: u32,
    conn: OnceLock<Connection>,
    /// The monitors of its session.
    pub layout: Mutex<Vec<Head>>,
}

impl Agent {
    pub async fn session(&self) -> SessionProxy<'static> {
        let conn = self
            .conn
            .get()
            .expect("agent registered after its connection");
        SessionProxy::builder(conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .expect("static proxy parameters")
    }

    pub fn send_effects(self: &Arc<Self>, e: Effects) {
        let agent = self.clone();
        tokio::spawn(async move {
            let res = agent
                .session()
                .await
                .set_effects(e.no_wallpaper, e.no_font_smoothing, e.no_animation)
                .await;
            if let Err(err) = res {
                tracing::warn!(agent = agent.id, "cannot send the desktop effects: {err}");
            }
        });
    }
}

/// Accepts session agents on `path`.
pub fn listen(shared: Arc<Shared>, path: &std::path::Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // Any user may connect: only the agent of the user in front of the
    // screen gets anything, and a user only speaks for their own uid.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    tracing::info!("waiting for session agents on {}", path.display());
    tokio::spawn(async move {
        let next = AtomicU64::new(1);
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let id = next.fetch_add(1, Ordering::Relaxed);
                    let shared = shared.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve(shared, id, stream).await {
                            tracing::warn!(agent = id, "session agent refused: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("session agent socket: {e}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });
    Ok(())
}

pub(super) async fn serve(
    shared: Arc<Shared>,
    id: u64,
    stream: tokio::net::UnixStream,
) -> anyhow::Result<()> {
    let uid = stream.peer_cred()?.uid();
    let agent = Arc::new(Agent {
        id,
        uid,
        conn: OnceLock::new(),
        layout: Mutex::new(Vec::new()),
    });
    let conn = zbus::connection::Builder::unix_stream(stream)
        .server(zbus::Guid::generate())?
        .p2p()
        .serve_at(
            DAEMON_PATH,
            DaemonIface {
                shared: shared.clone(),
                agent: agent.clone(),
            },
        )?
        .build()
        .await?;
    let _ = agent.conn.set(conn.clone());
    tracing::info!(agent = id, uid, "session agent connected");

    agent.send_effects(*shared.effects.lock().unwrap());
    shared.agents.lock().unwrap().push(agent.clone());
    shared.refresh_current();

    conn.closed().await;
    tracing::info!(agent = id, uid, "session agent gone");
    shared.agents.lock().unwrap().retain(|a| a.id != id);
    shared
        .transfers
        .lock()
        .unwrap()
        .retain(|_, t| t.agent.id != id);
    shared.refresh_current();
    Ok(())
}

pub struct DaemonIface {
    shared: Arc<Shared>,
    agent: Arc<Agent>,
}

impl DaemonIface {
    fn is_current(&self) -> bool {
        self.shared.current().is_some_and(|a| a.id == self.agent.id)
    }

    /// `Host.FileClipboard1`, for the active agent only.
    async fn host_files(&self) -> Result<HostFileClipboardProxy<'static>> {
        if !self.is_current() {
            return Err(Error::NoData("not the active session".into()));
        }
        let conn = self
            .shared
            .host()
            .ok_or_else(|| Error::NoData("no host session".into()))?;
        Ok(HostFileClipboardProxy::builder(&conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await?)
    }
}

/// A call to the host, with a timeout; its errors as `Failed`.
async fn host_call<T>(call: impl std::future::Future<Output = zbus::Result<T>>) -> Result<T> {
    tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| Error::Failed(format!("no answer within {CALL_TIMEOUT:?}")))?
        .map_err(|e| Error::Failed(e.to_string()))
}

#[zbus::interface(name = "fr.ameleo.DisplayAgent.Daemon1")]
impl DaemonIface {
    fn set_layout(&self, heads: Vec<Head>) {
        tracing::info!(agent = self.agent.id, ?heads, "session layout");
        *self.agent.layout.lock().unwrap() = heads.clone();
        if self.is_current() {
            self.shared.mouse.send(super::mouse::Cmd::Layout(heads));
        }
    }

    async fn clipboard_grabbed(&self, serial: u32, mimes: Vec<String>) {
        if self.is_current() {
            self.shared.emit_clipboard(Some((serial, mimes))).await;
        }
    }

    async fn clipboard_released(&self) {
        if self.is_current() {
            self.shared.emit_clipboard(None).await;
        }
    }

    async fn host_lock_clip_data(&self, clip_data_id: u32) -> Result<()> {
        let p = self.host_files().await?;
        host_call(p.lock_clip_data(clip_data_id)).await
    }

    async fn host_unlock_clip_data(&self, clip_data_id: u32) -> Result<()> {
        let p = self.host_files().await?;
        host_call(p.unlock_clip_data(clip_data_id)).await
    }

    #[allow(clippy::too_many_arguments)]
    async fn host_file_contents(
        &self,
        lindex: i32,
        flags: u32,
        position: u64,
        requested: u32,
        clip_data_id: u32,
        has_clip_data_id: bool,
    ) -> Result<Vec<u8>> {
        let p = self.host_files().await?;
        host_call(p.file_contents(
            lindex,
            flags,
            position,
            requested,
            clip_data_id,
            has_clip_data_id,
        ))
        .await
    }

    async fn host_clipboard_request(
        &self,
        serial: u32,
        mimes: Vec<String>,
    ) -> Result<(String, Vec<u8>)> {
        if !self.is_current() {
            return Err(Error::NoData("not the active session".into()));
        }
        let conn = self
            .shared
            .host()
            .ok_or_else(|| Error::NoData("no host session".into()))?;
        let proxy = HostClipboardProxy::builder(&conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let t = std::time::Instant::now();
        let res = tokio::time::timeout(CALL_TIMEOUT, proxy.request(serial, mimes.clone()))
            .await
            .map_err(|_| Error::Failed(format!("no answer within {CALL_TIMEOUT:?}")))?;
        match &res {
            Ok((m, d)) => {
                tracing::debug!(took = ?t.elapsed(), mime = m, bytes = d.len(), "host clipboard data")
            }
            Err(e) => {
                tracing::warn!(took = ?t.elapsed(), ?mimes, "host clipboard request failed: {e}")
            }
        }
        res.map_err(|e| Error::NoData(e.to_string()))
    }
}
