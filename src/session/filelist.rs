// SPDX-License-Identifier: GPL-3.0-or-later

//! File lists on the clipboard, MS-RDPECLIP style.
//!
//! Guest to host: files copied in a file manager (`text/uri-list`) become a
//! CLIPRDR_FILELIST, the packed `FILEGROUPDESCRIPTORW` that RDP puts on the
//! wire, and file contents are read on demand
//! (`FileClipboard1.FileContents`). The session agent runs as the user, so
//! it reads only what the user can read.
//!
//! Host to guest: the host's list is decoded here, and served by
//! [`super::hostfiles`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use proto::Error;
use proto::file_contents::{DESCRIPTOR_SIZE, MAX_RANGE, RANGE, SIZE};

const FD_ATTRIBUTES: u32 = 0x0004;
const FD_WRITESTIME: u32 = 0x0020;
const FD_FILESIZE: u32 = 0x0040;
const FD_SHOWPROGRESSUI: u32 = 0x4000;
const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x02;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
/// `cFileName` is `WCHAR[260]`, NUL included.
const MAX_NAME: usize = 259;
/// Guard against copying a whole disk by mistake.
const MAX_ENTRIES: usize = 100_000;
/// FILETIME of the Unix epoch, in 100 ns units.
const UNIX_EPOCH_FILETIME: u64 = 116_444_736_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Name relative to the copied items, `\` separated.
    pub name: String,
    pub attributes: u32,
    pub size: u64,
    /// FILETIME, 0 when unknown.
    pub write_time: u64,
    /// Absolute path in the guest (empty for host entries).
    pub path: PathBuf,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    /// The modification time, for the guest.
    pub fn mtime(&self) -> std::time::SystemTime {
        let units = self.write_time.saturating_sub(UNIX_EPOCH_FILETIME);
        std::time::UNIX_EPOCH + std::time::Duration::from_nanos(units.saturating_mul(100))
    }
}

/// The paths of a `text/uri-list` or `x-special/gnome-copied-files`: local
/// `file:` URIs only.
pub fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(data)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let rest = l
                .strip_prefix("file://localhost/")
                .map(|r| format!("/{r}"))
                .or_else(|| l.strip_prefix("file://").map(String::from))?;
            if !rest.starts_with('/') {
                return None; // another host
            }
            Some(PathBuf::from(OsString::from_vec(percent_decode(&rest))))
        })
        .collect()
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// A `file://` URI for `path`.
pub fn file_uri(path: &Path) -> String {
    let mut uri = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~".contains(&b) {
            uri.push(b as char);
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri
}

/// Lists the copied items and everything below the copied directories,
/// each directory before its content.
pub fn walk(roots: &[PathBuf]) -> Result<Vec<Entry>, Error> {
    let mut out = Vec::new();
    for root in roots {
        let name = root
            .file_name()
            .map(component)
            .ok_or_else(|| Error::InvalidArgument(format!("cannot copy {}", root.display())))?;
        add(root, name, true, &mut out)?;
    }
    Ok(out)
}

/// A file name as one component of a `\` separated name.
fn component(name: &std::ffi::OsStr) -> String {
    name.to_string_lossy().replace('\\', "_")
}

fn add(path: &Path, name: String, root: bool, out: &mut Vec<Entry>) -> Result<(), Error> {
    if out.len() >= MAX_ENTRIES {
        return Err(Error::Failed(format!("more than {MAX_ENTRIES} files")));
    }
    if name.encode_utf16().count() > MAX_NAME {
        return Err(Error::Failed(format!(
            "path too long for the clipboard: {name}"
        )));
    }
    // A link to a file is copied as the file. A link to a directory is
    // not followed below the copied items (it could loop, or leave the
    // tree), only when it is one of them.
    let link = std::fs::symlink_metadata(path)
        .map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?;
    let md = if link.file_type().is_symlink() {
        match std::fs::metadata(path) {
            Ok(md) if md.is_file() || (root && md.is_dir()) => md,
            _ => {
                tracing::debug!("not copied: {}", path.display());
                return Ok(());
            }
        }
    } else {
        link
    };
    if !md.is_file() && !md.is_dir() {
        // A pipe or a device: reading it would block or never end.
        tracing::debug!("not copied: {}", path.display());
        return Ok(());
    }
    let mut attributes = if md.is_dir() {
        FILE_ATTRIBUTE_DIRECTORY
    } else {
        FILE_ATTRIBUTE_ARCHIVE
    };
    if name.rsplit('\\').next().is_some_and(|n| n.starts_with('.')) {
        attributes |= FILE_ATTRIBUTE_HIDDEN;
    }
    if md.permissions().mode() & 0o222 == 0 {
        attributes |= FILE_ATTRIBUTE_READONLY;
    }
    let write_time = (md.mtime().max(0) as u64)
        .saturating_mul(10_000_000)
        .saturating_add(md.mtime_nsec().max(0) as u64 / 100)
        .saturating_add(UNIX_EPOCH_FILETIME);
    out.push(Entry {
        name: name.clone(),
        attributes,
        size: if md.is_dir() { 0 } else { md.len() },
        write_time,
        path: path.to_path_buf(),
    });
    if md.is_dir() {
        let mut children: Vec<_> = std::fs::read_dir(path)
            .map_err(|e| Error::Failed(format!("{}: {e}", path.display())))?
            .filter_map(|e| e.ok())
            .collect();
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let child_name = format!("{name}\\{}", component(&child.file_name()));
            add(&child.path(), child_name, false, out)?;
        }
    }
    Ok(())
}

/// CLIPRDR_FILELIST / FILEGROUPDESCRIPTORW.
pub fn encode(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + entries.len() * DESCRIPTOR_SIZE);
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for e in entries {
        let start = out.len();
        let flags = FD_ATTRIBUTES | FD_WRITESTIME | FD_FILESIZE | FD_SHOWPROGRESSUI;
        out.extend_from_slice(&flags.to_le_bytes());
        out.extend_from_slice(&[0; 16 + 8 + 8]); // clsid, sizel, pointl
        out.extend_from_slice(&e.attributes.to_le_bytes());
        out.extend_from_slice(&[0; 16]); // creation, last access
        out.extend_from_slice(&e.write_time.to_le_bytes());
        out.extend_from_slice(&((e.size >> 32) as u32).to_le_bytes());
        out.extend_from_slice(&(e.size as u32).to_le_bytes());
        let mut name = [0u16; MAX_NAME + 1];
        for (d, s) in name.iter_mut().zip(e.name.encode_utf16().take(MAX_NAME)) {
            *d = s;
        }
        for u in name {
            out.extend_from_slice(&u.to_le_bytes());
        }
        debug_assert_eq!(out.len() - start, DESCRIPTOR_SIZE);
    }
    out
}

/// Parses a CLIPRDR_FILELIST from the host. Names are checked: they become
/// paths in the guest.
pub fn decode(data: &[u8]) -> Result<Vec<Entry>, String> {
    let count = data.get(..4).ok_or("short file list")?;
    let count = u32::from_le_bytes(count.try_into().unwrap()) as usize;
    if count > MAX_ENTRIES || data.len() < 4 + count * DESCRIPTOR_SIZE {
        return Err(format!(
            "file list of {count} entries in {} bytes",
            data.len()
        ));
    }
    let mut out = Vec::with_capacity(count);
    for d in data[4..4 + count * DESCRIPTOR_SIZE]
        .as_chunks::<DESCRIPTOR_SIZE>()
        .0
    {
        let u32_at = |o: usize| u32::from_le_bytes(d[o..o + 4].try_into().unwrap());
        let flags = u32_at(0);
        let attributes = if flags & FD_ATTRIBUTES != 0 {
            u32_at(36)
        } else {
            0
        };
        let write_time = if flags & FD_WRITESTIME != 0 {
            u64::from_le_bytes(d[56..64].try_into().unwrap())
        } else {
            0
        };
        let size = if flags & FD_FILESIZE != 0 {
            (u64::from(u32_at(64)) << 32) | u64::from(u32_at(68))
        } else {
            0
        };
        let units: Vec<u16> = d[72..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .take_while(|&u| u != 0)
            .collect();
        let name = String::from_utf16_lossy(&units);
        check_relative(&name)?;
        out.push(Entry {
            name,
            attributes,
            size,
            write_time,
            path: PathBuf::new(),
        });
    }
    Ok(out)
}

/// A host name must stay below the paste target: no root, no `..`, no
/// `/`, no reserved characters.
fn check_relative(name: &str) -> Result<(), String> {
    let bad = name.is_empty()
        || name.starts_with(['\\', '/'])
        || name.contains(['/', ':', '*', '?', '"', '<', '>', '|'])
        || name.chars().any(char::is_control)
        || name
            .split('\\')
            .any(|c| c.is_empty() || c == "." || c == "..");
    if bad {
        Err(format!("invalid file name in the host list: {name:?}"))
    } else {
        Ok(())
    }
}

/// The file last read for the host, kept open: a client reads a file in
/// many File Contents Requests.
pub type OpenFile = std::sync::Mutex<Option<(PathBuf, File)>>;

/// Answers a File Contents Request on `entries`.
pub fn contents(
    entries: &[Entry],
    open: &OpenFile,
    lindex: i32,
    flags: u32,
    position: u64,
    requested: u32,
) -> Result<Vec<u8>, Error> {
    let entry = usize::try_from(lindex)
        .ok()
        .and_then(|i| entries.get(i))
        .ok_or_else(|| Error::NotFound(format!("file index {lindex}")))?;
    if entry.is_dir() {
        return Err(Error::InvalidArgument(format!(
            "{} is a directory",
            entry.name
        )));
    }
    let mut open = open.lock().unwrap();
    if open.as_ref().is_none_or(|(p, _)| *p != entry.path) {
        let file = File::open(&entry.path)
            .map_err(|e| Error::Failed(format!("{}: {e}", entry.path.display())))?;
        *open = Some((entry.path.clone(), file));
    }
    let file = &open.as_ref().unwrap().1;
    if flags & SIZE != 0 {
        let size = file
            .metadata()
            .map_err(|e| Error::Failed(e.to_string()))?
            .len();
        return Ok(size.to_le_bytes().to_vec());
    }
    if flags & RANGE == 0 {
        return Err(Error::InvalidArgument(format!("flags {flags:#x}")));
    }
    let want = requested.min(MAX_RANGE) as usize;
    let mut buf = vec![0u8; want];
    let mut got = 0;
    while got < want {
        match file.read_at(&mut buf[got..], position + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(Error::Failed(e.to_string())),
        }
    }
    buf.truncate(got);
    Ok(buf)
}

/// Guest file lists the host can read: the current one and the ones frozen
/// by `LockClipData`.
#[derive(Default)]
pub struct GuestFiles {
    /// Guest clipboard serial and its list, built on the first request.
    current: Option<(u32, Arc<Vec<Entry>>)>,
    /// Serial of the guest copy on the clipboard now, list built or not.
    serial: Option<u32>,
    locked: HashMap<u32, Locked>,
}

enum Locked {
    /// Taken before the list of the guest copy `serial` was built: bound
    /// to that copy's list only, never to a later one.
    Waiting {
        serial: Option<u32>,
    },
    List(Arc<Vec<Entry>>),
}

/// More locks than this is a host that forgets to unlock.
const MAX_LOCKS: usize = 64;

impl GuestFiles {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The list for guest clipboard `serial`, if already built.
    pub fn cached(&self, serial: u32) -> Option<Arc<Vec<Entry>>> {
        self.current
            .as_ref()
            .filter(|(s, _)| *s == serial)
            .map(|(_, e)| e.clone())
    }

    pub fn set_current(&mut self, serial: u32, entries: Arc<Vec<Entry>>) {
        // Locks taken on this copy before its list was built (MS-RDPECLIP:
        // a lock freezes the content of the moment it is taken).
        for lock in self.locked.values_mut() {
            if let Locked::Waiting { serial: s } = lock
                && s.is_none_or(|s| s == serial)
            {
                *lock = Locked::List(entries.clone());
            }
        }
        self.current = Some((serial, entries));
    }

    /// A new guest copy: the old list is not the current content anymore.
    pub fn clipboard_changed(&mut self, serial: u32) {
        self.current = None;
        self.serial = Some(serial);
    }

    pub fn lock(&mut self, id: u32) -> Result<(), Error> {
        if self.locked.len() >= MAX_LOCKS && !self.locked.contains_key(&id) {
            return Err(Error::Failed("too many clipboard locks".into()));
        }
        let lock = match &self.current {
            Some((s, e)) if self.serial.is_none_or(|serial| serial == *s) => {
                Locked::List(e.clone())
            }
            _ => Locked::Waiting {
                serial: self.serial,
            },
        };
        self.locked.insert(id, lock);
        Ok(())
    }

    pub fn unlock(&mut self, id: u32) {
        self.locked.remove(&id);
    }

    pub fn entries(&self, clip_data_id: Option<u32>) -> Result<Arc<Vec<Entry>>, Error> {
        match clip_data_id {
            Some(id) => match self.locked.get(&id) {
                Some(Locked::List(e)) => Ok(e.clone()),
                Some(Locked::Waiting { .. }) => Err(Error::NoData(format!(
                    "file list of lock {id} not requested yet"
                ))),
                None => Err(Error::NotFound(format!("clipboard lock {id}"))),
            },
            None => self
                .current
                .as_ref()
                .map(|(_, e)| e.clone())
                .ok_or_else(|| Error::NoData("no file list requested".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool, size: u64) -> Entry {
        Entry {
            name: name.into(),
            attributes: if dir {
                FILE_ATTRIBUTE_DIRECTORY
            } else {
                FILE_ATTRIBUTE_ARCHIVE
            },
            size,
            write_time: 0x01DB_0000_1234_5678,
            path: PathBuf::new(),
        }
    }

    #[test]
    fn descriptor_roundtrip() {
        let list = vec![
            entry("dir", true, 0),
            entry("dir\\é.txt", false, 5_000_000_000),
            entry("b.bin", false, 3),
        ];
        let blob = encode(&list);
        assert_eq!(blob.len(), 4 + 3 * 592);
        let d = &blob[4 + 592..4 + 2 * 592];
        assert_eq!(u32::from_le_bytes(d[64..68].try_into().unwrap()), 1);
        assert_eq!(decode(&blob).unwrap(), list);
    }

    #[test]
    fn host_names_stay_relative() {
        for bad in [
            "..\\x",
            "a\\..\\..\\x",
            "\\x",
            "C:\\x",
            "a\\\\b",
            "a/b",
            "a\\.\\b",
        ] {
            assert!(decode(&encode(&[entry(bad, false, 1)])).is_err(), "{bad}");
        }
        assert!(decode(&encode(&[entry("a\\b c\\d.txt", false, 1)])).is_ok());
        assert!(decode(&[1, 0, 0, 0, 0]).is_err());
    }

    #[test]
    fn uri_lists() {
        let list = b"# comment\r\nfile:///home/a/T%C3%A9l%C3%A9chargements/x%20y.pdf\r\n\
            file://localhost/tmp/b\r\nhttps://example.com/c\r\nfile://other/d\r\n";
        assert_eq!(
            parse_uri_list(list),
            [
                PathBuf::from("/home/a/Téléchargements/x y.pdf"),
                PathBuf::from("/tmp/b")
            ]
        );
        // Nautilus: the action, then the URIs.
        assert_eq!(
            parse_uri_list(b"copy\nfile:///a%2"),
            [PathBuf::from("/a%2")]
        );
        let p = Path::new("/run/user/1000/x y/é");
        assert_eq!(parse_uri_list(file_uri(p).as_bytes()), [p.to_path_buf()]);
    }

    #[test]
    fn filetimes() {
        let e = Entry {
            write_time: UNIX_EPOCH_FILETIME + 15 * 10_000_000,
            ..entry("a", false, 0)
        };
        assert_eq!(
            e.mtime(),
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(15)
        );
    }

    #[test]
    fn locks_before_the_list() {
        let mut g = GuestFiles::default();
        g.clipboard_changed(1);
        g.lock(7).unwrap();
        assert!(matches!(g.entries(Some(7)), Err(Error::NoData(_))));
        let list = Arc::new(vec![entry("a", false, 1)]);
        g.set_current(1, list.clone());
        assert_eq!(g.entries(Some(7)).unwrap(), list);
        g.clipboard_changed(2);
        g.set_current(2, Arc::new(vec![]));
        assert_eq!(g.entries(Some(7)).unwrap(), list);
        assert!(g.entries(None).unwrap().is_empty());
        g.unlock(7);
        assert!(matches!(g.entries(Some(7)), Err(Error::NotFound(_))));
    }

    #[test]
    fn walk_and_read() {
        let dir = std::env::temp_dir().join(format!("ada-walk-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("f.txt"), b"hello world").unwrap();
        std::fs::write(dir.join(".hidden"), b"").unwrap();
        // A link out of the tree is not followed, a pipe is left out.
        std::os::unix::fs::symlink("/", dir.join("root")).unwrap();
        rustix::fs::mknodat(
            rustix::fs::CWD,
            dir.join("pipe"),
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o600),
            0,
        )
        .unwrap();
        let out = walk(std::slice::from_ref(&dir)).unwrap();
        let top = dir.file_name().unwrap().to_string_lossy().into_owned();
        let names: Vec<_> = out.iter().map(|e| e.name.clone()).collect();
        assert_eq!(
            names,
            [
                top.clone(),
                format!("{top}\\.hidden"),
                format!("{top}\\sub"),
                format!("{top}\\sub\\f.txt")
            ]
        );
        assert_eq!(
            out[1].attributes & FILE_ATTRIBUTE_HIDDEN,
            FILE_ATTRIBUTE_HIDDEN
        );
        assert_eq!(out[3].size, 11);

        let open = OpenFile::default();
        assert_eq!(
            contents(&out, &open, 3, SIZE, 0, 0).unwrap(),
            11u64.to_le_bytes()
        );
        assert_eq!(contents(&out, &open, 3, RANGE, 6, 100).unwrap(), b"world");
        assert!(contents(&out, &open, 3, RANGE, 11, 5).unwrap().is_empty());
        assert!(contents(&out, &open, 2, RANGE, 0, 5).is_err(), "directory");
        drop(open);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
