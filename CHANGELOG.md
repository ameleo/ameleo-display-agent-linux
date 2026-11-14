# Changelog

## Unreleased

* First version: the virtio-serial port and the host's D-Bus session, `Agent1`, `Mouse1`
  (uinput), `Clipboard1` (text, PNG, BMP on Wayland: GNOME through a Shell extension, Plasma
  and wlroots through data control), `FileTransfer1` and `Display1`.
* systemd services, a udev rule that starts the daemon with the port, and `make deploy`.
