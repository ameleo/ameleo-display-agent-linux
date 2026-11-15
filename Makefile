# Builds and installs the agent:
#
#   make                  build, as yourself
#   sudo make deploy      install, and (re)start the services
#   make restart-session  restart your session agent (no root needed)
#
# Run `sudo make deploy` again after each change: it puts the new binary in
# place and restarts the daemon if it runs.

PREFIX ?= /usr
DESTDIR ?=
BINDIR = $(PREFIX)/bin
LIBDIR = $(PREFIX)/lib
DATADIR = $(PREFIX)/share
EXTENSION = ameleo-display-agent@ameleo.fr
# Where CUPS looks for its backends.
CUPS_SERVERBIN ?= $(shell cups-config --serverbin 2>/dev/null || echo $(PREFIX)/lib/cups)

.PHONY: all install setup deploy restart-session uninstall

all:
	cargo build --release --locked

install:
	install -Dm755 target/release/ameleo-display-agent $(DESTDIR)$(BINDIR)/ameleo-display-agent
	install -Dm644 packaging/ameleo-display-agent.service $(DESTDIR)$(LIBDIR)/systemd/system/ameleo-display-agent.service
	install -Dm644 packaging/ameleo-display-agent-session.service $(DESTDIR)$(LIBDIR)/systemd/user/ameleo-display-agent-session.service
	install -Dm644 packaging/60-ameleo-display-agent.rules $(DESTDIR)$(LIBDIR)/udev/rules.d/60-ameleo-display-agent.rules
	install -Dm644 packaging/modules-load.conf $(DESTDIR)$(LIBDIR)/modules-load.d/ameleo-display-agent.conf
	install -Dm644 gnome-shell-extension/metadata.json $(DESTDIR)$(DATADIR)/gnome-shell/extensions/$(EXTENSION)/metadata.json
	install -Dm644 gnome-shell-extension/extension.js $(DESTDIR)$(DATADIR)/gnome-shell/extensions/$(EXTENSION)/extension.js
	install -Dm755 packaging/cups-backend $(DESTDIR)$(CUPS_SERVERBIN)/backend/ameleo

# After install, as root: systemd and udev pick up the files, the daemon
# starts if the agent port is there (or restarts on the new binary), and
# every user's graphical session starts the session agent.
setup:
	systemctl daemon-reload
	udevadm control --reload
	udevadm trigger --action=add --subsystem-match=virtio-ports
	systemctl --global enable ameleo-display-agent-session.service
	systemctl try-restart ameleo-display-agent.service

deploy: install setup

# As the user of the graphical session.
restart-session:
	systemctl --user daemon-reload
	systemctl --user restart ameleo-display-agent-session.service

uninstall:
	-systemctl stop ameleo-display-agent.service
	-systemctl --global disable ameleo-display-agent-session.service
	rm -f $(DESTDIR)$(BINDIR)/ameleo-display-agent
	rm -f $(DESTDIR)$(LIBDIR)/systemd/system/ameleo-display-agent.service
	rm -f $(DESTDIR)$(LIBDIR)/systemd/user/ameleo-display-agent-session.service
	rm -f $(DESTDIR)$(LIBDIR)/udev/rules.d/60-ameleo-display-agent.rules
	rm -f $(DESTDIR)$(LIBDIR)/modules-load.d/ameleo-display-agent.conf
	rm -rf $(DESTDIR)$(DATADIR)/gnome-shell/extensions/$(EXTENSION)
	rm -f $(DESTDIR)$(CUPS_SERVERBIN)/backend/ameleo
