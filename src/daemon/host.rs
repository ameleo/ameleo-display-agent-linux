// SPDX-License-Identifier: GPL-3.0-or-later

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
async fn on_agent<T, F>(agent: &Arc<Agent>, call: impl FnOnce(crate::ipc::SessionProxy<'static>) -> F) -> Result<T>
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
            iface::DISPLAY,
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
        let (mime, data) = on_agent(&agent, |p| async move { p.clipboard_request(mimes).await }).await?;
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
