// SPDX-License-Identifier: GPL-3.0-only

//! D-Bus interfaces the daemon serves to the host (see the `proto` crate for
//! the contract). The pointer is handled here; the clipboard, the files and
//! the effects go to the session agent of the active user.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use proto::{Error, Result, iface};
use zbus::object_server::SignalEmitter;

use super::agents::Agent;
use super::mouse::Cmd;
use super::{CALL_TIMEOUT, ClipCmd, Effects, Shared};
use crate::ipc::to_proto;

/// Runs `call` on the active agent, with a timeout.
async fn on_agent<T, F>(
    agent: &Arc<Agent>,
    call: impl FnOnce(crate::ipc::SessionProxy<'static>) -> F,
) -> Result<T>
where
    F: std::future::Future<Output = zbus::Result<T>>,
{
    let proxy = agent.session().await;
    match tokio::time::timeout(CALL_TIMEOUT, call(proxy)).await {
        Ok(res) => res.map_err(to_proto),
        Err(_) => Err(Error::Failed("the session agent did not answer".into())),
    }
}

fn no_agent() -> Error {
    Error::Failed("no session agent for the user in front of the screen".into())
}

pub struct AgentIface(pub Arc<Shared>);

#[zbus::interface(name = "fr.ameleo.Agent1")]
impl AgentIface {
    #[zbus(property)]
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }

    #[zbus(property)]
    fn channels(&self) -> Vec<String> {
        [
            iface::MOUSE,
            iface::CLIPBOARD,
            iface::FILE_CLIPBOARD,
            iface::DISPLAY,
            iface::CAMERA,
            iface::DRIVE,
            iface::FILE_TRANSFER,
        ]
        .into_iter()
        .map(String::from)
        .collect()
    }

    #[zbus(property)]
    fn session_locked(&self) -> bool {
        self.0.locked.load(Ordering::Relaxed)
    }

    fn reset(&self) {
        tracing::info!("remote client reset");
        self.0.reset_client();
    }
}

pub struct MouseIface(pub Arc<Shared>);

/// `spawn = false`: calls are handled one by one in arrival order, so a
/// release never overtakes its press. The methods only queue an event.
#[zbus::interface(name = "fr.ameleo.Agent.Mouse1", spawn = false)]
impl MouseIface {
    fn set_abs_position(&self, head: u32, x: u32, y: u32) {
        tracing::trace!(head, x, y, "mouse move received");
        self.0.mouse.send(Cmd::Move { head, x, y });
    }

    fn press(&self, button: u32) {
        tracing::trace!(button, "mouse press received");
        self.0.mouse.send(Cmd::Button { button, down: true });
    }

    fn release(&self, button: u32) {
        tracing::trace!(button, "mouse release received");
        self.0.mouse.send(Cmd::Button {
            button,
            down: false,
        });
    }

    fn scroll(&self, dx: i32, dy: i32) {
        self.0.mouse.send(Cmd::Scroll { dx, dy });
    }
}

pub struct ClipboardIface(pub Arc<Shared>);

#[zbus::interface(name = "fr.ameleo.Agent.Clipboard1")]
impl ClipboardIface {
    fn grab(&self, serial: u32, mimes: Vec<String>) {
        tracing::info!(serial, ?mimes, "host clipboard grab");
        *self.0.host_grab.lock().unwrap() = Some((serial, mimes.clone()));
        let _ = self.0.clip.send(ClipCmd::Grab { serial, mimes });
    }

    fn release(&self) {
        tracing::info!("host clipboard release");
        *self.0.host_grab.lock().unwrap() = None;
        let _ = self.0.clip.send(ClipCmd::Release);
    }

    async fn request(&self, mimes: Vec<String>) -> Result<(String, Vec<u8>)> {
        if self.0.locked.load(Ordering::Relaxed) {
            return Err(Error::SessionLocked("the session is locked".into()));
        }
        let agent = self
            .0
            .current()
            .ok_or_else(|| Error::NoData("no session agent".into()))?;
        let (mime, data) =
            on_agent(&agent, |p| async move { p.clipboard_request(mimes).await }).await?;
        let max = self.0.max_clipboard.load(Ordering::Relaxed);
        if max >= 0 && data.len() > max as usize {
            return Err(Error::InvalidArgument(format!(
                "{} bytes exceed the {max} byte limit",
                data.len()
            )));
        }
        Ok((mime, data))
    }

    #[zbus(property)]
    fn max_size(&self) -> i32 {
        self.0.max_clipboard.load(Ordering::Relaxed)
    }

    #[zbus(property)]
    fn set_max_size(&self, value: i32) {
        self.0.max_clipboard.store(value, Ordering::Relaxed);
    }

    #[zbus(signal)]
    pub async fn grabbed(
        emitter: &SignalEmitter<'_>,
        serial: u32,
        mimes: Vec<String>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn released(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

pub struct FileClipboardIface(pub Arc<Shared>);

/// Files copied in the guest: served by the active session agent.
#[zbus::interface(name = "fr.ameleo.Agent.FileClipboard1")]
impl FileClipboardIface {
    async fn lock_clip_data(&self, clip_data_id: u32) -> Result<()> {
        tracing::debug!(clip_data_id, "lock clip data");
        let agent = self.0.current().ok_or_else(no_agent)?;
        on_agent(
            &agent,
            |p| async move { p.lock_clip_data(clip_data_id).await },
        )
        .await
    }

    async fn unlock_clip_data(&self, clip_data_id: u32) -> Result<()> {
        tracing::debug!(clip_data_id, "unlock clip data");
        let agent = self.0.current().ok_or_else(no_agent)?;
        on_agent(
            &agent,
            |p| async move { p.unlock_clip_data(clip_data_id).await },
        )
        .await
    }

    async fn file_contents(
        &self,
        lindex: i32,
        flags: u32,
        position: u64,
        requested: u32,
        clip_data_id: u32,
        has_clip_data_id: bool,
    ) -> Result<Vec<u8>> {
        if self.0.locked.load(Ordering::Relaxed) {
            return Err(Error::SessionLocked("the session is locked".into()));
        }
        let agent = self.0.current().ok_or_else(no_agent)?;
        let t = std::time::Instant::now();
        let res = on_agent(&agent, |p| async move {
            p.file_contents(
                lindex,
                flags,
                position,
                requested,
                clip_data_id,
                has_clip_data_id,
            )
            .await
        })
        .await;
        match &res {
            Ok(d) => {
                tracing::debug!(lindex, flags, position, requested, bytes = d.len(), took = ?t.elapsed(), "guest file contents served")
            }
            Err(e) => tracing::warn!(
                lindex,
                flags,
                position,
                requested,
                "guest file contents failed: {e}"
            ),
        }
        res
    }
}

pub struct CameraIface(pub Arc<Shared>);

/// Longest wait for a session agent to create a camera: the host's calls
/// wait behind it (`spawn = false`).
const CAMERA_ADD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Formats per camera.
const MAX_FORMATS: usize = 16;

/// Checks the formats of `AddCamera`.
pub fn check_formats(formats: &[(u32, u32, u32, u32)]) -> Result<()> {
    use proto::camera::{MAX_HEIGHT, MAX_WIDTH};
    if formats.is_empty() || formats.len() > MAX_FORMATS {
        return Err(Error::InvalidArgument(format!(
            "1 to {MAX_FORMATS} formats"
        )));
    }
    let bad = |&(w, h, num, den): &(u32, u32, u32, u32)| {
        w == 0
            || h == 0
            || w > MAX_WIDTH
            || h > MAX_HEIGHT
            || w % 2 != 0
            || h % 2 != 0
            || num == 0
            || den == 0
    };
    if let Some(f) = formats.iter().find(|f| bad(f)) {
        return Err(Error::InvalidArgument(format!("unsupported format {f:?}")));
    }
    Ok(())
}

/// Client cameras: shown in the active session, through PipeWire.
/// `spawn = false`: frames are passed on in the order they came.
#[zbus::interface(name = "fr.ameleo.Agent.Camera1", spawn = false)]
impl CameraIface {
    async fn add_camera(
        &self,
        id: u32,
        name: &str,
        formats: Vec<(u32, u32, u32, u32)>,
    ) -> Result<()> {
        tracing::info!(id, name, ?formats, "AddCamera");
        check_formats(&formats)?;
        {
            let mut cams = self.0.cameras.lock().unwrap();
            if !cams.contains_key(&id) && cams.len() >= proto::camera::MAX_CAMERAS as usize {
                return Err(Error::Failed(format!(
                    "at most {} cameras",
                    proto::camera::MAX_CAMERAS
                )));
            }
            cams.insert(
                id,
                super::CameraInfo {
                    name: name.to_string(),
                    formats: formats.clone(),
                    streaming: false,
                    controls: Vec::new(),
                },
            );
        }
        // Without a user session, the camera comes with the next one.
        let Some(agent) = self.0.current() else {
            return Ok(());
        };
        let res = tokio::time::timeout(
            CAMERA_ADD_TIMEOUT,
            agent.camera().camera_add(id, name, formats),
        )
        .await
        .map_err(|_| Error::Failed("the session agent did not answer".into()))
        .and_then(|r| r.map_err(to_proto));
        if res.is_err() {
            self.0.cameras.lock().unwrap().remove(&id);
        }
        res
    }

    async fn remove_camera(&self, id: u32) {
        tracing::info!(id, "RemoveCamera");
        let was = self.0.cameras.lock().unwrap().remove(&id);
        if let Some(agent) = self.0.current() {
            let _ = agent.camera().camera_remove(id).await;
        } else if was.is_some_and(|c| c.streaming) {
            self.0
                .emit_camera(id, super::CameraSignal::Stream(None))
                .await;
        }
    }

    async fn push_frame(&self, id: u32, _timestamp_us: u64, data: Vec<u8>) {
        if let Some(agent) = self.0.current() {
            let _ = agent.camera().camera_frame(id, data).await;
        }
    }

    /// Kept for the next active session, and shown in this one.
    async fn set_controls(
        &self,
        id: u32,
        controls: Vec<proto::camera_control::Control>,
    ) -> Result<()> {
        if controls.len() > proto::camera_control::MAX_CONTROLS {
            return Err(Error::InvalidArgument(format!(
                "at most {} controls",
                proto::camera_control::MAX_CONTROLS
            )));
        }
        tracing::info!(id, ?controls, "SetControls");
        match self.0.cameras.lock().unwrap().get_mut(&id) {
            Some(cam) => cam.controls = controls.clone(),
            None => return Err(Error::InvalidArgument(format!("no camera {id}"))),
        }
        if let Some(agent) = self.0.current() {
            let _ = agent.camera().camera_controls(id, controls).await;
        }
        Ok(())
    }

    async fn control_value(&self, id: u32, set: u8, property: u8, mode: u8, value: i32) {
        tracing::info!(id, set, property, mode, value, "ControlValue");
        let known = match self.0.cameras.lock().unwrap().get_mut(&id) {
            Some(cam) => cam
                .controls
                .iter_mut()
                .find(|c| c.0 == set && c.1 == property)
                .map(|c| (c.7, c.8) = (mode, value))
                .is_some(),
            None => false,
        };
        if known && let Some(agent) = self.0.current() {
            let _ = agent
                .camera()
                .camera_control_value(id, set, property, mode, value)
                .await;
        }
    }

    #[zbus(signal)]
    pub async fn stream_requested(
        emitter: &SignalEmitter<'_>,
        id: u32,
        format: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn stream_stopped(emitter: &SignalEmitter<'_>, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn key_frame_needed(emitter: &SignalEmitter<'_>, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn control_requested(
        emitter: &SignalEmitter<'_>,
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    ) -> zbus::Result<()>;
}

pub struct DisplayIface(pub Arc<Shared>);

#[zbus::interface(name = "fr.ameleo.Agent.Display1")]
impl DisplayIface {
    fn set_effects(
        &self,
        disable_wallpaper: bool,
        disable_font_smoothing: bool,
        disable_animation: bool,
    ) {
        let effects = Effects {
            no_wallpaper: disable_wallpaper,
            no_font_smoothing: disable_font_smoothing,
            no_animation: disable_animation,
        };
        tracing::info!(?effects, "SetEffects");
        self.0.set_effects(effects);
    }
}

pub struct FileTransferIface(pub Arc<Shared>);

#[zbus::interface(name = "fr.ameleo.Agent.FileTransfer1")]
impl FileTransferIface {
    async fn start(&self, name: &str, size: u64) -> Result<u32> {
        if self.0.locked.load(Ordering::Relaxed) {
            return Err(Error::SessionLocked("the session is locked".into()));
        }
        let agent = self.0.current().ok_or_else(no_agent)?;
        let name = name.to_string();
        let id = on_agent(&agent, |p| async move { p.file_start(&name, size).await }).await?;
        Ok(self.0.add_transfer(agent, id))
    }

    async fn write(&self, id: u32, data: Vec<u8>) -> Result<()> {
        if data.len() > proto::FILE_CHUNK_MAX {
            return Err(Error::InvalidArgument(format!(
                "chunk larger than {}",
                proto::FILE_CHUNK_MAX
            )));
        }
        let (agent, theirs) = self.transfer(id)?;
        let res = on_agent(&agent, |p| async move { p.file_write(theirs, data).await }).await;
        if res.is_err() {
            self.0.transfers.lock().unwrap().remove(&id);
        }
        res
    }

    async fn finish(&self, id: u32) -> Result<String> {
        let (agent, theirs) = self.transfer(id)?;
        self.0.transfers.lock().unwrap().remove(&id);
        on_agent(&agent, |p| async move { p.file_finish(theirs).await }).await
    }

    async fn cancel(&self, id: u32) -> Result<()> {
        let (agent, theirs) = self.transfer(id)?;
        self.0.transfers.lock().unwrap().remove(&id);
        on_agent(&agent, |p| async move { p.file_cancel(theirs).await }).await
    }
}

impl FileTransferIface {
    fn transfer(&self, id: u32) -> Result<(Arc<Agent>, u32)> {
        self.0
            .transfer(id)
            .ok_or_else(|| Error::NotFound(format!("transfer {id}")))
    }
}

pub struct DriveIface(pub Arc<Shared>);

/// Longest wait for a session agent to mount a drive.
const DRIVE_ADD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Client drives: mounted in the active session with FUSE. Their files go
/// from the session agent to `Host.Drive1` through the daemon
/// ([`super::agents`]).
#[zbus::interface(name = "fr.ameleo.Agent.Drive1")]
impl DriveIface {
    /// Returns the mount point, or `""` when no user session is in front
    /// of the screen: the drive is mounted in the next one.
    async fn add_drive(&self, id: u32, name: &str, label: &str) -> Result<String> {
        tracing::info!(id, name, label, "AddDrive");
        if name.len() > 255 || label.len() > 255 {
            return Err(Error::InvalidArgument("name or label too long".into()));
        }
        {
            let mut drives = self.0.drives.lock().unwrap();
            if !drives.contains_key(&id) && drives.len() >= proto::drive::MAX_DRIVES as usize {
                return Err(Error::Failed(format!(
                    "at most {} client drives",
                    proto::drive::MAX_DRIVES
                )));
            }
            drives.insert(
                id,
                super::DriveInfo {
                    name: name.to_string(),
                    label: label.to_string(),
                },
            );
        }
        let Some(agent) = self.0.current() else {
            return Ok(String::new());
        };
        let res = tokio::time::timeout(DRIVE_ADD_TIMEOUT, agent.drive().drive_add(id, name, label))
            .await
            .map_err(|_| Error::Failed("the session agent did not answer".into()))
            .and_then(|r| r.map_err(to_proto));
        match &res {
            Ok(path) => tracing::info!(id, path, "client drive mounted"),
            Err(e) => {
                tracing::warn!(id, "client drive not mounted: {e}");
                self.0.drives.lock().unwrap().remove(&id);
            }
        }
        res
    }

    async fn remove_drive(&self, id: u32) {
        tracing::info!(id, "RemoveDrive");
        if self.0.drives.lock().unwrap().remove(&id).is_none() {
            return;
        }
        if let Some(agent) = self.0.current()
            && let Err(e) = agent.drive().drive_remove(id).await
        {
            tracing::warn!(id, "client drive not unmounted: {e}");
        }
    }

    async fn changed(&self, id: u32, changes: Vec<proto::drive::Change>) {
        tracing::debug!(id, count = changes.len(), "client drive changes");
        if !self.0.drives.lock().unwrap().contains_key(&id) {
            return;
        }
        if let Some(agent) = self.0.current() {
            let _ = agent.drive().drive_changed(id, changes).await;
        }
    }
}
