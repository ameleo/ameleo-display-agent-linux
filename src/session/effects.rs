// SPDX-License-Identifier: GPL-3.0-or-later

//! Desktop effects turned off for remote sessions: wallpaper, font
//! smoothing, animations.
//!
//! The daemon keeps the host's choice across reboots and gives it to every
//! session agent when it starts, so it applies again at each logon. Turning
//! an effect off saves the user's own setting first
//! (`$XDG_STATE_HOME/ameleo-display-agent/effects-saved`); turning it back on
//! restores that setting instead of forcing a default.
//!
//! * GNOME: `gsettings` (`org.gnome.desktop.background picture-options`,
//!   `org.gnome.desktop.interface font-antialiasing` and
//!   `enable-animations`).
//! * Plasma: `kdeglobals` through `kwriteconfig6 --notify`
//!   (`AnimationDurationFactor`, `XftAntialias`), and the wallpaper plugin
//!   of each desktop through a Plasma script (`org.kde.color` when off).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use tokio::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Desktop {
    Gnome,
    Plasma,
    Other,
}

fn desktop() -> Desktop {
    let current = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let has = |name: &str| current.split(':').any(|d| d.eq_ignore_ascii_case(name));
    if has("GNOME") {
        Desktop::Gnome
    } else if has("KDE") {
        Desktop::Plasma
    } else {
        Desktop::Other
    }
}

const WALLPAPER: &str = "wallpaper";
const FONT: &str = "font-smoothing";
const ANIMATION: &str = "animation";

fn saved_path() -> Option<PathBuf> {
    dirs::state_dir().map(|d| d.join("ameleo-display-agent").join("effects-saved"))
}

/// The user's own settings, saved while the effect is off.
fn load_saved() -> BTreeMap<String, String> {
    let Some(path) = saved_path() else {
        return BTreeMap::new();
    };
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn store_saved(saved: &BTreeMap<String, String>) {
    let Some(path) = saved_path() else { return };
    let text: String = saved.iter().map(|(k, v)| format!("{k}\t{v}\n")).collect();
    let res = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, text));
    if let Err(e) = res {
        tracing::warn!("cannot save {}: {e}", path.display());
    }
}

async fn output(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .await
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{cmd} {args:?}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Applies `(no_wallpaper, no_font_smoothing, no_animation)`.
pub async fn apply(off: (bool, bool, bool)) {
    let desktop = desktop();
    if desktop == Desktop::Other {
        tracing::info!("desktop effects: not supported on this desktop");
        return;
    }
    let mut saved = load_saved();
    for (key, off) in [(WALLPAPER, off.0), (FONT, off.1), (ANIMATION, off.2)] {
        let res = if off {
            if !saved.contains_key(key) {
                match read(desktop, key).await {
                    Ok(v) => {
                        saved.insert(key.to_string(), v);
                        store_saved(&saved);
                    }
                    Err(e) => {
                        tracing::warn!("cannot read the {key} setting: {e}");
                        continue;
                    }
                }
            }
            turn_off(desktop, key).await
        } else if let Some(v) = saved.remove(key) {
            store_saved(&saved);
            restore(desktop, key, &v).await
        } else {
            Ok(())
        };
        match res {
            Ok(()) => tracing::debug!(key, off, "desktop effect applied"),
            Err(e) => tracing::warn!("cannot change the {key} setting: {e}"),
        }
    }
}

/// GNOME schema and key of an effect.
fn gsettings_key(key: &str) -> (&'static str, &'static str, &'static str) {
    match key {
        WALLPAPER => ("org.gnome.desktop.background", "picture-options", "'none'"),
        FONT => ("org.gnome.desktop.interface", "font-antialiasing", "'none'"),
        _ => ("org.gnome.desktop.interface", "enable-animations", "false"),
    }
}

/// `kdeglobals` group and key of an effect.
fn kdeglobals_key(key: &str) -> (&'static str, &'static str, &'static str) {
    match key {
        FONT => ("General", "XftAntialias", "false"),
        _ => ("KDE", "AnimationDurationFactor", "0"),
    }
}

async fn read(desktop: Desktop, key: &str) -> Result<String, String> {
    match desktop {
        Desktop::Gnome => {
            let (schema, k, _) = gsettings_key(key);
            output("gsettings", &["get", schema, k]).await
        }
        Desktop::Plasma if key == WALLPAPER => {
            plasma_script(
                "print(desktops().map(function(d){return d.id+'='+d.wallpaperPlugin}).join(';'))",
            )
            .await
        }
        Desktop::Plasma => {
            let (group, k, _) = kdeglobals_key(key);
            output(
                "kreadconfig6",
                &["--file", "kdeglobals", "--group", group, "--key", k],
            )
            .await
        }
        Desktop::Other => Ok(String::new()),
    }
}

async fn turn_off(desktop: Desktop, key: &str) -> Result<(), String> {
    match desktop {
        Desktop::Gnome => {
            let (schema, k, v) = gsettings_key(key);
            output("gsettings", &["set", schema, k, v]).await.map(drop)
        }
        Desktop::Plasma if key == WALLPAPER => {
            plasma_script("desktops().forEach(function(d){d.wallpaperPlugin='org.kde.color'})")
                .await
                .map(drop)
        }
        Desktop::Plasma => {
            let (group, k, v) = kdeglobals_key(key);
            output(
                "kwriteconfig6",
                &[
                    "--notify",
                    "--file",
                    "kdeglobals",
                    "--group",
                    group,
                    "--key",
                    k,
                    v,
                ],
            )
            .await
            .map(drop)
        }
        Desktop::Other => Ok(()),
    }
}

async fn restore(desktop: Desktop, key: &str, value: &str) -> Result<(), String> {
    match desktop {
        Desktop::Gnome => {
            let (schema, k, _) = gsettings_key(key);
            if value.is_empty() {
                output("gsettings", &["reset", schema, k]).await.map(drop)
            } else {
                output("gsettings", &["set", schema, k, value])
                    .await
                    .map(drop)
            }
        }
        Desktop::Plasma if key == WALLPAPER => plasma_script(&restore_wallpaper_script(value)?)
            .await
            .map(drop),
        Desktop::Plasma => {
            let (group, k, _) = kdeglobals_key(key);
            let mut args = vec![
                "--notify",
                "--file",
                "kdeglobals",
                "--group",
                group,
                "--key",
                k,
            ];
            if value.is_empty() {
                args.push("--delete");
            } else {
                args.push(value);
            }
            output("kwriteconfig6", &args).await.map(drop)
        }
        Desktop::Other => Ok(()),
    }
}

/// The script that gives each desktop its saved wallpaper plugin back.
fn restore_wallpaper_script(saved: &str) -> Result<String, String> {
    let safe = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    let mut map = Vec::new();
    for pair in saved.split(';').filter(|p| !p.is_empty()) {
        let (id, plugin) = pair
            .split_once('=')
            .ok_or_else(|| format!("bad saved wallpaper {pair:?}"))?;
        if !id.chars().all(|c| c.is_ascii_digit()) || id.is_empty() || !safe(plugin) {
            return Err(format!("bad saved wallpaper {pair:?}"));
        }
        map.push(format!("{id}:'{plugin}'"));
    }
    Ok(format!(
        "var m={{{}}};desktops().forEach(function(d){{if(m[d.id])d.wallpaperPlugin=m[d.id]}})",
        map.join(",")
    ))
}

#[zbus::proxy(
    interface = "org.kde.PlasmaShell",
    default_service = "org.kde.plasmashell",
    default_path = "/PlasmaShell",
    gen_blocking = false
)]
trait PlasmaShell {
    fn evaluate_script(&self, script: &str) -> zbus::Result<String>;
}

/// Runs a Plasma script. Plasma may not be up yet at logon: retried.
async fn plasma_script(script: &str) -> Result<String, String> {
    let conn = zbus::Connection::session()
        .await
        .map_err(|e| e.to_string())?;
    let shell = PlasmaShellProxy::new(&conn)
        .await
        .map_err(|e| e.to_string())?;
    let mut last = String::new();
    for _ in 0..20 {
        match shell.evaluate_script(script).await {
            Ok(out) => return Ok(out.trim().to_string()),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    Err(format!("Plasma script failed: {last}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallpaper_script() {
        let s = restore_wallpaper_script("1=org.kde.image;7=org.kde.potd").unwrap();
        assert!(s.starts_with("var m={1:'org.kde.image',7:'org.kde.potd'};"));
        assert!(restore_wallpaper_script("1=a'b").is_err());
        assert!(restore_wallpaper_script("x=org.kde.image").is_err());
    }
}
