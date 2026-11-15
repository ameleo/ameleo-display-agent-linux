// SPDX-License-Identifier: GPL-3.0-only

//! Client printers: CUPS queues, whose jobs go to the host.
//!
//! Each client printer is a CUPS queue with the `ameleo` backend
//! (`ameleo:/<queue>`), for the user in front of the screen only. Its PPD
//! offers the client driver's options ([`super::printcaps`]). CUPS turns
//! each job into PDF and runs the backend (as `lp`), which makes an XPS
//! document of it with Ghostscript and hands it to the daemon on
//! [`crate::ipc::print_socket`]. The daemon adds the job's PrintTicket and
//! sends the document to the host (`Host.Printer1`).
//!
//! When the host session ends, the printers stay 60 s, disabled: a printer
//! of the same name in the next session takes the queue over (the user's
//! settings with it); the others are deleted then.

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::{Error, HostPrinterProxy, Result};
use tokio::net::UnixListener;
use zbus::proxy::CacheProperties;

use super::Shared;
use super::agents::host_call;
use super::printcaps::{Caps, job_options};

/// The device URI scheme of our queues: the backend's name.
pub const SCHEME: &str = "ameleo";
/// How long the printers of an ended session wait for the next one.
const ORPHAN_DELAY: Duration = Duration::from_secs(60);
/// Longest wait for the host to take a whole job (`EndJob`).
const END_JOB_TIMEOUT: Duration = Duration::from_secs(600);

struct Printer {
    id: u32,
    name: String,
    queue: String,
    is_default: bool,
    caps: Arc<Caps>,
    /// The session ended: deleted when this runs out.
    orphan: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
pub struct Printers {
    list: Mutex<Vec<Printer>>,
    /// CUPS changes, one at a time and in order.
    cups: tokio::sync::Mutex<()>,
    next_job: AtomicU32,
    /// `AddPrinter` calls whose queue is being made.
    pending: std::sync::atomic::AtomicUsize,
}

/// Runs a CUPS command; its output, or its error.
async fn cups(cmd: &str, args: &[&str]) -> std::result::Result<String, String> {
    let out = tokio::process::Command::new(cmd)
        .args(args)
        .env("LC_ALL", "C")
        .output()
        .await
        .map_err(|e| format!("{cmd}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{cmd} {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// The login name of `uid`.
async fn user_name(uid: u32) -> Option<String> {
    let out = tokio::process::Command::new("getent")
        .args(["passwd", &uid.to_string()])
        .output()
        .await
        .ok()?;
    let line = String::from_utf8_lossy(&out.stdout);
    line.split(':')
        .next()
        .filter(|n| !n.trim().is_empty())
        .map(|n| n.trim().to_string())
}

/// The CUPS queues: `(name, device URI)`.
async fn queues() -> Vec<(String, String)> {
    let Ok(out) = cups("lpstat", &["-v"]).await else {
        return Vec::new();
    };
    out.lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("device for ")?;
            let (q, uri) = rest.split_once(": ")?;
            Some((q.to_string(), uri.trim().to_string()))
        })
        .collect()
}

/// A CUPS queue name from a printer's name: letters, digits, `-`, `_`.
fn queue_name(name: &str) -> String {
    let mut q = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '.' {
            q.push(c);
        } else if !q.ends_with('_') {
            q.push('_');
        }
    }
    // Never `-` first: the name goes to CUPS commands as an argument.
    let q: String = q.trim_matches(['_', '.', '-']).chars().take(100).collect();
    if q.is_empty() {
        "client-printer".into()
    } else {
        q
    }
}

impl Printers {
    /// Deletes the queues a previous daemon left.
    pub async fn cleanup(&self) {
        let _cups = self.cups.lock().await;
        for (q, uri) in queues().await {
            if uri.starts_with(&format!("{SCHEME}:")) {
                tracing::info!(queue = q, "deleting a client printer left behind");
                if let Err(e) = cups("lpadmin", &["-x", &q]).await {
                    tracing::warn!("{e}");
                }
            }
        }
    }

    /// `AddPrinter`: the queue is made in the background.
    pub fn add(
        self: &Arc<Self>,
        shared: Arc<Shared>,
        id: u32,
        name: String,
        is_default: bool,
        capabilities: String,
        default_ticket: String,
    ) {
        let caps = match Caps::new(&capabilities, &default_ticket) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    id,
                    "client printer capabilities not understood ({e}): generic options"
                );
                Caps::new("", &default_ticket).unwrap_or_default()
            }
        };
        tracing::info!(
            id,
            name,
            options = ?caps.features.iter().map(|f| f.keyword.as_str()).collect::<Vec<_>>(),
            "client printer options"
        );
        let this = self.clone();
        this.pending.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let _cups = this.cups.lock().await;
            // The same printer of this session again, or one of the last
            // session's by its name (ids are the host's, per session).
            let reused = {
                let mut list = this.list.lock().unwrap();
                let at = list
                    .iter()
                    .position(|p| p.id == id && p.orphan.is_none())
                    .or_else(|| {
                        list.iter()
                            .position(|p| p.orphan.is_some() && p.name == name)
                    });
                at.map(|i| list.remove(i))
            };
            let queue = match &reused {
                Some(p) => {
                    if let Some(t) = &p.orphan {
                        t.abort();
                    }
                    p.queue.clone()
                }
                None => {
                    // Other queues' names are taken; ours left behind are
                    // free.
                    let taken: Vec<String> = queues()
                        .await
                        .into_iter()
                        .filter(|(_, uri)| !uri.starts_with(&format!("{SCHEME}:")))
                        .map(|q| q.0)
                        .collect();
                    let ours: Vec<String> = this
                        .list
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|p| p.queue.clone())
                        .collect();
                    let base = queue_name(&name);
                    (1..)
                        .map(|n| {
                            if n == 1 {
                                base.clone()
                            } else {
                                format!("{base}_{n}")
                            }
                        })
                        .find(|q| !taken.contains(q) && !ours.contains(q))
                        .unwrap()
                }
            };
            let user = match shared.active_uid() {
                Some(uid) => user_name(uid).await,
                None => None,
            };
            match install(&queue, &name, &caps, user.as_deref()).await {
                Ok(()) => {
                    tracing::info!(id, queue, user, "client printer installed");
                    this.list.lock().unwrap().push(Printer {
                        id,
                        name,
                        queue: queue.clone(),
                        is_default,
                        caps: Arc::new(caps),
                        orphan: None,
                    });
                    if is_default {
                        shared.set_default_printer(&queue);
                    }
                }
                Err(e) => {
                    tracing::warn!(id, queue, "client printer not installed: {e}");
                    if reused.is_some() {
                        let _ = cups("lpadmin", &["-x", &queue]).await;
                    }
                }
            }
            this.pending.fetch_sub(1, Ordering::Relaxed);
        });
    }

    pub fn remove(self: &Arc<Self>, id: u32) {
        let this = self.clone();
        tokio::spawn(async move {
            let _cups = this.cups.lock().await;
            let p = {
                let mut list = this.list.lock().unwrap();
                list.iter().position(|p| p.id == id).map(|i| list.remove(i))
            };
            if let Some(p) = p {
                delete(p).await;
            }
        });
    }

    /// The host session ended: the printers wait for the next one.
    pub fn orphan_all(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            let _cups = this.cups.lock().await;
            let queues: Vec<String> = {
                let mut list = this.list.lock().unwrap();
                list.iter_mut()
                    .filter(|p| p.orphan.is_none())
                    .map(|p| {
                        let this = this.clone();
                        let id = p.id;
                        p.orphan = Some(tokio::spawn(async move {
                            tokio::time::sleep(ORPHAN_DELAY).await;
                            this.remove_orphan(id).await;
                        }));
                        p.queue.clone()
                    })
                    .collect()
            };
            // Unusable: no user may print. (`cupsdisable` is refused for a
            // queue limited to users, even to root.)
            for q in queues {
                tracing::info!(queue = q, "client printer unusable until the next session");
                if let Err(e) = cups("lpadmin", &["-p", &q, "-u", "allow:root"]).await {
                    tracing::warn!("{e}");
                }
            }
        });
    }

    async fn remove_orphan(&self, id: u32) {
        let _cups = self.cups.lock().await;
        let p = {
            let mut list = self.list.lock().unwrap();
            list.iter()
                .position(|p| p.id == id && p.orphan.is_some())
                .map(|i| list.remove(i))
        };
        if let Some(mut p) = p {
            // This runs in that task: aborting it would stop the deletion.
            p.orphan = None;
            delete(p).await;
        }
    }

    /// Another user is in front of the screen: the printers are theirs.
    pub fn user_changed(self: &Arc<Self>, shared: Arc<Shared>, uid: Option<u32>) {
        let this = self.clone();
        tokio::spawn(async move {
            let _cups = this.cups.lock().await;
            let user = match uid {
                Some(uid) => user_name(uid).await,
                None => None,
            };
            let allow = format!("allow:{}", user.as_deref().unwrap_or("root"));
            let printers: Vec<(String, bool)> = this
                .list
                .lock()
                .unwrap()
                .iter()
                // The last session's stay unusable until deleted.
                .filter(|p| p.orphan.is_none())
                .map(|p| (p.queue.clone(), p.is_default))
                .collect();
            for (q, is_default) in printers {
                if let Err(e) = cups("lpadmin", &["-p", &q, "-u", &allow]).await {
                    tracing::warn!("{e}");
                }
                if is_default {
                    shared.set_default_printer(&q);
                }
            }
        });
    }

    /// What a job of `queue` needs: the printer id and its options.
    fn job_target(&self, queue: &str) -> Option<(u32, Arc<Caps>)> {
        self.list
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.queue == queue && p.orphan.is_none())
            .map(|p| (p.id, p.caps.clone()))
    }

    /// Printers, those waiting for the next session included.
    /// Printers of this session, those being installed included.
    pub fn count(&self) -> usize {
        let installed = self
            .list
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.orphan.is_none())
            .count();
        installed + self.pending.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn insert_for_test(&self, id: u32, queue: &str, caps: Caps) {
        self.list.lock().unwrap().push(Printer {
            id,
            name: queue.into(),
            queue: queue.into(),
            is_default: false,
            caps: Arc::new(caps),
            orphan: None,
        });
    }
}

/// Makes (or updates) the CUPS queue.
async fn install(
    queue: &str,
    name: &str,
    caps: &Caps,
    user: Option<&str>,
) -> std::result::Result<(), String> {
    let dir = std::env::temp_dir();
    let ppd = dir.join(format!("ameleo-{queue}.ppd"));
    std::fs::write(&ppd, caps.ppd(name)).map_err(|e| format!("{}: {e}", ppd.display()))?;
    let uri = format!("{SCHEME}:/{queue}");
    let allow = format!("allow:{}", user.unwrap_or("root"));
    let res = cups(
        "lpadmin",
        &[
            "-p",
            queue,
            "-E",
            "-v",
            &uri,
            "-P",
            &ppd.to_string_lossy(),
            "-D",
            name,
            "-L",
            "ameleo client",
            "-o",
            "printer-is-shared=false",
            // A failed job must not stop the queue: the user could not
            // start it again.
            "-o",
            "printer-error-policy=abort-job",
            "-u",
            &allow,
        ],
    )
    .await;
    let _ = std::fs::remove_file(&ppd);
    // `-E` enables the queue and makes it accept jobs, a queue taken
    // over from the last session too.
    res.map(|_| ())
}

async fn delete(p: Printer) {
    if let Some(t) = p.orphan {
        t.abort();
    }
    match cups("lpadmin", &["-x", &p.queue]).await {
        Ok(_) => tracing::info!(id = p.id, queue = p.queue, "client printer deleted"),
        Err(e) => tracing::warn!("{e}"),
    }
}

/// Accepts the CUPS backend on `path`: only root and `lp` may print.
pub fn listen(shared: Arc<Shared>, path: &std::path::Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    tokio::spawn(async move {
        let lp = lp_uid().await;
        // Root in production; the user running the tests otherwise.
        let own = rustix::process::getuid().as_raw();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            };
            let uid = stream.peer_cred().map(|c| c.uid()).unwrap_or(u32::MAX);
            if uid != 0 && uid != own && Some(uid) != lp {
                tracing::warn!(uid, "print socket: refused");
                continue;
            }
            let shared = shared.clone();
            tokio::spawn(async move {
                let conn = zbus::connection::Builder::unix_stream(stream)
                    .server(zbus::Guid::generate())
                    .map(|b| b.p2p())
                    .and_then(|b| b.serve_at(crate::ipc::PRINT_PATH, PrintIface(shared)));
                match conn {
                    Ok(b) => match b.build().await {
                        Ok(conn) => conn.closed().await,
                        Err(e) => tracing::warn!("print socket: {e}"),
                    },
                    Err(e) => tracing::warn!("print socket: {e}"),
                }
            });
        }
    });
    Ok(())
}

async fn lp_uid() -> Option<u32> {
    let out = tokio::process::Command::new("getent")
        .args(["passwd", "lp"])
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&out.stdout)
        .split(':')
        .nth(2)?
        .parse()
        .ok()
}

/// What the CUPS backend calls.
pub struct PrintIface(Arc<Shared>);

#[zbus::interface(name = "fr.ameleo.DisplayAgent.Print1")]
impl PrintIface {
    /// A job for `queue`: `document` is an XPS document without ticket,
    /// `options` the CUPS job options.
    async fn submit(
        &self,
        #[zbus(connection)] backend: &zbus::Connection,
        queue: &str,
        title: &str,
        user: &str,
        options: &str,
        document: zbus::zvariant::OwnedFd,
    ) -> Result<()> {
        let (printer_id, caps) = self
            .0
            .printers
            .job_target(queue)
            .ok_or_else(|| Error::NotFound(format!("no client printer for {queue}")))?;
        let host = self
            .0
            .host()
            .ok_or_else(|| Error::Failed("no host session".into()))?;
        let opts = job_options(options);
        let ticket = caps.ticket(&opts);
        tracing::debug!(queue, ?opts, ticket, "client printer job ticket");
        // The document with its ticket, in memory.
        let doc = std::fs::File::from(std::os::fd::OwnedFd::from(document));
        let t = ticket.clone();
        let mut out = tokio::task::spawn_blocking(move || -> std::result::Result<_, String> {
            let fd = rustix::fs::memfd_create("ameleo-job", rustix::fs::MemfdFlags::CLOEXEC)
                .map_err(|e| e.to_string())?;
            let mut out = std::fs::File::from(fd);
            super::xps::with_ticket(doc, &mut out, &t)?;
            out.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
            Ok(out)
        })
        .await
        .map_err(|e| Error::Failed(e.to_string()))?
        .map_err(Error::Failed)?;

        let proxy = HostPrinterProxy::builder(&host)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let job = self.0.printers.next_job.fetch_add(1, Ordering::Relaxed) + 1;
        let size = out.metadata().map(|m| m.len()).unwrap_or(0);
        tracing::info!(
            queue,
            printer_id,
            job,
            title,
            user,
            size,
            "client printer job"
        );
        host_call(proxy.begin_job(printer_id, job, "application/oxps", title, user, &ticket))
            .await?;
        let sending = async {
            let mut buf = vec![0u8; proto::printer::CHUNK_MAX];
            loop {
                let n = out
                    .read(&mut buf)
                    .map_err(|e| Error::Failed(e.to_string()))?;
                if n == 0 {
                    break;
                }
                host_call(proxy.write_job(job, buf[..n].to_vec())).await?;
            }
            tokio::time::timeout(END_JOB_TIMEOUT, proxy.end_job(job))
                .await
                .map_err(|_| Error::Failed("the host did not take the job".into()))?
                .map_err(|e| Error::Failed(e.to_string()))
        };
        // The backend goes away when CUPS cancels the job: the host drops
        // it too.
        let sent: Result<()> = tokio::select! {
            r = sending => r,
            _ = backend.closed() => Err(Error::Failed("the job was cancelled".into())),
        };
        match &sent {
            Ok(()) => tracing::info!(job, "client printer job sent"),
            Err(e) => {
                tracing::warn!(job, "client printer job failed: {e}");
                let _ = proxy.abort_job(job).await;
            }
        }
        sent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_names() {
        assert_eq!(
            queue_name("HP LaserJet (redirection 2)"),
            "HP_LaserJet_redirection_2"
        );
        assert_eq!(queue_name("Imprimante d'Éric"), "Imprimante_d_ric");
        assert_eq!(queue_name("///"), "client-printer");
        assert_eq!(queue_name("-x printer"), "x_printer");
    }
}
