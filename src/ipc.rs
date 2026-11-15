// SPDX-License-Identifier: GPL-3.0-or-later

//! Private D-Bus contract between the system daemon and the session agents.
//!
//! The daemon (root) owns the virtio-serial port and the host session. Each
//! graphical session runs a session agent as its user, which connects to
//! [`SOCKET`]: a peer-to-peer D-Bus connection, the daemon being the server
//! (`EXTERNAL` authentication, so it knows the agent's uid and pid). On it:
//!
//! * the session agent serves [`SESSION_PATH`] (`fr.ameleo.DisplayAgent.Session1`):
//!   what the host asks of the user's desktop (clipboard, files, effects);
//! * the daemon serves [`DAEMON_PATH`] (`fr.ameleo.DisplayAgent.Daemon1`):
//!   what the desktop tells the host (clipboard changes, pastes of host
//!   data, the monitor layout for the pointer), and
//!   `fr.ameleo.DisplayAgent.HostDrive1`: the client drives' files.
//!
//! Only the agent of the active session of seat0 gets host requests.

use proto::Error;

pub const SOCKET: &str = "/run/ameleo-display-agent/session.sock";

/// [`SOCKET`], or `AMELEO_DISPLAY_AGENT_SOCKET` (tests, development).
pub fn socket() -> std::path::PathBuf {
    std::env::var_os("AMELEO_DISPLAY_AGENT_SOCKET")
        .map(Into::into)
        .unwrap_or_else(|| SOCKET.into())
}
pub const SESSION_PATH: &str = "/fr/ameleo/DisplayAgent/Session";
pub const DAEMON_PATH: &str = "/fr/ameleo/DisplayAgent/Daemon";

/// One monitor as the compositor lays it out: `(head, x, y, width, height,
/// mode_width, mode_height)`. `head` is the virtio-gpu scanout index, the
/// rectangle is in the compositor's logical coordinates, the mode in
/// pixels (the host's coordinates).
pub type Head = (u32, i32, i32, u32, u32, u32, u32);

#[zbus::proxy(
    interface = "fr.ameleo.DisplayAgent.Session1",
    default_service = "fr.ameleo.DisplayAgent.Session",
    default_path = "/fr/ameleo/DisplayAgent/Session",
    gen_blocking = false
)]
pub trait Session {
    /// The host owns the clipboard, with `mimes`.
    fn clipboard_grab(&self, serial: u32, mimes: Vec<String>) -> zbus::Result<()>;
    /// The host gave up its ownership.
    fn clipboard_release(&self) -> zbus::Result<()>;
    /// The guest clipboard in the first of `mimes` it can give.
    fn clipboard_request(&self, mimes: Vec<String>) -> zbus::Result<(String, Vec<u8>)>;
    /// Sends the current guest clipboard again (`ClipboardGrabbed`).
    fn announce(&self) -> zbus::Result<()>;

    fn set_effects(
        &self,
        no_wallpaper: bool,
        no_font_smoothing: bool,
        no_animation: bool,
    ) -> zbus::Result<()>;

    fn file_start(&self, name: &str, size: u64) -> zbus::Result<u32>;
    fn file_write(&self, id: u32, data: Vec<u8>) -> zbus::Result<()>;
    fn file_finish(&self, id: u32) -> zbus::Result<String>;
    fn file_cancel(&self, id: u32) -> zbus::Result<()>;

    /// `FileClipboard1`, for the files copied in the guest.
    fn lock_clip_data(&self, clip_data_id: u32) -> zbus::Result<()>;
    fn unlock_clip_data(&self, clip_data_id: u32) -> zbus::Result<()>;
    #[allow(clippy::too_many_arguments)]
    fn file_contents(
        &self,
        lindex: i32,
        flags: u32,
        position: u64,
        requested: u32,
        clip_data_id: u32,
        has_clip_data_id: bool,
    ) -> zbus::Result<Vec<u8>>;

    /// The host session ended or the client went away: drop the host
    /// clipboard and abort the transfers.
    fn reset(&self) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "fr.ameleo.DisplayAgent.Daemon1",
    default_service = "fr.ameleo.DisplayAgent.Daemon",
    default_path = "/fr/ameleo/DisplayAgent/Daemon",
    gen_blocking = false
)]
pub trait Daemon {
    /// The monitors of the session, see [`Head`].
    fn set_layout(&self, heads: Vec<Head>) -> zbus::Result<()>;
    /// The guest clipboard changed.
    fn clipboard_grabbed(&self, serial: u32, mimes: Vec<String>) -> zbus::Result<()>;
    /// The guest clipboard holds nothing exportable anymore.
    fn clipboard_released(&self) -> zbus::Result<()>;
    /// `Host.FileClipboard1`, for the host files pasted in the guest.
    fn host_lock_clip_data(&self, clip_data_id: u32) -> zbus::Result<()>;
    fn host_unlock_clip_data(&self, clip_data_id: u32) -> zbus::Result<()>;
    #[allow(clippy::too_many_arguments)]
    fn host_file_contents(
        &self,
        lindex: i32,
        flags: u32,
        position: u64,
        requested: u32,
        clip_data_id: u32,
        has_clip_data_id: bool,
    ) -> zbus::Result<Vec<u8>>;
    /// An app started client camera `id` in `format` (index into its
    /// formats), or stopped it (`-1`).
    fn camera_stream(&self, id: u32, format: i32) -> zbus::Result<()>;
    /// The decoder of camera `id` needs an IDR.
    fn camera_key_frame(&self, id: u32) -> zbus::Result<()>;
    /// An app changed a control of camera `id` (`ControlRequested`).
    fn camera_control(
        &self,
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    ) -> zbus::Result<()>;
    /// A guest app pastes host data: `Host.Clipboard1.Request`.
    fn host_clipboard_request(
        &self,
        serial: u32,
        mimes: Vec<String>,
    ) -> zbus::Result<(String, Vec<u8>)>;
}

/// The client cameras of the session agent, on [`SESSION_PATH`]. Its own
/// interface, served in call order: frames must not overtake each other.
#[zbus::proxy(
    interface = "fr.ameleo.DisplayAgent.Camera1",
    default_service = "fr.ameleo.DisplayAgent.Session",
    default_path = "/fr/ameleo/DisplayAgent/Session",
    gen_blocking = false
)]
pub trait SessionCamera {
    /// `formats`: `(width, height, fps numerator, fps denominator)`.
    fn camera_add(
        &self,
        id: u32,
        name: &str,
        formats: Vec<(u32, u32, u32, u32)>,
    ) -> zbus::Result<()>;
    fn camera_remove(&self, id: u32) -> zbus::Result<()>;
    fn camera_remove_all(&self) -> zbus::Result<()>;
    /// The client's controls, see `proto::Camera::set_controls`.
    fn camera_controls(
        &self,
        id: u32,
        controls: Vec<proto::camera_control::Control>,
    ) -> zbus::Result<()>;
    /// What a control is now (`ControlValue`).
    fn camera_control_value(
        &self,
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    ) -> zbus::Result<()>;
    /// One H.264 access unit.
    #[zbus(no_reply)]
    fn camera_frame(&self, id: u32, data: Vec<u8>) -> zbus::Result<()>;
}

/// The client drives of the session agent, on [`SESSION_PATH`].
#[zbus::proxy(
    interface = "fr.ameleo.DisplayAgent.Drive1",
    default_service = "fr.ameleo.DisplayAgent.Session",
    default_path = "/fr/ameleo/DisplayAgent/Session",
    gen_blocking = false
)]
pub trait SessionDrive {
    /// Mounts client drive `id`; returns its mount point.
    fn drive_add(&self, id: u32, name: &str, label: &str) -> zbus::Result<String>;
    fn drive_remove(&self, id: u32) -> zbus::Result<()>;
    fn drive_remove_all(&self) -> zbus::Result<()>;
    /// Changes made on the client, see `proto::Drive::changed`.
    fn drive_changed(&self, id: u32, changes: Vec<proto::drive::Change>) -> zbus::Result<()>;
}

/// `Host.Drive1`, through the daemon, on [`DAEMON_PATH`]: the same methods,
/// for the active agent only. A D-Bus error means the client could not be
/// asked (see `proto::HostDrive`).
#[zbus::proxy(
    interface = "fr.ameleo.DisplayAgent.HostDrive1",
    default_service = "fr.ameleo.DisplayAgent.Daemon",
    default_path = "/fr/ameleo/DisplayAgent/Daemon",
    gen_blocking = false
)]
pub trait HostDrive {
    #[allow(clippy::too_many_arguments)]
    fn open(
        &self,
        drive_id: u32,
        path: &str,
        desired_access: u32,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
    ) -> zbus::Result<(u32, u32, u32)>;
    fn close(&self, drive_id: u32, file_id: u32) -> zbus::Result<u32>;
    fn read(
        &self,
        drive_id: u32,
        file_id: u32,
        offset: u64,
        length: u32,
    ) -> zbus::Result<(u32, Vec<u8>)>;
    fn write(
        &self,
        drive_id: u32,
        file_id: u32,
        offset: u64,
        data: Vec<u8>,
    ) -> zbus::Result<(u32, u32)>;
    fn flush(&self, drive_id: u32, file_id: u32) -> zbus::Result<u32>;
    fn query(&self, drive_id: u32, file_id: u32) -> zbus::Result<(u32, proto::drive::Entry)>;
    fn set_basic(
        &self,
        drive_id: u32,
        file_id: u32,
        attributes: u32,
        creation_time: u64,
        last_access_time: u64,
        last_write_time: u64,
    ) -> zbus::Result<u32>;
    fn set_size(
        &self,
        drive_id: u32,
        file_id: u32,
        size: u64,
        allocation: bool,
    ) -> zbus::Result<u32>;
    fn set_delete(&self, drive_id: u32, file_id: u32, delete: bool) -> zbus::Result<u32>;
    fn rename(
        &self,
        drive_id: u32,
        file_id: u32,
        new_path: &str,
        replace: bool,
    ) -> zbus::Result<u32>;
    fn list(&self, drive_id: u32, path: &str) -> zbus::Result<(u32, Vec<proto::drive::Entry>)>;
    fn volume(&self, drive_id: u32) -> zbus::Result<(u32, u64, u64, String, String)>;
}

/// An error of the other side, as the protocol error it was.
pub fn to_proto(e: zbus::Error) -> Error {
    if let zbus::Error::MethodError(name, msg, _) = &e {
        let msg = msg.clone().unwrap_or_default();
        if let Some(kind) = name.as_str().strip_prefix("fr.ameleo.Agent.Error.") {
            return match kind {
                "SessionLocked" => Error::SessionLocked(msg),
                "NotEnoughSpace" => Error::NotEnoughSpace(msg),
                "InvalidArgument" => Error::InvalidArgument(msg),
                "NotFound" => Error::NotFound(msg),
                "NoData" => Error::NoData(msg),
                _ => Error::Failed(msg),
            };
        }
    }
    Error::Failed(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_keep_their_kind() {
        // Round trip through a real p2p connection is covered by the
        // daemon tests; here, a plain error that is not ours.
        let e = to_proto(zbus::Error::Failure("boom".into()));
        assert!(matches!(e, Error::Failed(m) if m.contains("boom")));
    }
}
