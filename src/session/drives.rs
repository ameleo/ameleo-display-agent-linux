// SPDX-License-Identifier: GPL-3.0-or-later

//! Client drives (as RDP drive redirection has them), through FUSE.
//!
//! Each client drive is a FUSE file system of the session agent, mounted in
//! `~/client-drives/<label>`. Every operation of a guest app becomes the
//! matching call of `Host.Drive1`, through the daemon, which the host
//! forwards to its client: nothing of the client's files is kept in the
//! guest. The mount belongs to the user, so other users cannot see it.
//!
//! The client is Windows-like: it has no inode numbers, and RDPDR gives one
//! directory entry per round trip. So the file system numbers paths itself,
//! and trusts what the client said of a file or a folder listing for a few
//! seconds ([`ENTRY_TTL`]), as the Windows agent does. Changes made on the
//! client (`Changed`) and through the mount forget what was known.
//!
//! The FUSE thread only hands requests to the agent's runtime: the host
//! calls of several requests run at the same time.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs,
    ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use proto::drive::{Entry, IO_MAX, action, information};
use rustix::fs::OFlags;

use crate::ipc::HostDriveProxy;

/// What the kernel may keep of names and attributes.
const TTL: Duration = Duration::from_secs(1);
/// How long what the client said of a file (listing, query) is trusted:
/// changes made on the client side show after that at the latest.
const ENTRY_TTL: Duration = Duration::from_secs(5);
/// How long the client volume's size is trusted.
const VOLUME_TTL: Duration = Duration::from_secs(10);
/// The FUSE subtype of the mounts, to find the ones a killed agent left.
const SUBTYPE: &str = "ameleo-drive";
/// Where the drives are mounted, in the home directory.
const DIR: &str = "client-drives";

const STATUS_SUCCESS: u32 = 0;
const STATUS_END_OF_FILE: u32 = 0xC000_0011;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const DELETE: u32 = 0x1_0000;
const SYNCHRONIZE: u32 = 0x10_0000;
const FILE_LIST_DIRECTORY: u32 = 0x1;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
/// Read, write and delete: POSIX has no sharing modes.
const SHARE_ALL: u32 = 0x7;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_OPEN_IF: u32 = 3;
const FILE_OVERWRITE: u32 = 4;
const FILE_OVERWRITE_IF: u32 = 5;
const FILE_DIRECTORY_FILE: u32 = 0x1;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x20;
const FILE_NON_DIRECTORY_FILE: u32 = 0x40;
const FILE_ATTRIBUTE_READONLY: u32 = 0x1;
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
const FILE_ATTRIBUTE_ARCHIVE: u32 = 0x20;
const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;

/// 100 ns intervals from 1601 to 1970.
const FILETIME_UNIX: u64 = 116_444_736_000_000_000;

fn errno(e: rustix::io::Errno) -> Errno {
    Errno::from_i32(e.raw_os_error())
}

/// The errno of a client's `NTSTATUS`.
fn status_errno(status: u32) -> Errno {
    use rustix::io::Errno as E;
    errno(match status {
        // Not found, path not found, no such file, deleted, delete pending.
        0xC000_0034 | 0xC000_003A | 0xC000_000F | 0xC000_0123 | 0xC000_0056 => E::NOENT,
        0xC000_0022 => E::ACCESS,
        // Sharing violation, lock conflict.
        0xC000_0043 | 0xC000_0054 => E::BUSY,
        0xC000_0035 => E::EXIST,
        0xC000_00BA => E::ISDIR,
        0xC000_0103 => E::NOTDIR,
        0xC000_0101 => E::NOTEMPTY,
        0xC000_007F => E::NOSPC,
        0xC000_00A2 => E::ROFS,
        0xC000_00D4 => E::XDEV,
        0xC000_0120 => E::INTR,
        // Invalid name, invalid parameter.
        0xC000_0033 | 0xC000_000D => E::INVAL,
        // Not supported, not implemented, invalid device request.
        0xC000_00BB | 0xC000_0002 | 0xC000_0010 => E::OPNOTSUPP,
        _ => E::IO,
    })
}

/// `Ok` for `STATUS_SUCCESS`, the errno otherwise.
fn check(status: u32) -> Result<(), Errno> {
    if status == STATUS_SUCCESS {
        Ok(())
    } else {
        Err(status_errno(status))
    }
}

fn time(filetime: u64) -> SystemTime {
    let t = filetime.saturating_sub(FILETIME_UNIX);
    UNIX_EPOCH + Duration::from_nanos(t.saturating_mul(100))
}

fn filetime(t: SystemTime) -> u64 {
    let since = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    FILETIME_UNIX + (since.as_nanos() / 100) as u64
}

/// `dir` of `dir\name` (`""`: the root).
fn parent(path: &str) -> &str {
    path.rsplit_once('\\').map_or("", |p| p.0)
}

/// `dir\name`; `None` for a name the client cannot take.
fn join(dir: &str, name: &OsStr) -> Option<String> {
    let name = name.to_str()?;
    if name.is_empty() || name.contains('\\') {
        return None;
    }
    Some(if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}\\{name}")
    })
}

fn is_dir(e: &Entry) -> bool {
    e.1 & FILE_ATTRIBUTE_DIRECTORY != 0
}

/// The paths the kernel knows, by inode number. The client has no inode
/// numbers: a path keeps its number while the kernel holds it.
struct Inodes {
    /// `(path, lookups)` by inode.
    by_ino: HashMap<u64, (String, u64)>,
    by_path: HashMap<String, u64>,
    next: u64,
}

impl Inodes {
    fn new() -> Self {
        let mut s = Self {
            by_ino: HashMap::new(),
            by_path: HashMap::new(),
            next: INodeNo::ROOT.0 + 1,
        };
        s.by_ino.insert(INodeNo::ROOT.0, (String::new(), 1));
        s.by_path.insert(String::new(), INodeNo::ROOT.0);
        s
    }

    fn path(&self, ino: u64) -> Option<String> {
        self.by_ino.get(&ino).map(|n| n.0.clone())
    }

    /// The inode of `path`, made if needed; `lookup`: the kernel holds it
    /// now (a lookup or create reply).
    fn ino(&mut self, path: &str, lookup: bool) -> u64 {
        let ino = match self.by_path.get(path) {
            Some(&ino) => ino,
            None => {
                if self.by_ino.len() > 100_000 {
                    // Numbers only readdir gave: the kernel does not hold
                    // them.
                    self.by_ino.retain(|&i, n| n.1 > 0 || i == INodeNo::ROOT.0);
                    let by_ino = &self.by_ino;
                    self.by_path.retain(|_, i| by_ino.contains_key(i));
                }
                let ino = self.next;
                self.next += 1;
                self.by_ino.insert(ino, (path.to_string(), 0));
                self.by_path.insert(path.to_string(), ino);
                ino
            }
        };
        if lookup && let Some(n) = self.by_ino.get_mut(&ino) {
            n.1 += 1;
        }
        ino
    }

    fn forget(&mut self, ino: u64, n: u64) {
        if ino == INodeNo::ROOT.0 {
            return;
        }
        let Some(node) = self.by_ino.get_mut(&ino) else {
            return;
        };
        node.1 = node.1.saturating_sub(n);
        if node.1 == 0 {
            let path = self.by_ino.remove(&ino).unwrap().0;
            if self.by_path.get(&path) == Some(&ino) {
                self.by_path.remove(&path);
            }
        }
    }

    /// `path` is gone: a new file there gets a new number.
    fn unlinked(&mut self, path: &str) {
        self.by_path.remove(path);
    }

    /// `old` (and what it holds) is now `new`.
    fn renamed(&mut self, old: &str, new: &str) {
        self.unlinked(new);
        let prefix = format!("{old}\\");
        let moved: Vec<(String, u64)> = self
            .by_path
            .iter()
            .filter(|(p, _)| *p == old || p.starts_with(&prefix))
            .map(|(p, &i)| (p.clone(), i))
            .collect();
        for (p, ino) in moved {
            self.by_path.remove(&p);
            let to = format!("{new}{}", &p[old.len()..]);
            if let Some(n) = self.by_ino.get_mut(&ino) {
                n.0 = to.clone();
            }
            self.by_path.insert(to, ino);
        }
    }
}

/// A file open through the mount.
struct Open {
    /// The client's file id.
    file: u32,
    ino: u64,
    append: bool,
    /// Written through this open.
    modified: AtomicBool,
}

/// A folder listing and when the client gave it.
type Listing = (Arc<Vec<Entry>>, Instant);

/// One client drive.
struct DriveFs {
    id: u32,
    host: HostDriveProxy<'static>,
    runtime: tokio::runtime::Handle,
    uid: u32,
    gid: u32,
    inodes: Mutex<Inodes>,
    /// What the client said lately of files, by lowercase path (the client
    /// ignores case).
    entries: Mutex<HashMap<String, (Entry, Instant)>>,
    /// Folder listings the client gave lately, by lowercase path.
    listings: Mutex<HashMap<String, Listing>>,
    /// `(when, total, free)`.
    volume: Mutex<Option<(Instant, u64, u64)>>,
    files: Mutex<HashMap<u64, Arc<Open>>>,
    /// Open folders: their listing, taken at `opendir`.
    dirs: Mutex<HashMap<u64, Arc<Vec<Entry>>>>,
    next_fh: AtomicU64,
}

impl DriveFs {
    /// A D-Bus error: the client could not be asked.
    fn lost<T>(&self, what: &str, r: zbus::Result<T>) -> Result<T, Errno> {
        r.map_err(|e| {
            tracing::warn!(drive = self.id, "client drive {what}: {e}");
            errno(rustix::io::Errno::IO)
        })
    }

    fn known(&self, path: &str) -> Option<Entry> {
        if path.is_empty() {
            return Some((String::new(), FILE_ATTRIBUTE_DIRECTORY, 0, 0, 0, 0, 0));
        }
        self.entries
            .lock()
            .unwrap()
            .get(&path.to_lowercase())
            .filter(|(_, at)| at.elapsed() < ENTRY_TTL)
            .map(|(e, _)| e.clone())
    }

    fn remember(&self, path: &str, entry: Entry) {
        let mut cache = self.entries.lock().unwrap();
        if cache.len() > 10_000 {
            cache.retain(|_, (_, at)| at.elapsed() < ENTRY_TTL);
        }
        cache.insert(path.to_lowercase(), (entry, Instant::now()));
    }

    /// `path` changed: what we knew of it, of its folder's listing and of
    /// its own listing is stale.
    fn forget(&self, path: &str) {
        let key = path.to_lowercase();
        self.entries.lock().unwrap().remove(&key);
        let mut listings = self.listings.lock().unwrap();
        listings.remove(&key);
        listings.remove(parent(&key));
    }

    fn listing(&self, dir: &str) -> Option<Arc<Vec<Entry>>> {
        self.listings
            .lock()
            .unwrap()
            .get(&dir.to_lowercase())
            .filter(|(_, at)| at.elapsed() < ENTRY_TTL)
            .map(|(l, _)| l.clone())
    }

    /// `path` is not in the fresh listing of its folder.
    fn absent(&self, path: &str) -> bool {
        !path.is_empty() && self.listing(parent(path)).is_some() && self.known(path).is_none()
    }

    async fn list(&self, dir: &str) -> Result<Arc<Vec<Entry>>, Errno> {
        if let Some(l) = self.listing(dir) {
            return Ok(l);
        }
        let (st, entries) = self.lost("list", self.host.list(self.id, dir).await)?;
        check(st)?;
        // Stamped before the entries: they outlive it, so a name missing
        // from a fresh listing is missing (see `absent`).
        let at = Instant::now();
        let entries: Vec<Entry> = entries
            .into_iter()
            .filter(|e| {
                !(e.0.is_empty() || e.0 == "." || e.0 == ".." || e.0.contains(['\\', '/', '\0']))
            })
            .collect();
        for e in &entries {
            if let Some(p) = join(dir, OsStr::new(&e.0)) {
                self.remember(&p, e.clone());
            }
        }
        let entries = Arc::new(entries);
        let mut listings = self.listings.lock().unwrap();
        if listings.len() > 1_000 {
            listings.retain(|_, (_, at)| at.elapsed() < ENTRY_TTL);
        }
        listings.insert(dir.to_lowercase(), (entries.clone(), at));
        Ok(entries)
    }

    /// Opens `path` on the client: `(file id, information)`.
    async fn open(
        &self,
        path: &str,
        access: u32,
        attributes: u32,
        disposition: u32,
        options: u32,
    ) -> Result<(u32, u32), Errno> {
        let r = self
            .host
            .open(
                self.id,
                path,
                access,
                attributes,
                SHARE_ALL,
                disposition,
                options | FILE_SYNCHRONOUS_IO_NONALERT,
            )
            .await;
        let (st, file, info) = self.lost("open", r)?;
        if st != STATUS_SUCCESS {
            tracing::debug!(
                drive = self.id,
                path,
                status = format!("{st:#x}"),
                access = format!("{access:#x}"),
                disposition,
                "client drive open refused"
            );
            return Err(status_errno(st));
        }
        if info != information::OPENED {
            self.forget(path); // created, overwritten or superseded
        }
        Ok((file, info))
    }

    async fn close(&self, file: u32) {
        match self.host.close(self.id, file).await {
            Ok(STATUS_SUCCESS) => {}
            Ok(st) => tracing::debug!(drive = self.id, status = format!("{st:#x}"), "close"),
            Err(e) => tracing::debug!(drive = self.id, "close: {e}"),
        }
    }

    async fn query(&self, file: u32) -> Result<Entry, Errno> {
        let (st, e) = self.lost("query", self.host.query(self.id, file).await)?;
        check(st)?;
        Ok(e)
    }

    /// What the client says of `path`: from what it said lately, or asked.
    async fn entry(&self, path: &str) -> Result<Entry, Errno> {
        if let Some(e) = self.known(path) {
            return Ok(e);
        }
        if self.absent(path) {
            return Err(errno(rustix::io::Errno::NOENT));
        }
        let (file, _) = self
            .open(path, FILE_READ_ATTRIBUTES | SYNCHRONIZE, 0, FILE_OPEN, 0)
            .await?;
        let e = self.query(file).await;
        self.close(file).await;
        let e = e?;
        self.remember(path, e.clone());
        Ok(e)
    }

    /// Runs `f` on `path` opened for `access`, then closes it.
    async fn with_open<T, F, Fut>(
        &self,
        path: &str,
        access: u32,
        options: u32,
        f: F,
    ) -> Result<T, Errno>
    where
        F: FnOnce(u32) -> Fut,
        Fut: Future<Output = Result<T, Errno>>,
    {
        let (file, _) = self
            .open(path, access | SYNCHRONIZE, 0, FILE_OPEN, options)
            .await?;
        let r = f(file).await;
        self.close(file).await;
        r
    }

    async fn volume(&self) -> Result<(u64, u64), Errno> {
        if let Some((at, total, free)) = *self.volume.lock().unwrap()
            && at.elapsed() < VOLUME_TTL
        {
            return Ok((total, free));
        }
        let (st, total, free, _, _) = self.lost("volume", self.host.volume(self.id).await)?;
        check(st)?;
        *self.volume.lock().unwrap() = Some((Instant::now(), total, free));
        Ok((total, free))
    }

    fn attr(&self, ino: u64, e: &Entry) -> FileAttr {
        let dir = is_dir(e);
        let size = if dir { 0 } else { e.2 };
        let mtime = time(e.5);
        FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: time(e.4),
            mtime,
            ctime: if e.6 == 0 { mtime } else { time(e.6) },
            crtime: time(e.3),
            kind: if dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: match (dir, e.1 & FILE_ATTRIBUTE_READONLY != 0) {
                (true, _) => 0o755,
                (false, true) => 0o444,
                (false, false) => 0o644,
            },
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 64 * 1024,
            flags: 0,
        }
    }

    fn path(&self, ino: INodeNo) -> Result<String, Errno> {
        self.inodes
            .lock()
            .unwrap()
            .path(ino.0)
            .ok_or(errno(rustix::io::Errno::STALE))
    }

    fn child(&self, parent: INodeNo, name: &OsStr) -> Result<String, Errno> {
        let dir = self.path(parent)?;
        join(&dir, name).ok_or(errno(rustix::io::Errno::INVAL))
    }

    fn opened(&self, file: u32, ino: u64, append: bool) -> FileHandle {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.files.lock().unwrap().insert(
            fh,
            Arc::new(Open {
                file,
                ino,
                append,
                modified: AtomicBool::new(false),
            }),
        );
        FileHandle(fh)
    }

    fn file(&self, fh: FileHandle) -> Result<Arc<Open>, Errno> {
        self.files
            .lock()
            .unwrap()
            .get(&fh.0)
            .cloned()
            .ok_or(errno(rustix::io::Errno::BADF))
    }

    /// The file of `ino` changes: what we knew of it is stale.
    fn modified(&self, ino: u64) {
        if let Some(path) = self.inodes.lock().unwrap().path(ino) {
            self.forget(&path);
        }
    }

    /// The attributes after a change: asked again.
    async fn fresh_attr(&self, ino: u64, path: &str) -> Result<FileAttr, Errno> {
        self.forget(path);
        let e = self.entry(path).await?;
        Ok(self.attr(ino, &e))
    }

    async fn setattr(
        &self,
        ino: u64,
        mode: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        fh: Option<FileHandle>,
    ) -> Result<FileAttr, Errno> {
        let path = self.path(INodeNo(ino))?;
        let open = fh.and_then(|fh| self.file(fh).ok());
        if let Some(size) = size {
            self.forget(&path);
            let set = |file| async move {
                let r = self.host.set_size(self.id, file, size, false).await;
                check(self.lost("set size", r)?)
            };
            match &open {
                Some(o) => {
                    o.modified.store(true, Ordering::Relaxed);
                    set(o.file).await?
                }
                None => {
                    self.with_open(&path, GENERIC_WRITE, FILE_NON_DIRECTORY_FILE, set)
                        .await?
                }
            }
        }
        if atime.is_some() || mtime.is_some() {
            let t = |t: Option<TimeOrNow>| match t {
                None => 0,
                Some(TimeOrNow::Now) => filetime(SystemTime::now()),
                Some(TimeOrNow::SpecificTime(t)) => filetime(t),
            };
            let (a, m) = (t(atime), t(mtime));
            self.forget(&path);
            self.with_open(&path, FILE_WRITE_ATTRIBUTES, 0, |file| async move {
                let r = self.host.set_basic(self.id, file, 0, 0, a, m).await;
                check(self.lost("set times", r)?)
            })
            .await?;
        }
        if let Some(mode) = mode {
            // The owner's write permission is the client's read-only
            // attribute; folders have none that means anything.
            let e = self.entry(&path).await?;
            let readonly = mode & 0o200 == 0;
            if !is_dir(&e) && readonly != (e.1 & FILE_ATTRIBUTE_READONLY != 0) {
                let mut attributes = e.1 & !FILE_ATTRIBUTE_READONLY;
                if readonly {
                    attributes |= FILE_ATTRIBUTE_READONLY;
                }
                if attributes == 0 {
                    attributes = FILE_ATTRIBUTE_NORMAL;
                }
                self.forget(&path);
                self.with_open(&path, FILE_WRITE_ATTRIBUTES, 0, |file| async move {
                    let r = self
                        .host
                        .set_basic(self.id, file, attributes, 0, 0, 0)
                        .await;
                    check(self.lost("set attributes", r)?)
                })
                .await?;
            }
        }
        match &open {
            Some(o) => {
                let e = self.query(o.file).await?;
                Ok(self.attr(ino, &e))
            }
            None => self.fresh_attr(ino, &path).await,
        }
    }

    async fn read(&self, fh: FileHandle, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let open = self.file(fh)?;
        let mut data = Vec::with_capacity(size as usize);
        while (data.len() as u32) < size {
            let n = (size - data.len() as u32).min(IO_MAX);
            let at = offset + data.len() as u64;
            let (st, chunk) = self.lost("read", self.host.read(self.id, open.file, at, n).await)?;
            if st == STATUS_END_OF_FILE {
                break;
            }
            if st != STATUS_SUCCESS {
                if !data.is_empty() {
                    break; // what was read is the answer
                }
                return Err(status_errno(st));
            }
            let got = chunk.len().min(n as usize);
            data.extend_from_slice(&chunk[..got]);
            if (got as u32) < n {
                break; // end of file
            }
        }
        Ok(data)
    }

    async fn write(&self, fh: FileHandle, offset: u64, data: Vec<u8>) -> Result<u32, Errno> {
        let open = self.file(fh)?;
        open.modified.store(true, Ordering::Relaxed);
        self.modified(open.ino);
        let offset = if open.append {
            self.query(open.file).await?.2
        } else {
            offset
        };
        let mut done = 0usize;
        while done < data.len() {
            let n = (data.len() - done).min(IO_MAX as usize);
            let chunk = data[done..done + n].to_vec();
            let at = offset + done as u64;
            let r = self.host.write(self.id, open.file, at, chunk).await;
            let (st, count) = self.lost("write", r)?;
            if st != STATUS_SUCCESS {
                if done > 0 {
                    break;
                }
                return Err(status_errno(st));
            }
            let count = (count as usize).min(n);
            done += count;
            if count < n {
                break;
            }
        }
        Ok(done as u32)
    }

    /// `unlink` and `rmdir`: marked deleted on the client, which deletes it
    /// when closed.
    async fn delete(&self, path: &str, dir: bool) -> Result<(), Errno> {
        let options = if dir {
            FILE_DIRECTORY_FILE
        } else {
            FILE_NON_DIRECTORY_FILE
        };
        self.forget(path);
        self.with_open(
            path,
            DELETE | FILE_READ_ATTRIBUTES,
            options,
            |file| async move {
                let r = self.host.set_delete(self.id, file, true).await;
                check(self.lost("delete", r)?)
            },
        )
        .await?;
        self.forget(path);
        self.inodes.lock().unwrap().unlinked(path);
        Ok(())
    }

    async fn rename(&self, old: &str, new: &str, replace: bool) -> Result<(), Errno> {
        self.forget(old);
        self.forget(new);
        let new_path = new.to_string();
        self.with_open(old, DELETE | FILE_READ_ATTRIBUTES, 0, |file| async move {
            let r = self.host.rename(self.id, file, &new_path, replace).await;
            check(self.lost("rename", r)?)
        })
        .await?;
        self.forget(old);
        self.forget(new);
        self.inodes.lock().unwrap().renamed(old, new);
        Ok(())
    }
}

/// The FUSE side of a [`DriveFs`]: each request runs on the runtime.
struct Fs(Arc<DriveFs>);

impl Fs {
    fn spawn(&self, f: impl Future<Output = ()> + Send + 'static) {
        self.0.runtime.spawn(f);
    }
}

/// The client's access of an open.
fn access(flags: OpenFlags) -> u32 {
    match flags.acc_mode() {
        OpenAccMode::O_RDONLY => GENERIC_READ,
        OpenAccMode::O_WRONLY => GENERIC_WRITE,
        OpenAccMode::O_RDWR => GENERIC_READ | GENERIC_WRITE,
    }
}

fn has(flags: i32, f: OFlags) -> bool {
    flags as u32 & f.bits() != 0
}

impl Filesystem for Fs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let fs = self.0.clone();
        let path = match fs.child(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        self.spawn(async move {
            match fs.entry(&path).await {
                Ok(e) => {
                    let ino = fs.inodes.lock().unwrap().ino(&path, true);
                    reply.entry(&TTL, &fs.attr(ino, &e), Generation(0));
                }
                Err(e) => reply.error(e),
            }
        });
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.0.inodes.lock().unwrap().forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let fs = self.0.clone();
        let path = match fs.path(ino) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        let open = fh.and_then(|fh| fs.file(fh).ok());
        self.spawn(async move {
            let e = match (fs.known(&path), open) {
                (Some(e), _) => Ok(e),
                // Open: its size may be changing.
                (None, Some(o)) => fs.query(o.file).await,
                (None, None) => fs.entry(&path).await,
            };
            match e {
                Ok(e) => reply.attr(&TTL, &fs.attr(ino.0, &e)),
                Err(e) => reply.error(e),
            }
        });
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let fs = self.0.clone();
        self.spawn(async move {
            match fs.setattr(ino.0, mode, size, atime, mtime, fh).await {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(e) => reply.error(e),
            }
        });
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let fs = self.0.clone();
        let path = match fs.path(ino) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        self.spawn(async move {
            let disposition = if has(flags.0, OFlags::TRUNC) {
                FILE_OVERWRITE
            } else {
                FILE_OPEN
            };
            let r = fs
                .open(
                    &path,
                    access(flags),
                    0,
                    disposition,
                    FILE_NON_DIRECTORY_FILE,
                )
                .await;
            match r {
                Ok((file, _)) => {
                    let fh = fs.opened(file, ino.0, has(flags.0, OFlags::APPEND));
                    reply.opened(fh, FopenFlags::empty());
                }
                Err(e) => reply.error(e),
            }
        });
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let fs = self.0.clone();
        let path = match fs.child(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        self.spawn(async move {
            let disposition = if has(flags, OFlags::EXCL) {
                FILE_CREATE
            } else if has(flags, OFlags::TRUNC) {
                FILE_OVERWRITE_IF
            } else {
                FILE_OPEN_IF
            };
            let attributes = if mode & !umask & 0o200 == 0 {
                FILE_ATTRIBUTE_READONLY
            } else {
                FILE_ATTRIBUTE_ARCHIVE
            };
            let r = fs
                .open(
                    &path,
                    access(OpenFlags(flags)),
                    attributes,
                    disposition,
                    FILE_NON_DIRECTORY_FILE,
                )
                .await;
            let (file, _) = match r {
                Ok(v) => v,
                Err(e) => return reply.error(e),
            };
            fs.forget(&path);
            match fs.query(file).await {
                Ok(e) => {
                    let ino = fs.inodes.lock().unwrap().ino(&path, true);
                    let fh = fs.opened(file, ino, has(flags, OFlags::APPEND));
                    reply.created(
                        &TTL,
                        &fs.attr(ino, &e),
                        Generation(0),
                        fh,
                        FopenFlags::empty(),
                    );
                }
                Err(e) => {
                    fs.close(file).await;
                    reply.error(e);
                }
            }
        });
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let fs = self.0.clone();
        let path = match fs.child(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        self.spawn(async move {
            let r = fs
                .open(
                    &path,
                    FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                    0,
                    FILE_CREATE,
                    FILE_DIRECTORY_FILE,
                )
                .await;
            let (file, _) = match r {
                Ok(v) => v,
                Err(e) => return reply.error(e),
            };
            let e = fs.query(file).await;
            fs.close(file).await;
            fs.forget(&path);
            match e {
                Ok(e) => {
                    let ino = fs.inodes.lock().unwrap().ino(&path, true);
                    reply.entry(&TTL, &fs.attr(ino, &e), Generation(0));
                }
                Err(e) => reply.error(e),
            }
        });
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let fs = self.0.clone();
        let path = match fs.child(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        self.spawn(async move {
            match fs.delete(&path, false).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        });
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let fs = self.0.clone();
        let path = match fs.child(parent, name) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        self.spawn(async move {
            match fs.delete(&path, true).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        });
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        if flags.intersects(RenameFlags::RENAME_EXCHANGE | RenameFlags::RENAME_WHITEOUT) {
            return reply.error(errno(rustix::io::Errno::INVAL));
        }
        let fs = self.0.clone();
        let (old, new) = match (fs.child(parent, name), fs.child(newparent, newname)) {
            (Ok(o), Ok(n)) => (o, n),
            (Err(e), _) | (_, Err(e)) => return reply.error(e),
        };
        let replace = !flags.contains(RenameFlags::RENAME_NOREPLACE);
        self.spawn(async move {
            match fs.rename(&old, &new, replace).await {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let fs = self.0.clone();
        self.spawn(async move {
            match fs.read(fh, offset, size).await {
                Ok(data) => reply.data(&data),
                Err(e) => reply.error(e),
            }
        });
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let fs = self.0.clone();
        let data = data.to_vec();
        self.spawn(async move {
            match fs.write(fh, offset, data).await {
                Ok(n) => reply.written(n),
                Err(e) => reply.error(e),
            }
        });
    }

    /// Each `close` of a descriptor: the writes are already on the client.
    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let fs = self.0.clone();
        self.spawn(async move {
            let r = match fs.file(fh) {
                Ok(o) => {
                    let r = fs.host.flush(fs.id, o.file).await;
                    fs.lost("flush", r).and_then(check)
                }
                Err(e) => Err(e),
            };
            match r {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e),
            }
        });
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let fs = self.0.clone();
        let Some(open) = fs.files.lock().unwrap().remove(&fh.0) else {
            return reply.ok();
        };
        self.spawn(async move {
            fs.close(open.file).await;
            // The client applies some changes at close (times, size).
            if open.modified.load(Ordering::Relaxed) {
                fs.modified(open.ino);
            }
            reply.ok();
        });
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let fs = self.0.clone();
        let path = match fs.path(ino) {
            Ok(p) => p,
            Err(e) => return reply.error(e),
        };
        self.spawn(async move {
            match fs.list(&path).await {
                Ok(entries) => {
                    let fh = fs.next_fh.fetch_add(1, Ordering::Relaxed);
                    fs.dirs.lock().unwrap().insert(fh, entries);
                    reply.opened(FileHandle(fh), FopenFlags::empty());
                }
                Err(e) => reply.error(e),
            }
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let fs = &self.0;
        let Some(entries) = fs.dirs.lock().unwrap().get(&fh.0).cloned() else {
            return reply.error(errno(rustix::io::Errno::BADF));
        };
        let Ok(dir) = fs.path(ino) else {
            return reply.error(errno(rustix::io::Errno::STALE));
        };
        let mut inodes = fs.inodes.lock().unwrap();
        let up = inodes.ino(parent(&dir), false);
        let mut list: Vec<(u64, FileType, OsString)> = vec![
            (ino.0, FileType::Directory, ".".into()),
            (
                if dir.is_empty() { ino.0 } else { up },
                FileType::Directory,
                "..".into(),
            ),
        ];
        for e in entries.iter() {
            let Some(path) = join(&dir, OsStr::new(&e.0)) else {
                continue;
            };
            let kind = if is_dir(e) {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            list.push((inodes.ino(&path, false), kind, e.0.clone().into()));
        }
        drop(inodes);
        for (i, (ino, kind, name)) in list.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.0.dirs.lock().unwrap().remove(&fh.0);
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let fs = self.0.clone();
        self.spawn(async move {
            // A volume the client cannot measure shows as empty.
            let (total, free) = fs.volume().await.unwrap_or((0, 0));
            const BLOCK: u64 = 4096;
            reply.statfs(
                total / BLOCK,
                free / BLOCK,
                free / BLOCK,
                0,
                0,
                BLOCK as u32,
                255,
                BLOCK as u32,
            );
        });
    }

    /// The client decides at open: everything may be tried.
    fn access(&self, _req: &Request, _ino: INodeNo, _mask: fuser::AccessFlags, reply: ReplyEmpty) {
        reply.ok();
    }

    /// No extended attributes, no ACLs.
    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(Errno::NO_XATTR);
    }

    fn listxattr(&self, _req: &Request, _ino: INodeNo, size: u32, reply: ReplyXattr) {
        if size == 0 {
            reply.size(0);
        } else {
            reply.data(&[]);
        }
    }
}

struct Mount {
    path: PathBuf,
    fs: Arc<DriveFs>,
    session: BackgroundSession,
}

/// The client drives of the session.
pub struct Drives {
    /// Where they are mounted.
    root: PathBuf,
    /// KDE's places file, where each drive gets a bookmark.
    places: Option<PathBuf>,
    mounts: Mutex<HashMap<u32, Mount>>,
}

/// `~/client-drives`, or `AMELEO_DISPLAY_AGENT_DRIVES`.
pub fn default_root() -> Option<PathBuf> {
    std::env::var_os("AMELEO_DISPLAY_AGENT_DRIVES")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(DIR)))
}

/// A directory name from the drive's label.
fn dir_name(id: u32, name: &str, label: &str) -> String {
    let clean = |s: &str| {
        let s: String = s
            .chars()
            .map(|c| if c == '/' || c.is_control() { '-' } else { c })
            .take(100)
            .collect();
        s.trim().trim_start_matches('.').trim().to_string()
    };
    [clean(label), clean(name)]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| format!("drive {id}"))
}

/// The mount points of `root` that a previous agent left (`SUBTYPE`), from
/// `/proc/self/mountinfo`.
fn stale_mounts(root: &Path) -> Vec<PathBuf> {
    let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let unescape = |s: &str| {
        let mut out = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\'
                && i + 3 < b.len()
                && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
            {
                out.push(v);
                i += 4;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        PathBuf::from(OsString::from(String::from_utf8_lossy(&out).into_owned()))
    };
    info.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let point = unescape(left.split(' ').nth(4)?);
            let fstype = right.split(' ').next()?;
            (fstype == format!("fuse.{SUBTYPE}") && point.starts_with(root)).then_some(point)
        })
        .collect()
}

fn fusermount_lazy(path: &Path) {
    let _ = std::process::Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(path)
        .stderr(std::process::Stdio::null())
        .status();
}

impl Drives {
    /// Unmounts what a killed agent left in `root`.
    pub fn new(root: PathBuf, places: Option<PathBuf>) -> Self {
        if let Some(file) = &places {
            super::places::remove_all(file, &root);
        }
        for point in stale_mounts(&root) {
            tracing::info!("unmounting a stale client drive: {}", point.display());
            fusermount_lazy(&point);
            let _ = std::fs::remove_dir(&point);
        }
        let _ = std::fs::remove_dir(&root);
        Self {
            root,
            places,
            mounts: Mutex::new(HashMap::new()),
        }
    }

    /// Mounts client drive `id`, whose files `host` gives; returns its
    /// mount point. Blocks while mounting.
    pub fn add(
        &self,
        host: HostDriveProxy<'static>,
        id: u32,
        name: &str,
        label: &str,
    ) -> Result<PathBuf, String> {
        let mut mounts = self.mounts.lock().unwrap();
        if let Some(m) = mounts.get(&id) {
            return Ok(m.path.clone());
        }
        let base = dir_name(id, name, label);
        let path = (1..)
            .map(|n| match n {
                1 => self.root.join(&base),
                n => self.root.join(format!("{base} ({n})")),
            })
            .find(|p| {
                !mounts.values().any(|m| &m.path == p)
                    && match std::fs::read_dir(p) {
                        Ok(mut d) => d.next().is_none(),
                        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
                    }
            })
            .unwrap();
        std::fs::create_dir_all(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let fs = Arc::new(DriveFs {
            id,
            host,
            runtime: tokio::runtime::Handle::current(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            inodes: Mutex::new(Inodes::new()),
            entries: Mutex::default(),
            listings: Mutex::default(),
            volume: Mutex::default(),
            files: Mutex::default(),
            dirs: Mutex::default(),
            next_fh: AtomicU64::new(1),
        });
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::FSName("ameleo-display-agent".into()),
            MountOption::Subtype(SUBTYPE.into()),
        ];
        match fuser::spawn_mount(Fs(fs.clone()), &path, &config) {
            Ok(session) => {
                tracing::info!(id, label, "client drive mounted on {}", path.display());
                if let Some(file) = &self.places {
                    let title = path.file_name().unwrap_or_default().to_string_lossy();
                    super::places::add(file, &path, &title, id);
                }
                mounts.insert(
                    id,
                    Mount {
                        path: path.clone(),
                        fs,
                        session,
                    },
                );
                Ok(path)
            }
            Err(e) => {
                let _ = std::fs::remove_dir(&path);
                let _ = std::fs::remove_dir(&self.root);
                Err(format!("cannot mount {}: {e}", path.display()))
            }
        }
    }

    pub fn remove(&self, id: u32) {
        let m = self.mounts.lock().unwrap().remove(&id);
        if let Some(m) = m {
            if let Some(file) = &self.places {
                super::places::remove(file, &m.path);
            }
            unmount(m);
        }
    }

    /// Unmounts them all; the returned threads end when they are.
    pub fn remove_all(&self) -> Vec<std::thread::JoinHandle<()>> {
        let all: Vec<Mount> = self
            .mounts
            .lock()
            .unwrap()
            .drain()
            .map(|(_, m)| m)
            .collect();
        if let Some(file) = &self.places {
            for m in &all {
                super::places::remove(file, &m.path);
            }
        }
        all.into_iter().map(unmount).collect()
    }
}

/// Unmounts, even while an app still uses the drive: then the mount is
/// detached and its connection aborted, the app gets errors.
fn unmount(m: Mount) -> std::thread::JoinHandle<()> {
    let dev = rustix::fs::stat(&m.path).ok().map(|s| s.st_dev);
    let path = m.path.clone();
    std::thread::spawn(move || {
        if let Err(e) = m.session.umount_and_join() {
            tracing::info!("client drive busy ({e}): detached from {}", path.display());
            fusermount_lazy(&path);
            // The FUSE control file system lets the mount's owner end
            // the connection, so the file system thread stops.
            if let Some(dev) = dev {
                let minor = rustix::fs::minor(dev);
                let _ = std::fs::write(format!("/sys/fs/fuse/connections/{minor}/abort"), "1");
            }
        }
        let _ = std::fs::remove_dir(&path);
        if let Some(root) = path.parent() {
            let _ = std::fs::remove_dir(root);
        }
        tracing::info!("client drive unmounted from {}", path.display());
    })
}

impl Drives {
    /// Changes made on the client: what we knew of the paths is forgotten,
    /// and the kernel's cache of them too.
    pub fn changed(&self, id: u32, changes: &[proto::drive::Change]) {
        let mounts = self.mounts.lock().unwrap();
        let Some(m) = mounts.get(&id) else { return };
        let fs = m.fs.clone();
        let notifier = m.session.notifier();
        let mut stale = Vec::new();
        for (act, path) in changes {
            let path = path.trim_start_matches('\\').to_string();
            fs.forget(&path);
            let inodes = fs.inodes.lock().unwrap();
            let dir = parent(&path);
            let name = path.rsplit('\\').next().unwrap_or(&path).to_string();
            match *act {
                action::ADDED
                | action::REMOVED
                | action::RENAMED_OLD_NAME
                | action::RENAMED_NEW_NAME => {
                    if let Some(&p) = inodes.by_path.get(dir) {
                        stale.push((Some((p, name)), inodes.by_path.get(&path).copied()));
                    }
                }
                _ => {
                    if let Some(&i) = inodes.by_path.get(&path) {
                        stale.push((None, Some(i)));
                    }
                }
            }
        }
        // The kernel takes the notifications on its own time: not from the
        // runtime's threads.
        std::thread::spawn(move || {
            for (entry, ino) in stale {
                if let Some((parent, name)) = entry {
                    let _ = notifier.inval_entry(INodeNo(parent), OsStr::new(&name));
                }
                if let Some(ino) = ino {
                    let _ = notifier.inval_inode(INodeNo(ino), 0, 0);
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        assert_eq!(time(0), UNIX_EPOCH);
        let t = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(time(filetime(t)), t);
        assert_eq!(filetime(UNIX_EPOCH), FILETIME_UNIX);
    }

    #[test]
    fn paths() {
        assert_eq!(join("", OsStr::new("a")).unwrap(), "a");
        assert_eq!(join("a\\b", OsStr::new("c")).unwrap(), "a\\b\\c");
        assert!(join("a", OsStr::new("x\\y")).is_none());
        assert_eq!(parent("a\\b\\c"), "a\\b");
        assert_eq!(parent("a"), "");
        assert_eq!(dir_name(3, "C", "C sur PC/1"), "C sur PC-1");
        assert_eq!(dir_name(3, "C", " .. "), "C");
        assert_eq!(dir_name(3, "", ""), "drive 3");
    }

    #[test]
    fn statuses() {
        assert_eq!(status_errno(0xC000_0034), errno(rustix::io::Errno::NOENT));
        assert_eq!(
            status_errno(0xC000_0101),
            errno(rustix::io::Errno::NOTEMPTY)
        );
        assert_eq!(status_errno(0xDEAD_BEEF), errno(rustix::io::Errno::IO));
        assert!(check(0).is_ok());
    }

    #[test]
    fn inode_numbers() {
        let mut i = Inodes::new();
        let a = i.ino("a", true);
        let ab = i.ino("a\\b", true);
        let abc = i.ino("a\\b\\c", false);
        assert_eq!(i.ino("a", false), a);
        i.renamed("a", "z");
        assert_eq!(i.path(ab).unwrap(), "z\\b");
        assert_eq!(i.path(abc).unwrap(), "z\\b\\c");
        assert_eq!(i.ino("z\\b", false), ab);
        assert_ne!(i.ino("a\\b", false), ab);
        i.forget(ab, 1);
        assert!(i.path(ab).is_none());
        i.forget(INodeNo::ROOT.0, 5);
        assert_eq!(i.path(INodeNo::ROOT.0).unwrap(), "");
    }

    #[test]
    fn stale_mounts_found() {
        // This process has none: only the parsing runs.
        assert!(stale_mounts(Path::new("/nonexistent-ada")).is_empty());
    }
}
