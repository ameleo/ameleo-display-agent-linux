# ameleo-display-agent-linux

A Linux guest agent for QEMU `-display dbus`, written in Rust. It replaces spice-vdagent: the
host and the agent talk over one virtio-serial port, with a peer-to-peer D-Bus session on it
and one versioned D-Bus interface per feature, as defined by
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol). The Windows
agent is [ameleo-display-agent-windows](https://github.com/ameleo/ameleo-display-agent-windows).

## QEMU setup

```
-device virtio-serial-pci,id=virtserial1
-chardev dbus,id=agent0,name=fr.ameleo.agent.0
-device virtserialport,name=fr.ameleo.agent.0,chardev=agent0,bus=virtserial1.0,nr=1
```

The guest sees the port as `/dev/virtio-ports/fr.ameleo.agent.0`. The agent does not use the
`qemu-vdagent` chardev or the `com.redhat.spice.0` port.

The agent works in Wayland sessions of GNOME (45 or later), Plasma 6 and the wlroots
compositors. It does not support X11 sessions.

## Repository layout

The agent is one program, `ameleo-display-agent`, with two roles, as spice-vdagentd and
spice-vdagent have:

| Role | Role in the guest |
|---|---|
| `ameleo-display-agent daemon` | The system service, run as root: the port, the host's D-Bus session, the pointer and the session lock |
| `ameleo-display-agent session` | The session agent, run as the user in each graphical session: the clipboard, the monitor layout, files sent by the host and the desktop effects |

The modules of `src`, by feature:

| Module | Role |
|---|---|
| `main`, `ipc` | The entry point, and the private D-Bus interfaces between the daemon and the session agents |
| `channel/` | The virtio-serial port and the D-Bus sessions on it |
| `daemon/` | The host's interfaces, the uinput pointer, logind and the session agents |
| `session/` | The clipboard (Wayland or GNOME Shell), the file clipboard, the monitors, file transfer and desktop effects |

`gnome-shell-extension/` holds the GNOME Shell extension that gives the clipboard to the agent
on GNOME, and `packaging/` the systemd units and the udev rule.

## Building

With stable Rust:

```
make
```

## Installing in the guest

Build as yourself, then install as root:

```
make
sudo make deploy
```

`deploy` installs the agent under `/usr` (set `PREFIX` to change it), reloads systemd and udev,
and enables the session agent for the graphical sessions of every user. Run it again after each
change: it installs the new binary and restarts the daemon if it is running. `sudo make
uninstall` removes everything.

udev starts the daemon, `ameleo-display-agent.service`, when the agent port appears, at boot
too, and systemd stops it when the port goes away; the service needs no `enable`. Each graphical
session starts its session agent, `ameleo-display-agent-session.service`, a systemd user
service. To restart your own session agent on a new binary, without root, run
`make restart-session`.

On GNOME, each user must also enable the extension, then log out and in again:

```
gnome-extensions enable ameleo-display-agent@ameleo.fr
```

To enable it for all users, add it to the `enabled-extensions` key in a dconf default.

If spice-vdagent is installed, stop and disable it (`spice-vdagentd.service` and the
`spice-vdagent` autostart entry): both agents would drive the pointer and the clipboard.

### Debugging

Both roles write their logs to the journal:

```
journalctl -u ameleo-display-agent
journalctl --user -u ameleo-display-agent-session
```

To run the daemon in the foreground, stop the service first, because only one process can open
the port:

```
sudo systemctl stop ameleo-display-agent
sudo AMELEO_DISPLAY_AGENT_LOG=debug ameleo-display-agent daemon
```

| Variable | Default | Effect |
|---|---|---|
| `AMELEO_DISPLAY_AGENT_LOG` | `info` | The `tracing` filter, such as `debug` or `debug,ameleo_display_agent::daemon::host=trace` (to see pointer events) |
| `AMELEO_DISPLAY_AGENT_SOCKET` | `/run/ameleo-display-agent/session.sock` | The socket between the daemon and the session agents, for development |

To set them for the services, use `systemctl edit` and add `Environment=` lines under
`[Service]`.

`cargo test` runs the unit tests, and a test that goes from a host client, through the D-Bus
framing, to the daemon and to a session agent. In a Wayland session with wl-clipboard,
`cargo build && cargo test -- --ignored` also runs the real session agent on the desktop's
clipboard, in both directions. That test changes the clipboard.

## How the agent implements the protocol

[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) documents the D-Bus
interfaces for hosts. This section describes how this agent implements them in Linux.

### Daemon and session agents

* **Daemon.** The daemon runs as root. It owns the port, serves the host's D-Bus session on it,
  and follows the seat0 session through logind: which user is in front of the screen, and
  whether the session is locked. On `Agent1.Reset`, it locks that session.
* **Session agents.** Each graphical session runs a session agent as its user. It connects to
  the daemon on `/run/ameleo-display-agent/session.sock`, with a peer-to-peer D-Bus connection:
  the daemon serves `fr.ameleo.DisplayAgent.Daemon1`, and the session agent serves
  `fr.ameleo.DisplayAgent.Session1` (see `src/ipc.rs`).
* **One user.** The daemon knows the user of each session agent from the socket. Only the agent
  of the user in front of the screen gets the host's requests; the agents of other users get
  nothing.

### Port

The Linux virtio console driver reports the host's side of the port: while the host is away,
reads return the end of the file and `poll` returns `POLLHUP`. The daemon takes the first end of
the file after data as a disconnection, and starts its D-Bus framing again, so that a message
cut in the middle cannot swallow the next handshake. As the Windows agent does, it also follows
the D-Bus framing to see a new handshake, whatever the port reports.

### Pointer

* **Device.** The daemon creates a uinput absolute mouse, as QEMU's USB tablet is: absolute
  axes, five buttons and both wheels, with high resolution scrolling. The compositor maps it to
  the bounding box of all monitors, in logical coordinates.
* **Monitors.** The session agent reads the layout from `wl_output` and `xdg_output`. The
  connector `Virtual-N` is the virtio-gpu scanout `N-1`, the `head` of the protocol. The host
  gives pixels of the head's mode, and the daemon scales them to the logical size, so HiDPI and
  fractional scaling work.
* **Login screen.** Without a session agent, the daemon reads the heads from DRM and places them
  side by side at scale 1, as the compositors do by default.

### Clipboard

* **Plasma and wlroots.** The session agent uses `ext-data-control-v1`, or
  `wlr-data-control-unstable-v1`, which works without a window. It fetches host data only when a
  guest app pastes it.
* **GNOME.** Mutter has no data control protocol, so the clipboard goes through the agent's
  GNOME Shell extension. The extension only answers `/usr/bin/ameleo-display-agent` (or
  `/usr/local/bin`), so that other apps cannot use it to get around the clipboard rules of
  Wayland. The Shell needs the data when the selection is set: when the host copies, the agent
  fetches the data at once, in one format (text if there is any, PNG otherwise).
* **Formats.** Text is UTF-8 with `\n` line endings on both sides. The agent converts images
  between PNG and BMP when one side has only one of them, and exports JPEG copies as PNG.
* **Lock.** The agent does not give out the guest clipboard while the session is locked.

### File clipboard

Files and folders go through the clipboard in both directions, in the MS-RDPECLIP format, so a
host that bridges an RDP client passes them on as they are.

* **Guest to host.** When the user copies files in a file manager (`text/uri-list` or
  `x-special/gnome-copied-files`), the agent offers a file list to the host. It walks the copied
  files and folders as the user, and reads each file only when the host asks for it
  (`FileContents`). It follows a link to a file, but not a link to a folder below the copied
  items, and leaves out pipes and devices.
* **Host to guest.** Linux file managers paste paths, not file contents. So that nothing is
  copied into the guest in advance, the agent mounts a read-only FUSE file system on
  `$XDG_RUNTIME_DIR/ameleo-display-agent/clipboard`, which only the user can see. When the user
  pastes, the agent locks the host's clipboard content (`LockClipData`), puts the host's list in
  a folder of that file system, and gives the paths to the file manager. The file manager's
  reads go to the host as `FileContents` requests while it copies, with its own progress dialog.
  The last four host lists stay readable, so that a long copy survives a few new host copies.
* **Names.** The agent checks the names that come from the host: no `..`, no root and no `/`.
* **Lock.** The host cannot read the guest files while the session is locked.

Without FUSE (`/dev/fuse` and `fusermount3`), host files cannot be pasted; the rest works.

### File transfer

* Files are saved in the user's Downloads folder (`XDG_DOWNLOAD_DIR`), as the user. When the
  name is taken, the agent adds ` (n)` to it.
* The agent checks the free space first, and refuses names with `/`, control characters, `.` or
  `..`. It removes the file of a transfer that does not finish.
* When the file is complete, the file manager shows it (`org.freedesktop.FileManager1`).
* Transfers are refused while the session is locked.

### Desktop effects

The daemon keeps the host's choice in `/var/lib/ameleo-display-agent/effects`, and each session
agent applies it when it starts, so it holds across reboots and is applied again at each logon.
Before it turns an effect off, the agent saves the user's own setting in
`~/.local/state/ameleo-display-agent/effects-saved`; turning the effect back on restores that
setting.

| Effect | GNOME (`gsettings`) | Plasma |
|---|---|---|
| Wallpaper | `org.gnome.desktop.background picture-options 'none'` | The `org.kde.color` wallpaper plugin, through a Plasma script |
| Font smoothing | `org.gnome.desktop.interface font-antialiasing 'none'` | `XftAntialias=false` in `kdeglobals` |
| Animations | `org.gnome.desktop.interface enable-animations false` | `AnimationDurationFactor=0` in `kdeglobals` |

## Comparison with spice-vdagent

| spice-vdagent | This agent |
|---|---|
| Absolute mouse through uinput, multiple monitors | Same, with the layout of the Wayland compositors, HiDPI and fractional scaling, plus the horizontal wheel and high resolution scrolling |
| Clipboard text and images, on X11 and partly on Wayland | Text, PNG and BMP on Wayland: GNOME, Plasma and wlroots |
| Clipboard file lists through spice-webdavd | Replaced by the MS-RDPECLIP file clipboard, both ways, without webdavd: host files are read on demand through FUSE |
| File transfer to Downloads, `name (n).ext` | Same, with flow control through method replies; refused while the session is locked |
| Reset when the client disconnects | `Agent1.Reset()`, which also locks the session, done implicitly on every new session |
| Monitor configuration | **Left out on purpose**: the layout belongs to the display driver and the compositor |
| Capability bitmask | Replaced by D-Bus interfaces and introspection |

## Not supported yet

* Client cameras, client printers and client drives. The agent does not list them in
  `Agent1.Channels`.
* X11 sessions, and the clipboard on the login screen.
* Distribution packages (RPM and deb).

## License

The agent is free software under the GNU General Public License, version 3 or (at your option)
any later version; see [LICENSE](LICENSE). The D-Bus interfaces it implements,
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol), are under the MIT
or Apache-2.0 license, so that other hosts and agents can implement them.
