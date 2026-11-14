// SPDX-License-Identifier: GPL-3.0-or-later

//! Host to guest file push into the user's Downloads folder
//! (`XDG_DOWNLOAD_DIR`). The session agent runs as the user: the files are
//! theirs.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use proto::Error;

/// Same limit as spice-vdagent: `name (63).ext` at most.
const MAX_ATTEMPTS: u32 = 64;
/// `NAME_MAX`.
const MAX_NAME: usize = 255;

struct Task {
    file: Option<File>,
    path: PathBuf,
    size: u64,
    written: u64,
}

impl Drop for Task {
    fn drop(&mut self) {
        // Still open: unfinished transfer, remove the partial file.
        if self.file.take().is_some()
            && let Err(e) = std::fs::remove_file(&self.path)
        {
            tracing::warn!("cannot remove partial file {}: {e}", self.path.display());
        }
    }
}

#[derive(Default)]
pub struct FileXfer {
    next_id: u32,
    tasks: HashMap<u32, Task>,
}

impl FileXfer {
    /// Aborts every transfer.
    pub fn reset(&mut self) {
        self.tasks.clear();
    }

    pub fn start(&mut self, name: &str, size: u64) -> Result<u32, Error> {
        validate_name(name)?;
        let dir = download_dir().ok_or_else(|| Error::Failed("no download folder".into()))?;
        std::fs::create_dir_all(&dir)
            .map_err(|e| Error::Failed(format!("cannot create {}: {e}", dir.display())))?;
        let free = free_space(&dir).map_err(|e| Error::Failed(e.to_string()))?;
        if free < size {
            return Err(Error::NotEnoughSpace(free.to_string()));
        }
        let (file, path) = create_unique(&dir, name)?;
        tracing::info!("file transfer to {} ({size} bytes)", path.display());

        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        self.tasks.insert(
            id,
            Task {
                file: Some(file),
                path,
                size,
                written: 0,
            },
        );
        Ok(id)
    }

    pub fn write(&mut self, id: u32, data: &[u8]) -> Result<(), Error> {
        let task = self
            .tasks
            .get_mut(&id)
            .ok_or_else(|| Error::NotFound(format!("transfer {id}")))?;
        if task.written + data.len() as u64 > task.size {
            self.tasks.remove(&id);
            return Err(Error::InvalidArgument("more data than announced".into()));
        }
        let res = task.file.as_mut().expect("open task").write_all(data);
        if let Err(e) = res {
            self.tasks.remove(&id);
            return Err(Error::Failed(format!("write failed: {e}")));
        }
        task.written += data.len() as u64;
        Ok(())
    }

    /// Returns the file's path and its final name.
    pub fn finish(&mut self, id: u32) -> Result<(PathBuf, String), Error> {
        let mut task = self
            .tasks
            .remove(&id)
            .ok_or_else(|| Error::NotFound(format!("transfer {id}")))?;
        if task.written != task.size {
            return Err(Error::InvalidArgument(format!(
                "{} of {} bytes received",
                task.written, task.size
            )));
        }
        let file = task.file.take().expect("open task");
        if let Err(e) = file.sync_all() {
            tracing::warn!("cannot flush {}: {e}", task.path.display());
        }
        drop(file); // close and keep
        let path = std::mem::take(&mut task.path);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok((path, name))
    }

    pub fn cancel(&mut self, id: u32) -> Result<(), Error> {
        self.tasks
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| Error::NotFound(format!("transfer {id}")))
    }
}

fn validate_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > MAX_NAME - 8 // room for " (63)"
        || name.contains(['/', '\\', '\0'])
        || name.chars().any(char::is_control)
    {
        return Err(Error::InvalidArgument(format!(
            "invalid file name {name:?}"
        )));
    }
    Ok(())
}

fn download_dir() -> Option<PathBuf> {
    dirs::download_dir().or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
}

fn create_unique(dir: &Path, name: &str) -> Result<(File, PathBuf), Error> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for attempt in 0..MAX_ATTEMPTS {
        let candidate = if attempt == 0 {
            name.to_string()
        } else {
            format!("{stem} ({attempt}){ext}")
        };
        let path = dir.join(candidate);
        // create_new: O_EXCL, never follows a symlink planted there.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(Error::Failed(format!(
                    "cannot create {}: {e}",
                    path.display()
                )));
            }
        }
    }
    Err(Error::Failed(format!("too many copies of {name}")))
}

fn free_space(dir: &Path) -> std::io::Result<u64> {
    let st = rustix::fs::statvfs(dir)?;
    Ok(st.f_bavail.saturating_mul(st.f_frsize))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("report.pdf").is_ok());
        assert!(validate_name(".bashrc").is_ok());
        assert!(validate_name("a:b").is_ok());
        assert!(validate_name("..").is_err());
        assert!(validate_name("dir/x").is_err());
        assert!(validate_name("dir\\x").is_err());
        assert!(validate_name("a\nb").is_err());
        assert!(validate_name(&"x".repeat(300)).is_err());
    }

    #[test]
    fn unfinished_task_is_deleted_finished_is_kept() {
        let dir = std::env::temp_dir().join(format!("ada-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let (file, path) = create_unique(&dir, "partial.bin").unwrap();
        drop(Task {
            file: Some(file),
            path: path.clone(),
            size: 10,
            written: 0,
        });
        assert!(!path.exists(), "partial file left behind");

        let (file, path) = create_unique(&dir, "done.bin").unwrap();
        let mut task = Task {
            file: Some(file),
            path: path.clone(),
            size: 0,
            written: 0,
        };
        drop(task.file.take());
        drop(task);
        assert!(path.exists(), "finished file removed");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unique_suffix() {
        let dir = std::env::temp_dir().join(format!("ada-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_, a) = create_unique(&dir, "f.txt").unwrap();
        let (_, b) = create_unique(&dir, "f.txt").unwrap();
        let (_, c) = create_unique(&dir, ".hidden").unwrap();
        let (_, d) = create_unique(&dir, ".hidden").unwrap();
        assert_eq!(a.file_name().unwrap(), "f.txt");
        assert_eq!(b.file_name().unwrap(), "f (1).txt");
        assert_eq!(c.file_name().unwrap(), ".hidden");
        assert_eq!(d.file_name().unwrap(), ".hidden (1)");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn transfer_flow() {
        let mut x = FileXfer::default();
        assert!(matches!(x.write(1, b"x"), Err(Error::NotFound(_))));
        assert!(matches!(x.start("a/b", 1), Err(Error::InvalidArgument(_))));
    }
}
