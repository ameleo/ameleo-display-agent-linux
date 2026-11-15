// SPDX-License-Identifier: GPL-3.0-only

//! The GNOME clipboard, through the agent's GNOME Shell extension
//! (`gnome-shell-extension/`): Mutter has no data control protocol, the
//! Shell has the clipboard. The extension serves
//! `fr.ameleo.DisplayAgent.Shell1` on the Shell's connection to the session
//! bus, and answers only this program.
//!
//! The Shell sets a selection with its data: when the host copies, the
//! agent fetches the host data at once (one type) instead of on paste.

use futures_util::StreamExt;
use tokio::sync::mpsc;

use super::wayland::Event;

#[zbus::proxy(
    interface = "fr.ameleo.DisplayAgent.Shell1",
    default_service = "org.gnome.Shell",
    default_path = "/fr/ameleo/DisplayAgent/Shell",
    gen_blocking = false
)]
pub trait Shell {
    /// The types of the current selection.
    fn get_mimetypes(&self) -> zbus::Result<Vec<String>>;
    fn get_content(&self, mime: &str) -> zbus::Result<Vec<u8>>;
    /// Sets the selection: `data` as `mime` (text types are all offered
    /// for a text).
    fn set_content(&self, mime: &str, data: Vec<u8>) -> zbus::Result<()>;
    /// Empties the clipboard if the selection is still the agent's.
    fn clear(&self) -> zbus::Result<()>;
    /// The selection changed: its types; `ours` when the agent set it.
    #[zbus(signal)]
    fn owner_changed(&self, mimes: Vec<String>, ours: bool) -> zbus::Result<()>;
}

#[derive(Clone)]
pub struct Handle {
    shell: ShellProxy<'static>,
}

impl Handle {
    pub async fn read(&self, mime: &str) -> Result<Vec<u8>, String> {
        self.shell
            .get_content(mime)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn set(&self, mime: &str, data: Vec<u8>) -> Result<(), String> {
        self.shell
            .set_content(mime, data)
            .await
            .map_err(|e| e.to_string())
    }

    pub async fn clear(&self) {
        let _ = self.shell.clear().await;
    }
}

/// Connects to the extension and forwards the selection changes.
pub async fn connect(events: mpsc::UnboundedSender<Event>) -> anyhow::Result<Handle> {
    let conn = zbus::Connection::session().await?;
    let shell = ShellProxy::builder(&conn)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await?;
    let mut changes = shell.receive_owner_changed().await?;
    match shell.get_mimetypes().await {
        Ok(mimes) => {
            tracing::info!("clipboard: GNOME Shell extension");
            if !mimes.is_empty() {
                let _ = events.send(Event::Selection(Some(mimes)));
            }
        }
        Err(e) => tracing::warn!(
            "the GNOME Shell extension does not answer ({e}): no clipboard until it is enabled \
             (gnome-extensions enable ameleo-display-agent@ameleo.fr)"
        ),
    }
    tokio::spawn(async move {
        while let Some(sig) = changes.next().await {
            let Ok(args) = sig.args() else { continue };
            if args.ours {
                continue;
            }
            let mimes = args.mimes;
            let _ = events.send(Event::Selection((!mimes.is_empty()).then_some(mimes)));
        }
    });
    Ok(Handle { shell })
}
