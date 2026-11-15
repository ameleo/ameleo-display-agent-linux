// SPDX-License-Identifier: GPL-3.0-only

//! The session agents: one per graphical session, connected to
//! [`crate::ipc::SOCKET`]. The daemon serves `fr.ameleo.DisplayAgent.Daemon1`
//! to each of them and calls their `fr.ameleo.DisplayAgent.Session1`.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use proto::{Error, HostClipboardProxy, HostDriveProxy, HostFileClipboardProxy, Result};
use tokio::net::UnixListener;
use zbus::Connection;
use zbus::proxy::CacheProperties;

use super::{CALL_TIMEOUT, CameraSignal, Effects, Shared};
use crate::ipc::{DAEMON_PATH, Head, SessionCameraProxy, SessionDriveProxy, SessionProxy};

pub struct Agent {
    pub id: u64,
    pub uid: u32,
    conn: OnceLock<Connection>,
    /// Its cameras, built once: a frame is sent per proxy call.
    camera: OnceLock<SessionCameraProxy<'static>>,
    drive: OnceLock<SessionDriveProxy<'static>>,
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

    pub fn camera(&self) -> &SessionCameraProxy<'static> {
        self.camera
            .get()
            .expect("agent registered after its connection")
    }

    pub fn drive(&self) -> &SessionDriveProxy<'static> {
        self.drive
            .get()
            .expect("agent registered after its connection")
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
        camera: OnceLock::new(),
        drive: OnceLock::new(),
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
        .serve_at(
            DAEMON_PATH,
            HostDriveIface {
                shared: shared.clone(),
                agent: agent.clone(),
            },
        )?
        .build()
        .await?;
    let _ = agent.conn.set(conn.clone());
    let camera = SessionCameraProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let _ = agent.camera.set(camera);
    let drive = SessionDriveProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    let _ = agent.drive.set(drive);
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
pub(super) async fn host_call<T>(
    call: impl std::future::Future<Output = zbus::Result<T>>,
) -> Result<T> {
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

    async fn camera_stream(&self, id: u32, format: i32) {
        if !self.is_current() {
            return;
        }
        let format = u32::try_from(format).ok();
        if let Some(cam) = self.shared.cameras.lock().unwrap().get_mut(&id) {
            cam.streaming = format.is_some();
        }
        self.shared
            .emit_camera(id, CameraSignal::Stream(format))
            .await;
    }

    async fn camera_control(&self, id: u32, set: u8, property: u8, mode: u8, value: i32) {
        if self.is_current() {
            self.shared
                .emit_camera(id, CameraSignal::Control(set, property, mode, value))
                .await;
        }
    }

    async fn camera_key_frame(&self, id: u32) {
        if self.is_current() {
            self.shared.emit_camera(id, CameraSignal::KeyFrame).await;
        }
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

/// `Host.Drive1` for the active agent: its FUSE file systems' requests,
/// passed on as they are. Methods run concurrently, as the host takes them.
pub struct HostDriveIface {
    shared: Arc<Shared>,
    agent: Arc<Agent>,
}

impl HostDriveIface {
    /// The host's `Drive1`, when this agent may use drive `id`.
    async fn host(&self, id: u32) -> Result<HostDriveProxy<'static>> {
        if !self.shared.current().is_some_and(|a| a.id == self.agent.id) {
            return Err(Error::Failed("not the active session".into()));
        }
        if !self.shared.drives.lock().unwrap().contains_key(&id) {
            return Err(Error::NotFound(format!("no client drive {id}")));
        }
        let conn = self
            .shared
            .host()
            .ok_or_else(|| Error::Failed("no host session".into()))?;
        Ok(HostDriveProxy::builder(&conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await?)
    }
}

#[zbus::interface(name = "fr.ameleo.DisplayAgent.HostDrive1")]
impl HostDriveIface {
    #[allow(clippy::too_many_arguments)]
    async fn open(
        &self,
        drive_id: u32,
        path: &str,
        desired_access: u32,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
    ) -> Result<(u32, u32, u32)> {
        let p = self.host(drive_id).await?;
        host_call(p.open(
            drive_id,
            path,
            desired_access,
            file_attributes,
            share_access,
            create_disposition,
            create_options,
        ))
        .await
    }

    async fn close(&self, drive_id: u32, file_id: u32) -> Result<u32> {
        let p = self.host(drive_id).await?;
        host_call(p.close(drive_id, file_id)).await
    }

    async fn read(
        &self,
        drive_id: u32,
        file_id: u32,
        offset: u64,
        length: u32,
    ) -> Result<(u32, Vec<u8>)> {
        let p = self.host(drive_id).await?;
        host_call(p.read(drive_id, file_id, offset, length.min(proto::drive::IO_MAX))).await
    }

    async fn write(
        &self,
        drive_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<(u32, u32)> {
        if data.len() > proto::drive::IO_MAX as usize {
            return Err(Error::InvalidArgument("write larger than IO_MAX".into()));
        }
        let p = self.host(drive_id).await?;
        host_call(p.write(drive_id, file_id, offset, data)).await
    }

    async fn flush(&self, drive_id: u32, file_id: u32) -> Result<u32> {
        let p = self.host(drive_id).await?;
        host_call(p.flush(drive_id, file_id)).await
    }

    async fn query(&self, drive_id: u32, file_id: u32) -> Result<(u32, proto::drive::Entry)> {
        let p = self.host(drive_id).await?;
        host_call(p.query(drive_id, file_id)).await
    }

    async fn set_basic(
        &self,
        drive_id: u32,
        file_id: u32,
        attributes: u32,
        creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
    ) -> Result<u32> {
        let p = self.host(drive_id).await?;
        host_call(p.set_basic(
            drive_id,
            file_id,
            attributes,
            creation_time,
            last_access_time,
            last_write_time,
        ))
        .await
    }

    async fn set_size(
        &self,
        drive_id: u32,
        file_id: u32,
        size: u64,
        allocation: bool,
    ) -> Result<u32> {
        let p = self.host(drive_id).await?;
        host_call(p.set_size(drive_id, file_id, size, allocation)).await
    }

    async fn set_delete(&self, drive_id: u32, file_id: u32, delete: bool) -> Result<u32> {
        let p = self.host(drive_id).await?;
        host_call(p.set_delete(drive_id, file_id, delete)).await
    }

    async fn rename(
        &self,
        drive_id: u32,
        file_id: u32,
        new_path: &str,
        replace: bool,
    ) -> Result<u32> {
        let p = self.host(drive_id).await?;
        host_call(p.rename(drive_id, file_id, new_path, replace)).await
    }

    async fn list(&self, drive_id: u32, path: &str) -> Result<(u32, Vec<proto::drive::Entry>)> {
        let p = self.host(drive_id).await?;
        host_call(p.list(drive_id, path)).await
    }

    async fn volume(&self, drive_id: u32) -> Result<(u32, u64, u64, String, String)> {
        let p = self.host(drive_id).await?;
        host_call(p.volume(drive_id)).await
    }
}
