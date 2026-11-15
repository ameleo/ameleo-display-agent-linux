// SPDX-License-Identifier: GPL-3.0-only
//
// The clipboard for ameleo-display-agent on GNOME.
//
// Serves fr.ameleo.DisplayAgent.Shell1 at /fr/ameleo/DisplayAgent/Shell on
// the Shell's session bus connection (org.gnome.Shell). Only the agent's
// program may call it: the caller's executable is checked on every call, so
// that other apps of the session do not get around Wayland's clipboard rules.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const PATH = '/fr/ameleo/DisplayAgent/Shell';
const IFACE = `
<node>
  <interface name="fr.ameleo.DisplayAgent.Shell1">
    <method name="GetMimetypes">
      <arg type="as" direction="out" name="mimes"/>
    </method>
    <method name="GetContent">
      <arg type="s" direction="in" name="mime"/>
      <arg type="ay" direction="out" name="data"/>
    </method>
    <method name="SetContent">
      <arg type="s" direction="in" name="mime"/>
      <arg type="ay" direction="in" name="data"/>
    </method>
    <method name="Clear"/>
    <signal name="OwnerChanged">
      <arg type="as" name="mimes"/>
      <arg type="b" name="ours"/>
    </signal>
  </interface>
</node>`;

// Where packages install the agent.
const AGENT_PATHS = [
    '/usr/bin/ameleo-display-agent',
    '/usr/local/bin/ameleo-display-agent',
];

const CLIPBOARD = Meta.SelectionType.SELECTION_CLIPBOARD;
const MAX_READ = 256 * 1024 * 1024;

function callerExecutable(sender) {
    return new Promise((resolve, reject) => {
        Gio.DBus.session.call(
            'org.freedesktop.DBus', '/org/freedesktop/DBus', 'org.freedesktop.DBus',
            'GetConnectionUnixProcessID', new GLib.Variant('(s)', [sender]),
            new GLib.VariantType('(u)'), Gio.DBusCallFlags.NONE, -1, null,
            (conn, res) => {
                try {
                    const [pid] = conn.call_finish(res).deepUnpack();
                    resolve(GLib.file_read_link(`/proc/${pid}/exe`));
                } catch (e) {
                    reject(e);
                }
            });
    });
}

export default class AmeleoDisplayAgentExtension extends Extension {
    enable() {
        this._selection = global.display.get_selection();
        this._ours = null;
        this._ownerChangedId = this._selection.connect('owner-changed',
            (selection, type, source) => {
                if (type !== CLIPBOARD)
                    return;
                const mimes = source ? source.get_mimetypes() : [];
                const ours = source !== null && source === this._ours;
                if (!ours)
                    this._ours = null;
                this._dbus?.emit_signal('OwnerChanged',
                    new GLib.Variant('(asb)', [mimes, ours]));
            });
        this._dbus = Gio.DBusExportedObject.wrapJSObject(IFACE, this);
        this._dbus.export(Gio.DBus.session, PATH);
    }

    disable() {
        this._dbus?.unexport();
        this._dbus = null;
        if (this._ownerChangedId)
            this._selection.disconnect(this._ownerChangedId);
        this._ownerChangedId = 0;
        this._selection = null;
        this._ours = null;
    }

    // Runs `fn` if the caller is the agent, answers its error otherwise.
    async _guarded(invocation, fn) {
        try {
            const exe = await callerExecutable(invocation.get_sender());
            if (!AGENT_PATHS.includes(exe)) {
                invocation.return_dbus_error('org.freedesktop.DBus.Error.AccessDenied',
                    `${exe} is not ameleo-display-agent`);
                return;
            }
            await fn();
        } catch (e) {
            invocation.return_dbus_error('fr.ameleo.Agent.Error.Failed', `${e}`);
        }
    }

    GetMimetypesAsync(params, invocation) {
        this._guarded(invocation, () => {
            // The agent's own selection is no guest copy.
            const mimes = this._ours ? [] : this._selection.get_mimetypes(CLIPBOARD);
            invocation.return_value(new GLib.Variant('(as)', [mimes]));
        });
    }

    GetContentAsync([mime], invocation) {
        this._guarded(invocation, () => new Promise((resolve, reject) => {
            const stream = Gio.MemoryOutputStream.new_resizable();
            this._selection.transfer_async(CLIPBOARD, mime, MAX_READ, stream, null,
                (selection, res) => {
                    try {
                        selection.transfer_finish(res);
                        stream.close(null);
                        const bytes = stream.steal_as_bytes();
                        invocation.return_value(new GLib.Variant('(ay)', [bytes.toArray()]));
                        resolve();
                    } catch (e) {
                        reject(e);
                    }
                });
        }));
    }

    SetContentAsync([mime, data], invocation) {
        this._guarded(invocation, () => {
            const source = Meta.SelectionSourceMemory.new(mime, new GLib.Bytes(data));
            this._ours = source;
            this._selection.set_owner(CLIPBOARD, source);
            invocation.return_value(null);
        });
    }

    ClearAsync(params, invocation) {
        this._guarded(invocation, () => {
            if (this._ours)
                this._selection.unset_owner(CLIPBOARD, this._ours);
            this._ours = null;
            invocation.return_value(null);
        });
    }
}
