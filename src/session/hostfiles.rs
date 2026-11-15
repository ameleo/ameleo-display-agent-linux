// SPDX-License-Identifier: GPL-3.0-only

//! Host files pasted in the guest, through a read-only FUSE file system.
//!
//! Linux file managers paste paths (`text/uri-list`), not file contents.
//! So that nothing is copied ahead into the guest, the session agent mounts
//! a file system on `$XDG_RUNTIME_DIR/ameleo-display-agent/clipboard` and
//! puts each host file list there, in a directory of its own named after
//! its clipboard lock (`LockClipData`). The pasted paths point into it, and
//! the file manager's reads go to the host as `FileContents` requests while
//! it copies. Only the user can see the file system.
//!
//! The last [`KEPT`] lists stay readable, so that a copy still running
//! survives a few new host copies; older ones are unlocked and removed.

use std::collections::{HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fuser::{
    AccessFlags, BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem,
    FopenFlags, Generation, INodeNo, LockOwner, MountOption, OpenAccMode, OpenFlags, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyXattr, Request,
};
use proto::file_contents::MAX_RANGE;

use super::filelist::Entry;

/// Host lists kept readable.
const KEPT: usize = 4;
const FILE_ATTRIBUTE_READONLY: u32 = 0x01;
const TTL: Duration = Duration::from_secs(1);

/// Reads `len` bytes at `position` of file `lindex` of the list locked as
/// `clip_data_id`; the host may give fewer.
pub type Fetch = Arc<
    dyn Fn(u32, i32, u64, u32) -> Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send>>
        + Send
        + Sync,
>;
/// Unlocks a list the guest does not need anymore.
pub type Unlock = Arc<dyn Fn(u32) + Send + Sync>;

struct Node {
    parent: u64,
    name: OsString,
    dir: bool,
    size: u64,
    mtime: SystemTime,
    /// Children, in list order.
    children: Vec<u64>,
    /// The file in the host list: `(clip_data_id, lindex)`.
    host: Option<(u32, i32)>,
    /// Read-only on the client (`FILE_ATTRIBUTE_READONLY`).
    readonly: bool,
}

struct Tree {
    nodes: HashMap<u64, Node>,
    next: u64,
    /// The lists, oldest first: `(clip_data_id, directory inode)`.
    lists: VecDeque<(u32, u64)>,
    uid: u32,
    gid: u32,
}

impl Tree {
    fn new() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            INodeNo::ROOT.0,
            Node {
                parent: INodeNo::ROOT.0,
                name: OsString::new(),
                dir: true,
                size: 0,
                mtime: SystemTime::now(),
                children: Vec::new(),
                host: None,
                readonly: false,
            },
        );
        Self {
            nodes,
            next: INodeNo::ROOT.0 + 1,
            lists: VecDeque::new(),
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
        }
    }

    fn insert(&mut self, parent: u64, node: Node) -> u64 {
        let ino = self.next;
        self.next += 1;
        self.nodes.insert(ino, node);
        if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.push(ino);
        }
        ino
    }

    fn child(&self, parent: u64, name: &OsStr) -> Option<u64> {
        self.nodes
            .get(&parent)?
            .children
            .iter()
            .copied()
            .find(|c| self.nodes.get(c).is_some_and(|n| n.name == name))
    }

    fn remove(&mut self, ino: u64) {
        if let Some(node) = self.nodes.remove(&ino) {
            for c in node.children {
                self.remove(c);
            }
        }
    }

    fn attr(&self, ino: u64) -> Option<FileAttr> {
        let n = self.nodes.get(&ino)?;
        Some(FileAttr {
            ino: INodeNo(ino),
            size: n.size,
            blocks: n.size.div_ceil(512),
            atime: n.mtime,
            mtime: n.mtime,
            ctime: n.mtime,
            crtime: n.mtime,
            kind: if n.dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            // The modes the pasted copies get: file managers keep them. The
            // mount is read-only anyway.
            perm: match (n.dir, n.readonly) {
                (true, _) => 0o755,
                (false, true) => 0o444,
                (false, false) => 0o644,
            },
            nlink: if n.dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 64 * 1024,
            flags: 0,
        })
    }

    /// Adds a host list, returns the paths of its top level items relative
    /// to the mount and the lists that went out.
    fn add_list(&mut self, clip_data_id: u32, entries: &[Entry]) -> (Vec<PathBuf>, Vec<u32>) {
        let now = SystemTime::now();
        let dir = Node {
            parent: INodeNo::ROOT.0,
            name: clip_data_id.to_string().into(),
            dir: true,
            size: 0,
            mtime: now,
            children: Vec::new(),
            host: None,
            readonly: false,
        };
        let root = self.insert(INodeNo::ROOT.0, dir);
        let mut tops = Vec::new();
        // Names are `a\b\c`, each directory before its content.
        let mut dirs: HashMap<String, u64> = HashMap::new();
        for (lindex, e) in entries.iter().enumerate() {
            let (parent_name, name) = match e.name.rsplit_once('\\') {
                Some((p, n)) => (Some(p), n),
                None => (None, e.name.as_str()),
            };
            let parent = match parent_name {
                None => root,
                Some(p) => match dirs.get(p) {
                    Some(&ino) => ino,
                    None => {
                        tracing::warn!("host file list: {} comes before its folder", e.name);
                        continue;
                    }
                },
            };
            if self.child(parent, OsStr::new(name)).is_some() {
                tracing::warn!("host file list: {} is listed twice", e.name);
                continue;
            }
            let node = Node {
                parent,
                name: name.into(),
                dir: e.is_dir(),
                size: if e.is_dir() { 0 } else { e.size },
                mtime: if e.write_time == 0 { now } else { e.mtime() },
                children: Vec::new(),
                host: (!e.is_dir()).then_some((clip_data_id, lindex as i32)),
                readonly: e.attributes & FILE_ATTRIBUTE_READONLY != 0,
            };
            let ino = self.insert(parent, node);
            if e.is_dir() {
                dirs.insert(e.name.clone(), ino);
            }
            if parent == root {
                tops.push(Path::new(&clip_data_id.to_string()).join(name));
            }
        }
        self.lists.push_back((clip_data_id, root));
        let mut gone = Vec::new();
        while self.lists.len() > KEPT {
            let (id, ino) = self.lists.pop_front().unwrap();
            self.detach(ino);
            gone.push(id);
        }
        (tops, gone)
    }

    fn detach(&mut self, ino: u64) {
        if let Some(root) = self.nodes.get_mut(&INodeNo::ROOT.0) {
            root.children.retain(|&c| c != ino);
        }
        self.remove(ino);
    }

    fn clear(&mut self) -> Vec<u32> {
        let lists: Vec<_> = self.lists.drain(..).collect();
        for &(_, ino) in &lists {
            self.detach(ino);
        }
        lists.into_iter().map(|(id, _)| id).collect()
    }
}

struct Fs {
    tree: Arc<Mutex<Tree>>,
    fetch: Fetch,
    runtime: tokio::runtime::Handle,
}

impl Filesystem for Fs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let tree = self.tree.lock().unwrap();
        match tree.child(parent.0, name).and_then(|c| tree.attr(c)) {
            Some(attr) => reply.entry(&TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.tree.lock().unwrap().attr(ino.0) {
            Some(attr) => reply.attr(&TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let tree = self.tree.lock().unwrap();
        match tree.nodes.get(&ino.0) {
            None => reply.error(Errno::ENOENT),
            Some(n) if n.dir => reply.error(Errno::EISDIR),
            Some(_) if flags.acc_mode() != OpenAccMode::O_RDONLY => reply.error(Errno::EROFS),
            Some(_) => reply.opened(FileHandle(0), FopenFlags::empty()),
        }
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let (host, file_size) = {
            let tree = self.tree.lock().unwrap();
            match tree.nodes.get(&ino.0) {
                Some(Node {
                    host: Some(h),
                    size,
                    ..
                }) => (*h, *size),
                Some(_) => return reply.error(Errno::EISDIR),
                None => return reply.error(Errno::ENOENT),
            }
        };
        let want = u64::from(size).min(file_size.saturating_sub(offset));
        if want == 0 {
            return reply.data(&[]);
        }
        let fetch = self.fetch.clone();
        // Answered from the runtime: the file system thread goes on with
        // other requests meanwhile.
        self.runtime.spawn(async move {
            let (clip_data_id, lindex) = host;
            let mut data = Vec::with_capacity(want as usize);
            while (data.len() as u64) < want {
                let len = (want - data.len() as u64).min(u64::from(MAX_RANGE)) as u32;
                match fetch(clip_data_id, lindex, offset + data.len() as u64, len).await {
                    Ok(chunk) if chunk.is_empty() => break, // end of the file
                    Ok(chunk) => data.extend_from_slice(&chunk[..chunk.len().min(len as usize)]),
                    Err(e) => {
                        tracing::warn!(clip_data_id, lindex, offset, "host file read failed: {e}");
                        return reply.error(Errno::EIO);
                    }
                }
            }
            reply.data(&data);
        });
    }

    /// Everything can be read, nothing written.
    fn access(&self, _req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        if !self.tree.lock().unwrap().nodes.contains_key(&ino.0) {
            reply.error(Errno::ENOENT);
        } else if mask.contains(AccessFlags::W_OK) {
            reply.error(Errno::EROFS);
        } else {
            reply.ok();
        }
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

    /// Nothing to write back: read only.
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

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let tree = self.tree.lock().unwrap();
        let Some(node) = tree.nodes.get(&ino.0) else {
            return reply.error(Errno::ENOENT);
        };
        if !node.dir {
            return reply.error(Errno::ENOTDIR);
        }
        let mut entries = vec![
            (ino.0, FileType::Directory, OsString::from(".")),
            (node.parent, FileType::Directory, OsString::from("..")),
        ];
        for &c in &node.children {
            if let Some(n) = tree.nodes.get(&c) {
                let kind = if n.dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                };
                entries.push((c, kind, n.name.clone()));
            }
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }
}

/// The mounted file system.
pub struct HostFiles {
    mount: PathBuf,
    tree: Arc<Mutex<Tree>>,
    unlock: Unlock,
    _session: BackgroundSession,
}

impl HostFiles {
    /// Mounts the file system on `mount`.
    pub fn mount(mount: PathBuf, fetch: Fetch, unlock: Unlock) -> std::io::Result<Self> {
        std::fs::create_dir_all(mount.parent().unwrap_or(Path::new("/")))?;
        // A previous agent killed without unmounting leaves a dead mount.
        if let Err(e) = std::fs::create_dir(&mount)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            let _ = std::process::Command::new("fusermount3")
                .args(["-u", "-z"])
                .arg(&mount)
                .status();
            std::fs::create_dir_all(&mount)?;
        }
        if std::fs::read_dir(&mount).is_err() {
            let _ = std::process::Command::new("fusermount3")
                .args(["-u", "-z"])
                .arg(&mount)
                .status();
        }
        let tree = Arc::new(Mutex::new(Tree::new()));
        let fs = Fs {
            tree: tree.clone(),
            fetch,
            runtime: tokio::runtime::Handle::current(),
        };
        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::RO,
            MountOption::NoDev,
            MountOption::NoSuid,
            MountOption::NoExec,
            MountOption::FSName("ameleo-display-agent".into()),
            MountOption::Subtype("clipboard".into()),
        ];
        let session = fuser::spawn_mount(fs, &mount, &config)?;
        tracing::info!("host files mounted on {}", mount.display());
        Ok(Self {
            mount,
            tree,
            unlock,
            _session: session,
        })
    }

    /// Puts a host list in the file system; returns the paths of its top
    /// level items.
    pub fn add(&self, clip_data_id: u32, entries: &[Entry]) -> Vec<PathBuf> {
        let (tops, gone) = self.tree.lock().unwrap().add_list(clip_data_id, entries);
        for id in gone {
            (self.unlock)(id);
        }
        tops.into_iter().map(|p| self.mount.join(p)).collect()
    }

    /// Drops every list (the host session ended).
    pub fn clear(&self) {
        let gone = self.tree.lock().unwrap().clear();
        for id in gone {
            (self.unlock)(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, dir: bool, size: u64) -> Entry {
        Entry {
            name: name.into(),
            attributes: if dir { 0x10 } else { 0x20 },
            size,
            write_time: 0,
            path: PathBuf::new(),
        }
    }

    #[test]
    fn tree_from_a_list() {
        let mut t = Tree::new();
        let list = [
            entry("d", true, 0),
            entry("d\\a.txt", false, 3),
            entry("d\\s", true, 0),
            entry("d\\s\\b", false, 1),
            entry("x\\orphan", false, 1),
            entry("top.bin", false, 9),
        ];
        let (tops, gone) = t.add_list(7, &list);
        assert_eq!(tops, [PathBuf::from("7/d"), PathBuf::from("7/top.bin")]);
        assert!(gone.is_empty());
        let root = t.child(INodeNo::ROOT.0, OsStr::new("7")).unwrap();
        let d = t.child(root, OsStr::new("d")).unwrap();
        let s = t.child(d, OsStr::new("s")).unwrap();
        let b = t.child(s, OsStr::new("b")).unwrap();
        assert_eq!(t.nodes[&b].host, Some((7, 3)));
        assert_eq!(t.attr(d).unwrap().kind, FileType::Directory);
        assert_eq!(t.attr(d).unwrap().perm, 0o755);
        assert_eq!(t.attr(b).unwrap().perm, 0o644);
        assert!(t.child(root, OsStr::new("orphan")).is_none());
        assert_eq!(
            t.nodes[&t.child(root, OsStr::new("top.bin")).unwrap()].host,
            Some((7, 5))
        );
    }

    #[test]
    fn read_only_files_stay_read_only() {
        let mut t = Tree::new();
        let mut ro = entry("ro.txt", false, 1);
        ro.attributes = 0x21; // READONLY | ARCHIVE
        t.add_list(1, &[ro, entry("rw.txt", false, 1)]);
        let root = t.child(INodeNo::ROOT.0, OsStr::new("1")).unwrap();
        let perm = |n: &str| t.attr(t.child(root, OsStr::new(n)).unwrap()).unwrap().perm;
        assert_eq!((perm("ro.txt"), perm("rw.txt")), (0o444, 0o644));
    }

    #[test]
    fn old_lists_go() {
        let mut t = Tree::new();
        let mut gone = Vec::new();
        for id in 1..=6 {
            gone.extend(t.add_list(id, &[entry("f", false, 1)]).1);
        }
        assert_eq!(gone, [1, 2]);
        assert!(t.child(INodeNo::ROOT.0, OsStr::new("2")).is_none());
        assert!(t.child(INodeNo::ROOT.0, OsStr::new("3")).is_some());
        assert_eq!(t.clear(), [3, 4, 5, 6]);
        assert_eq!(t.nodes.len(), 1);
    }

    /// Mounts the file system for real: needs /dev/fuse and fusermount3.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn mounted() {
        let content: Vec<u8> = (0..200_000u32).map(|i| i as u8).collect();
        let served = content.clone();
        // A host that gives at most 7000 bytes per answer.
        let fetch: Fetch = Arc::new(move |_, _, pos, len| {
            let served = served.clone();
            Box::pin(async move {
                let start = (pos as usize).min(served.len());
                let end = (start + (len as usize).min(7000)).min(served.len());
                Ok(served[start..end].to_vec())
            })
        });
        let unlocked = Arc::new(Mutex::new(Vec::new()));
        let u = unlocked.clone();
        let dir = std::env::temp_dir().join(format!("ada-fuse-{}", std::process::id()));
        let fs = HostFiles::mount(
            dir.clone(),
            fetch,
            Arc::new(move |id| u.lock().unwrap().push(id)),
        )
        .unwrap();
        let tops = fs.add(
            3,
            &[
                entry("folder", true, 0),
                entry("folder\\data.bin", false, content.len() as u64),
            ],
        );
        assert_eq!(tops, [dir.join("3/folder")]);
        let read = tokio::task::spawn_blocking({
            let p = dir.join("3/folder/data.bin");
            move || std::fs::read(p).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(read, content);
        assert!(std::fs::write(dir.join("3/folder/new"), b"x").is_err());
        fs.clear();
        assert_eq!(*unlocked.lock().unwrap(), [3]);
        drop(fs);
        let _ = std::fs::remove_dir(&dir);
    }
}
