# Changelog

This file lists the changes in each release. The builds will be available on the
[releases page](https://github.com/ameleo/ameleo-display-agent-linux/releases).

## Unreleased

The first version: a Linux guest agent for QEMU `-display dbus`, written in Rust, which replaces
spice-vdagent and implements
[ameleo-display-protocol](https://github.com/ameleo/ameleo-display-protocol) 0.1.0. It talks to
the host over one virtio-serial port, with a peer-to-peer D-Bus session and one versioned D-Bus
interface per feature. A system daemon owns the port and the pointer; a session agent in each
Wayland session (GNOME, Plasma or wlroots) handles the user's desktop.

- **Mouse:** absolute positioning on several monitors, with HiDPI and fractional scaling, five
  buttons, both wheels and high resolution scrolling.
- **Clipboard:** text and images (PNG and BMP), in both directions. On GNOME, the clipboard goes
  through the agent's GNOME Shell extension.
- **File clipboard:** files and folders in both directions, in the MS-RDPECLIP format. Host
  files are read on demand through a FUSE file system, so nothing is copied in advance.
- **Client cameras:** shown as PipeWire cameras of the user's session; the agent decodes the
  H.264 stream in the guest with the system's OpenH264 library.
- **File transfer:** files sent from the client are saved in Downloads.
- **Display effects:** wallpaper, font smoothing and animations can be turned off on request, on
  GNOME and Plasma, and are restored afterwards.

`make deploy` installs the agent with its systemd services and a udev rule that starts the
daemon when the agent port appears. There are no distribution packages yet. Client printers,
client drives and camera controls are not supported yet.
