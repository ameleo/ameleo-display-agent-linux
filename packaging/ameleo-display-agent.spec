# The ameleo display agent for Fedora. `make rpm` builds it from the Git tree; the release
# workflow builds it for each tag. The build fetches the crates with cargo (Cargo.lock), so
# it needs the network.

# A Rust release build has no debug information to split.
%global debug_package %{nil}

Name:           ameleo-display-agent
Version:        0.1.0
Release:        1%{?dist}
Summary:        Guest agent for QEMU -display dbus (ameleo display protocol)
License:        GPL-3.0-only
URL:            https://github.com/ameleo/ameleo-display-agent-linux
Source0:        %{name}-%{version}.tar.gz
ExclusiveArch:  x86_64

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  clang
BuildRequires:  make
BuildRequires:  pipewire-devel
BuildRequires:  systemd-rpm-macros

# Client printers: CUPS queues, documents made with Ghostscript.
Requires:       cups
Requires:       cups-client
Requires:       ghostscript
# File clipboard and client drives.
Requires:       fuse3
# Client cameras: the system's Cisco OpenH264 (fedora-cisco-openh264 repository).
Recommends:     openh264
Recommends:     pipewire
%{?systemd_requires}

%description
The Linux guest agent of QEMU's -display dbus: it replaces spice-vdagent and implements the
ameleo display protocol over the virtio-serial port fr.ameleo.agent.0. A system daemon owns the
port and the pointer; a session agent in each Wayland session (GNOME, Plasma, wlroots) handles
the clipboard, files, client cameras, drives and printers, and the desktop effects.

%prep
%autosetup

%build
make

%install
%make_install PREFIX=%{_prefix} CUPS_SERVERBIN=%{_prefix}/lib/cups

%post
%systemd_post ameleo-display-agent.service
if [ $1 -eq 1 ]; then
    # Every graphical session starts the session agent.
    systemctl --global enable ameleo-display-agent-session.service >/dev/null 2>&1 || :
    # A daemon installed before (make deploy) runs the new binary.
    systemctl try-restart ameleo-display-agent.service >/dev/null 2>&1 || :
fi
# The daemon starts when the agent port appears (udev), or restarts on the new binary.
udevadm control --reload >/dev/null 2>&1 || :
udevadm trigger --action=add --subsystem-match=virtio-ports >/dev/null 2>&1 || :

%preun
%systemd_preun ameleo-display-agent.service
if [ $1 -eq 0 ]; then
    systemctl --global disable ameleo-display-agent-session.service >/dev/null 2>&1 || :
fi

%postun
%systemd_postun_with_restart ameleo-display-agent.service
udevadm control --reload >/dev/null 2>&1 || :

%files
%license LICENSE
%doc README.md CHANGELOG.md
%{_bindir}/ameleo-display-agent
%{_unitdir}/ameleo-display-agent.service
%{_userunitdir}/ameleo-display-agent-session.service
%{_udevrulesdir}/60-ameleo-display-agent.rules
%{_modulesloaddir}/ameleo-display-agent.conf
%{_datadir}/gnome-shell/extensions/ameleo-display-agent@ameleo.fr/
%{_prefix}/lib/cups/backend/ameleo

%changelog
* Sun Nov 15 2026 Mickael Bourneuf - 0.1.0-1
- First release.
