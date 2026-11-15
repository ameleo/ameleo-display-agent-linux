// SPDX-License-Identifier: GPL-3.0-only

//! The `ameleo` CUPS backend: the jobs of the client printers.
//!
//! CUPS runs it (as `lp`) with the job as PDF, as the queue's PPD asks
//! (`*cupsFilter: "application/vnd.cups-pdf 0 -"`):
//!
//! ```text
//! ameleo-display-agent cups-backend job user title copies options [file]
//! ```
//!
//! with the queue in `DEVICE_URI` (`ameleo:/<queue>`). It makes an XPS
//! document of the PDF with Ghostscript (`xpswrite`), and hands it to the
//! daemon on [`crate::ipc::print_socket`], which sends it to the host.
//! Messages go to CUPS on stderr, with its prefixes (`ERROR:`...).

use std::io::Write;
use std::path::{Path, PathBuf};

use zbus::proxy::CacheProperties;

/// CUPS backend exit codes.
const OK: i32 = 0;
const FAILED: i32 = 1;

/// Temporary files, removed when dropped.
struct Temp(Vec<PathBuf>);

impl Drop for Temp {
    fn drop(&mut self) {
        for p in &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub async fn run(args: &[String]) -> i32 {
    // No arguments: CUPS lists the devices; ours are made by the daemon.
    if args.is_empty() {
        return OK;
    }
    if args.len() < 5 {
        eprintln!("Usage: ameleo-display-agent cups-backend job user title copies options [file]");
        return FAILED;
    }
    // CUPS cancels a job with SIGTERM: the job is dropped, and with it
    // the temporary files, Ghostscript and the connection (the daemon then
    // aborts the job on the host).
    let Ok(mut term) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    else {
        return FAILED;
    };
    tokio::select! {
        r = print(args) => match r {
            Ok(()) => OK,
            Err(e) => {
                eprintln!("ERROR: {e}");
                FAILED
            }
        },
        _ = term.recv() => {
            eprintln!("INFO: Job cancelled");
            FAILED
        }
    }
}

async fn print(args: &[String]) -> Result<(), String> {
    let (job, user, title) = (&args[0], &args[1], &args[2]);
    // Given a file, the backend makes the copies (CUPS's filters made them
    // otherwise): the driver is asked for them.
    let mut options = args[4].clone();
    if args.get(5).is_some()
        && let Ok(copies) = args[3].parse::<u32>()
        && copies > 1
    {
        options.push_str(&format!(" ameleo-copies={copies}"));
    }
    let options = &options;
    let uri = std::env::var("DEVICE_URI").map_err(|_| "no DEVICE_URI")?;
    let queue = uri
        .strip_prefix("ameleo:")
        .map(|q| q.trim_start_matches('/'))
        .filter(|q| !q.is_empty())
        .ok_or_else(|| format!("not a client printer: {uri}"))?
        .to_string();
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let base = tmp.join(format!("ameleo-{}-{job}", std::process::id()));
    let mut temp = Temp(Vec::new());
    // The PDF: the file CUPS gave, or stdin.
    let pdf = match args.get(5) {
        Some(f) => PathBuf::from(f),
        None => {
            let p = base.with_extension("pdf");
            temp.0.push(p.clone());
            let mut f = std::fs::File::create(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            std::io::copy(&mut std::io::stdin().lock(), &mut f).map_err(|e| e.to_string())?;
            f.flush().map_err(|e| e.to_string())?;
            p
        }
    };
    let outlines = base.with_extension("outlines.pdf");
    let xps = base.with_extension("xps");
    temp.0.push(outlines.clone());
    temp.0.push(xps.clone());
    eprintln!("INFO: Converting the document");
    to_xps(&pdf, &outlines, &xps).await?;

    eprintln!("INFO: Sending the document to the client");
    let stream = tokio::net::UnixStream::connect(crate::ipc::print_socket())
        .await
        .map_err(|e| format!("the agent's daemon is not running: {e}"))?;
    let conn = zbus::connection::Builder::unix_stream(stream)
        .p2p()
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let proxy = crate::ipc::PrintProxy::builder(&conn)
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let doc = std::fs::File::open(&xps).map_err(|e| e.to_string())?;
    use std::os::fd::AsFd;
    proxy
        .submit(&queue, title, user, options, doc.as_fd().into())
        .await
        .map_err(|e| match e {
            zbus::Error::MethodError(_, Some(msg), _) => msg,
            e => e.to_string(),
        })?;
    eprintln!("INFO: Sent to the client printer");
    Ok(())
}

/// PDF to XPS, with Ghostscript, in two passes. `xpswrite` cannot write
/// fonts: it draws the text of embedded fonts as small rectangles at
/// 96 dpi. So the text first becomes outlines (`pdfwrite` with
/// `NoOutputFonts`), which `xpswrite` writes as vector paths.
async fn to_xps(pdf: &Path, outlines: &Path, xps: &Path) -> Result<(), String> {
    gs("pdfwrite", &["-dNoOutputFonts"], pdf, outlines).await?;
    gs("xpswrite", &[], outlines, xps).await
}

async fn gs(device: &str, extra: &[&str], input: &Path, output: &Path) -> Result<(), String> {
    let out = tokio::process::Command::new("gs")
        .args(["-q", "-dSAFER", "-dBATCH", "-dNOPAUSE"])
        .arg(format!("-sDEVICE={device}"))
        .args(extra)
        .arg(format!("-sOutputFile={}", output.display()))
        .arg(input)
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("Ghostscript (gs): {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "Ghostscript could not convert the document: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}
