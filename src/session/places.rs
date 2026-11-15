// SPDX-License-Identifier: GPL-3.0-only

//! The client drives in the Places panel of KDE (Dolphin, the file
//! dialogs): bookmarks in `~/.local/share/user-places.xbel`.
//!
//! Dolphin's Devices come from Solid, which lists udisks devices and a few
//! network file systems, not FUSE mounts like ours. So the agent adds a
//! bookmark for each mounted drive, and removes it when the drive goes.
//! KDE reads the file again when it changes. GNOME Files needs nothing: it
//! shows the mounts of the home folder on its own.
//!
//! The agent's bookmarks are the ones under the drives' folder; the file is
//! edited as text, the rest left as it is. Without the file (no KDE), the
//! agent adds nothing.

use std::path::{Path, PathBuf};

use super::filelist::file_uri;

/// KDE's places file.
pub fn default_file() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join("user-places.xbel"))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The `href` of `path`, as in the file.
fn href(path: &Path) -> String {
    escape(&file_uri(path))
}

/// `xml` without the bookmarks whose `href` is `exact`, or starts with
/// `prefix`.
fn without(xml: &str, exact: Option<&str>, prefix: Option<&str>) -> String {
    let mut out = String::with_capacity(xml.len());
    let mut rest = xml;
    const OPEN: &str = "<bookmark href=\"";
    while let Some(at) = rest.find(OPEN) {
        let after = &rest[at + OPEN.len()..];
        let target = after.split('"').next().unwrap_or_default();
        let ours =
            exact.is_some_and(|e| target == e) || prefix.is_some_and(|p| target.starts_with(p));
        let end = rest[at..]
            .find("</bookmark>")
            .map(|e| at + e + "</bookmark>".len());
        match (ours, end) {
            (true, Some(end)) => {
                // With the indentation before it and the line end after it.
                let line_start = rest[..at].trim_end_matches([' ', '\t']).len();
                out.push_str(&rest[..line_start]);
                rest = &rest[end..];
                if let Some(r) = rest.strip_prefix('\n') {
                    rest = r;
                }
            }
            _ => {
                out.push_str(&rest[..at + OPEN.len()]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// `xml` with a bookmark to `target`, before the end of the file.
fn with(xml: &str, target: &str, title: &str, id: &str) -> Option<String> {
    let end = xml.rfind("</xbel>")?;
    let entry = format!(
        " <bookmark href=\"{target}\">\n  <title>{}</title>\n  <info>\n   <metadata owner=\"http://freedesktop.org\">\n    <bookmark:icon name=\"drive-harddisk\"/>\n   </metadata>\n   <metadata owner=\"http://www.kde.org\">\n    <ID>{id}</ID>\n   </metadata>\n  </info>\n </bookmark>\n",
        escape(title)
    );
    let mut out = without(xml, Some(target), None);
    let end = out.rfind("</xbel>").unwrap_or(end);
    out.insert_str(end, &entry);
    Some(out)
}

/// Rewrites `file` with `f`, when it exists and `f` changes it.
fn edit(file: &Path, f: impl FnOnce(&str) -> Option<String>) {
    let Ok(xml) = std::fs::read_to_string(file) else {
        return;
    };
    let Some(new) = f(&xml).filter(|n| *n != xml) else {
        return;
    };
    let tmp = file.with_extension("xbel.ameleo-tmp");
    let res = std::fs::write(&tmp, new).and_then(|()| std::fs::rename(&tmp, file));
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("cannot update {}: {e}", file.display());
    }
}

/// Adds a place for the drive mounted on `mount`.
pub fn add(file: &Path, mount: &Path, title: &str, id: u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let target = href(mount);
    edit(file, |xml| {
        with(xml, &target, title, &format!("{secs}/{id}"))
    });
}

/// Removes the place of `mount`.
pub fn remove(file: &Path, mount: &Path) {
    let target = href(mount);
    edit(file, |xml| Some(without(xml, Some(&target), None)));
}

/// Removes every place under `root` (what a killed agent left).
pub fn remove_all(file: &Path, root: &Path) {
    let prefix = format!("{}/", href(root));
    edit(file, |xml| Some(without(xml, None, Some(&prefix))));
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE xbel>
<xbel xmlns:bookmark="http://www.freedesktop.org/standards/desktop-bookmarks">
 <bookmark href="file:///home/u">
  <title>Home</title>
 </bookmark>
</xbel>
"#;

    #[test]
    fn places_added_and_removed() {
        let root = Path::new("/home/u/client-drives");
        let a = href(&root.join("C sur PC"));
        let b = href(&root.join("D & co"));
        let x = with(XML, &a, "C sur PC", "1/1").unwrap();
        let x = with(&x, &b, "D & co", "1/2").unwrap();
        assert!(x.contains("<title>D &amp; co</title>"));
        assert!(x.contains("href=\"file:///home/u/client-drives/C%20sur%20PC\""));
        assert!(x.ends_with(" </bookmark>\n</xbel>\n"));
        // Added again: not twice.
        let x = with(&x, &a, "C sur PC", "1/3").unwrap();
        assert_eq!(x.matches(&a).count(), 1);
        assert_eq!(without(&x, Some(&a), None).matches("<bookmark ").count(), 2);
        let prefix = format!("{}/", href(root));
        assert_eq!(without(&x, None, Some(&prefix)), XML);
    }
}
