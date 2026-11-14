# ameleo-display-agent-linux

A Linux guest agent for QEMU `-display dbus`, written in Rust. It replaces spice-vdagent: the
host and the agent talk over one virtio-serial port, with a peer-to-peer D-Bus session on it and
one versioned D-Bus interface per feature, as defined by
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol). It is the Linux
counterpart of
[ameleo-display-agent-windows](https://github.com/ameleo/ameleo-display-agent-windows).

## QEMU setup

```
-device virtio-serial-pci,id=virtserial1
-chardev dbus,id=agent0,name=fr.ameleo.agent.0
-device virtserialport,name=fr.ameleo.agent.0,chardev=agent0,bus=virtserial1.0,nr=1
```

The guest sees the port as `/dev/virtio-ports/fr.ameleo.agent.0`. The agent does not use the
`qemu-vdagent` chardev or the `com.redhat.spice.0` port.

## What it does

| Interface | Status |
|---|---|
| `fr.ameleo.Agent1` | Version, channels, `SessionLocked` (from logind), `Reset()` (also locks the session) |
| `fr.ameleo.Agent.Mouse1` | Absolute pointer over all monitors, 5 buttons, both wheels with high resolution scrolling |
| `fr.ameleo.Agent.Clipboard1` | Text, PNG and BMP, both ways |
| `fr.ameleo.Agent.FileTransfer1` | Files saved in the user's Downloads folder, shown in the file manager |
| `fr.ameleo.Agent.Display1` | Wallpaper, font smoothing and animations, on GNOME and Plasma |
| `FileClipboard1`, `Camera1`, `Printer1`, `Drive1` | Not yet: the agent does not list them in `Channels` |

Supported desktops: Wayland sessions of GNOME (45 or later), Plasma 6 and the wlroots
compositors. X11 sessions are not supported.

## Design

The agent is one program with two roles, like spice-vdagentd and spice-vdagent:

```
QEMU host ── virtio-serial ── daemon (root) ──── /run/ameleo-display-agent/session.sock
                               │  uinput pointer          │ peer-to-peer D-Bus
                               │  logind                  │
                               └──────────────────── session agent (user, one per session)
                                                       clipboard, monitors, files, effects
```

* **`ameleo-display-agent daemon`** runs as root (system service, started by udev when the
  port appears). It owns the port and serves the host's D-Bus session on it. It injects the
  pointer through a uinput absolute mouse, and follows the seat0 session through logind: who is
  in front of the screen, and whether it is locked.
* **`ameleo-display-agent session`** runs as the user in each graphical session (systemd user
  service). It connects to the daemon's socket and does what needs the user's desktop.

The daemon serves `fr.ameleo.DisplayAgent.Daemon1` to each session agent, which serves
`fr.ameleo.DisplayAgent.Session1` back (see `src/ipc.rs`). Only the agent of the user of the
active seat0 session gets the host's requests; the other users' agents get nothing.

| Module | Role |
|---|---|
| `channel/` | The virtio-serial port and the D-Bus framing on it (shared with the Windows agent) |
| `daemon/` | The host's interfaces, the pointer, logind, the session agents |
| `session/` | The clipboard (Wayland or GNOME Shell), monitors, file transfer, desktop effects |
| `ipc` | The private contract between the daemon and the session agents |
| `gnome-shell-extension/` | The clipboard on GNOME |

## How the agent implements the protocol

### Session and port

The Linux virtio console driver reports the host's side of the port: while the host is away,
reads return end of file and `poll` returns `POLLHUP`. The daemon takes the first end of file
after data as a disconnect and resets its D-Bus framer, so a message cut in the middle never
swallows the next handshake. Like the Windows agent, it also follows the D-Bus framing to spot
a new handshake, whatever the port says.

### Pointer

The uinput device is an absolute mouse (`ABS_X`/`ABS_Y`, buttons, wheels), like QEMU's USB
tablet: the compositor maps it to the bounding box of all monitors, in logical coordinates.
The session agent reads the layout from `wl_output` and `xdg_output`: connector `Virtual-N` is
scanout `N-1`, the host's `head`. The host's coordinates are in pixels of the head's mode; the
daemon scales them to the logical size, so HiDPI and fractional scaling work.

Without a session agent (on the login screen), the daemon reads the heads from DRM and puts
them side by side at scale 1, the compositors' default.

### Clipboard

* **Plasma and wlroots**: the session agent uses `ext-data-control-v1` (or
  `wlr-data-control-unstable-v1`), which works without a window. Host data is fetched only when
  a guest app pastes.
* **GNOME**: Mutter has no data control protocol, so the clipboard goes through the agent's
  GNOME Shell extension. The extension answers only `/usr/bin/ameleo-display-agent` (or
  `/usr/local/bin`), so other apps cannot use it to get around Wayland's clipboard rules. The
  Shell needs the data with the selection: when the host copies, the agent fetches it at once,
  in one type (text, else PNG).
* Text is UTF-8 with `\n` on both sides. Images are converted between PNG and BMP when one side
  has only the other; JPEG copies are exported as PNG.
* The clipboard is not given out while the session is locked.

### File transfer

Files go to `XDG_DOWNLOAD_DIR` (`~/Downloads`), as the user, named `name (n).ext` when the name
is taken. The agent checks the free space first and refuses names with `/`, control
characters, `.` or `..`. An unfinished file is removed. The file manager shows the new file
(`org.freedesktop.FileManager1.ShowItems`). Transfers are refused while the session is locked.

### Desktop effects

The daemon keeps the host's choice in `/var/lib/ameleo-display-agent/effects`; each session
agent applies it when it starts, so it holds across reboots and logons. Turning an effect off
saves the user's own setting first, in
`~/.local/state/ameleo-display-agent/effects-saved`, and turning it back on restores it.

| Effect | GNOME (`gsettings`) | Plasma |
|---|---|---|
| Wallpaper | `org.gnome.desktop.background picture-options 'none'` | Wallpaper plugin `org.kde.color` (Plasma script) |
| Font smoothing | `org.gnome.desktop.interface font-antialiasing 'none'` | `kdeglobals` `[General] XftAntialias=false` |
| Animations | `org.gnome.desktop.interface enable-animations false` | `kdeglobals` `[KDE] AnimationDurationFactor=0` |

## Building

With stable Rust (edition 2024):

```
make
```

## Installing in the guest

```
make                      # build, as yourself
sudo make deploy          # install, then (re)start the services
```

`deploy` installs the files under `/usr` (`PREFIX`), reloads systemd and udev, starts the daemon
if the agent port is there (or restarts it on the new binary), and enables the session agent
for every user's graphical session. Run it again after each change.

* udev starts `ameleo-display-agent.service` whenever the port appears, at boot too; it needs no
  `enable`, and stops with the port.
* The session agent starts with each graphical session. `make restart-session` restarts yours
  on the new binary, without root.
* On GNOME, enable the extension (for each user, or with a dconf default):
  `gnome-extensions enable ameleo-display-agent@ameleo.fr`, then log out and in again.
* `sudo make uninstall` removes everything.

If spice-vdagent is installed, stop and disable it (`spice-vdagentd.service`, and the
`spice-vdagent` autostart entry): both agents would drive the pointer and the clipboard.

### Debugging

```
journalctl -u ameleo-display-agent            # the daemon
journalctl --user -u ameleo-display-agent-session
```

| Variable | Default | Effect |
|---|---|---|
| `AMELEO_DISPLAY_AGENT_LOG` | `info` | The `tracing` filter, such as `debug` or `debug,ameleo_display_agent::daemon::host=trace` (to see pointer events) |
| `AMELEO_DISPLAY_AGENT_SOCKET` | `/run/ameleo-display-agent/session.sock` | The daemon's socket for the session agents (development) |

Set them with `systemctl edit` (`[Service]` `Environment=...`).

### Tests

`cargo test` runs the unit tests and an end-to-end test: a host client, through the framer, to
the daemon, to a session agent. In a Wayland session with wl-clipboard,
`cargo build && cargo test -- --ignored` also runs the real session agent against the desktop's
clipboard, both ways (it changes the clipboard).

## Comparison with spice-vdagent

| spice-vdagent | This agent |
|---|---|
| Absolute mouse through uinput, multiple monitors | Same, with the layout of Wayland compositors, HiDPI and fractional scaling, plus the horizontal wheel and high resolution scrolling |
| Clipboard text and images, X11 and (partly) Wayland | Text, PNG and BMP on Wayland: GNOME, Plasma, wlroots |
| File transfer to Downloads, `name (n).ext` | Same, with flow control through method replies, and refused while locked |
| Monitor configuration | **Left out on purpose**: the layout belongs to the display driver and the compositor |
| Capability bitmask | Replaced by D-Bus interfaces and introspection |

## Not supported yet

* The file clipboard (`FileClipboard1`), client cameras, printers and drives.
* X11 sessions.
* The clipboard on the login screen.
* Distribution packages (RPM, deb).

## License

The agent is free software under the GNU General Public License, version 3 or (at your option)
any later version; see [LICENSE](LICENSE). The D-Bus interfaces it implements,
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol), are under the MIT
or Apache-2.0 license, so that other hosts and agents can implement them.
