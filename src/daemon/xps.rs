// SPDX-License-Identifier: GPL-3.0-only

//! The job's OpenXPS document, with its PrintTicket.
//!
//! Ghostscript's `xpswrite` makes a Microsoft XPS document, without a
//! ticket. The host wants OpenXPS (ECMA-388), as Windows makes it: the same
//! markup in another namespace, which the XML parts are moved to (the
//! relationship types share it). The client driver takes its settings from
//! the job ticket of the document: `Metadata/Job_PT.xml`, linked from the
//! fixed document sequence.

use std::io::{Read, Seek, Write};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

const TICKET_PART: &str = "Metadata/Job_PT.xml";
/// The namespace of Microsoft XPS, and the one of OpenXPS.
const XPS_NS: &str = "http://schemas.microsoft.com/xps/2005/06";
const OXPS_NS: &str = "http://schemas.openxps.org/oxps/v1.0";

/// The parts made of XML, moved to OpenXPS.
fn is_xml(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    [".fdseq", ".fdoc", ".fpage", ".rels", ".dict", ".xml"]
        .iter()
        .any(|e| n.ends_with(e))
}

fn to_oxps(xml: &str) -> String {
    xml.replace(XPS_NS, OXPS_NS)
}
const TICKET_TYPE: &str = "application/vnd.ms-printing.printticket+xml";

fn read_part<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Option<String> {
    let mut f = zip.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// The value of `attr="..."` in `tag`.
fn attr<'a>(tag: &'a str, attr: &str) -> Option<&'a str> {
    let at = tag.find(&format!("{attr}=\""))? + attr.len() + 2;
    let end = tag[at..].find('"')?;
    Some(&tag[at..at + end])
}

/// The relationships of `xml` (`<Relationship .../>` tags).
fn relationships(xml: &str) -> Vec<&str> {
    xml.split("<Relationship ")
        .skip(1)
        .filter_map(|r| r.split_once('>').map(|(t, _)| t))
        .collect()
}

/// Copies the XPS document of `input` to `out`, with `ticket` as its job
/// ticket (replacing any).
pub fn with_ticket<R: Read + Seek, W: Write + Seek>(
    input: R,
    out: W,
    ticket: &str,
) -> Result<(), String> {
    let mut zip = ZipArchive::new(input).map_err(|e| format!("not an XPS document: {e}"))?;
    let root_rels = read_part(&mut zip, "_rels/.rels").ok_or("no _rels/.rels")?;
    // The fixed document sequence, and the namespace of its relationship
    // types (XPS or OpenXPS).
    let (seq, rel_ns) = relationships(&root_rels)
        .into_iter()
        .find_map(|r| {
            let ty = attr(r, "Type")?;
            let ns = ty.strip_suffix("/fixedrepresentation")?;
            Some((
                attr(r, "Target")?.trim_start_matches('/').to_string(),
                ns.to_string(),
            ))
        })
        .ok_or("no fixed document sequence")?;
    let seq_rels = match seq.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/_rels/{file}.rels"),
        None => format!("_rels/{seq}.rels"),
    };
    // OpenXPS's relationship types, whichever the document had.
    let rel_ns = to_oxps(&rel_ns);
    let ticket_rel = format!(
        "<Relationship Type=\"{rel_ns}/printticket\" Target=\"/{TICKET_PART}\" Id=\"RameleoJobPT\" />"
    );
    let rels = match read_part(&mut zip, &seq_rels) {
        Some(old) => {
            let old = to_oxps(&old);
            // Its other relationships stay; a ticket it had goes.
            let mut kept = String::new();
            for r in relationships(&old) {
                if !attr(r, "Type").is_some_and(|t| t.ends_with("/printticket")) {
                    kept.push_str(&format!("<Relationship {r}>\n"));
                }
            }
            format!(
                "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n{kept}{ticket_rel}\n</Relationships>\n"
            )
        }
        None => format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n{ticket_rel}\n</Relationships>\n"
        ),
    };
    let mut types = read_part(&mut zip, "[Content_Types].xml").ok_or("no [Content_Types].xml")?;
    let override_ =
        format!("<Override PartName=\"/{TICKET_PART}\" ContentType=\"{TICKET_TYPE}\" />");
    if !types.contains(&format!("PartName=\"/{TICKET_PART}\"")) {
        let end = types.rfind("</Types>").ok_or("bad [Content_Types].xml")?;
        types.insert_str(end, &override_);
    }

    let mut w = ZipWriter::new(out);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let err = |e: zip::result::ZipError| e.to_string();
    w.start_file("[Content_Types].xml", opts).map_err(err)?;
    w.write_all(types.as_bytes()).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let f = zip.by_index_raw(i).map_err(err)?;
        let name = f.name().map_err(err)?.to_string();
        if name == "[Content_Types].xml" || name == seq_rels || name == TICKET_PART {
            continue;
        }
        if !is_xml(&name) {
            w.raw_copy_file(f).map_err(err)?;
            continue;
        }
        drop(f);
        let text = read_part(&mut zip, &name).ok_or_else(|| format!("cannot read {name}"))?;
        w.start_file(name, opts).map_err(err)?;
        w.write_all(to_oxps(&text).as_bytes())
            .map_err(|e| e.to_string())?;
    }
    w.start_file(seq_rels, opts).map_err(err)?;
    w.write_all(rels.as_bytes()).map_err(|e| e.to_string())?;
    w.start_file(TICKET_PART, opts).map_err(err)?;
    w.write_all(ticket.as_bytes()).map_err(|e| e.to_string())?;
    w.finish().map_err(err)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A document as `xpswrite` makes it.
    fn document() -> Vec<u8> {
        let mut w = ZipWriter::new(Cursor::new(Vec::new()));
        let o = SimpleFileOptions::default();
        for (name, data) in [
            ("FixedDocumentSequence.fdseq", "<FixedDocumentSequence/>"),
            (
                "[Content_Types].xml",
                "<?xml version=\"1.0\"?><Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\" /></Types>",
            ),
            ("Documents/1/FixedDocument.fdoc", "<FixedDocument/>"),
            (
                "_rels/.rels",
                "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\n<Relationship Type=\"http://schemas.microsoft.com/xps/2005/06/fixedrepresentation\" Target=\"/FixedDocumentSequence.fdseq\" Id=\"R1\" />\n</Relationships>",
            ),
            (
                "Documents/1/Pages/1.fpage",
                "<FixedPage xmlns=\"http://schemas.microsoft.com/xps/2005/06\"/>",
            ),
            ("Resources/f.ttf", "binary"),
        ] {
            w.start_file(name, o).unwrap();
            w.write_all(data.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn parts(zip: &[u8]) -> ZipArchive<Cursor<&[u8]>> {
        ZipArchive::new(Cursor::new(zip)).unwrap()
    }

    #[test]
    fn ticket_added() {
        let mut out = Cursor::new(Vec::new());
        with_ticket(Cursor::new(document()), &mut out, "<PT1/>").unwrap();
        // Again, on its own output: the ticket is replaced, not added.
        let mut again = Cursor::new(Vec::new());
        with_ticket(Cursor::new(out.into_inner()), &mut again, "<PT2/>").unwrap();
        let bytes = again.into_inner();
        let mut z = parts(&bytes);
        assert_eq!(
            z.by_index(0).unwrap().name().unwrap(),
            "[Content_Types].xml"
        );
        assert_eq!(read_part(&mut z, TICKET_PART).unwrap(), "<PT2/>");
        assert_eq!(
            read_part(&mut z, "Documents/1/Pages/1.fpage").unwrap(),
            "<FixedPage xmlns=\"http://schemas.openxps.org/oxps/v1.0\"/>"
        );
        assert_eq!(read_part(&mut z, "Resources/f.ttf").unwrap(), "binary");
        let root = read_part(&mut z, "_rels/.rels").unwrap();
        assert!(root.contains("http://schemas.openxps.org/oxps/v1.0/fixedrepresentation"));
        assert!(!root.contains(XPS_NS));
        let rels = read_part(&mut z, "_rels/FixedDocumentSequence.fdseq.rels").unwrap();
        assert_eq!(rels.matches("<Relationship ").count(), 1);
        assert!(rels.contains(
            "Type=\"http://schemas.openxps.org/oxps/v1.0/printticket\" Target=\"/Metadata/Job_PT.xml\""
        ));
        let types = read_part(&mut z, "[Content_Types].xml").unwrap();
        assert_eq!(types.matches("Job_PT.xml").count(), 1);
        assert_eq!(z.len(), 8);
    }

    #[test]
    fn not_xps() {
        let mut out = Cursor::new(Vec::new());
        assert!(with_ticket(Cursor::new(b"%PDF-1.4".to_vec()), &mut out, "").is_err());
    }

    /// A real document from Ghostscript: needs `gs`.
    #[test]
    #[ignore]
    fn ghostscript_document() {
        let dir = std::env::temp_dir().join(format!("ada-xps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ps = dir.join("t.ps");
        std::fs::write(&ps, "%!PS\n/Helvetica findfont 24 scalefont setfont 72 700 moveto (Bonjour) show showpage\n").unwrap();
        let xps = dir.join("t.xps");
        let ok = std::process::Command::new("gs")
            .args(["-q", "-dSAFER", "-dBATCH", "-dNOPAUSE", "-sDEVICE=xpswrite"])
            .arg(format!("-sOutputFile={}", xps.display()))
            .arg(&ps)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let mut out = Cursor::new(Vec::new());
        with_ticket(std::fs::File::open(&xps).unwrap(), &mut out, "<PT/>").unwrap();
        let bytes = out.into_inner();
        let mut z = parts(&bytes);
        assert_eq!(read_part(&mut z, TICKET_PART).unwrap(), "<PT/>");
        assert!(read_part(&mut z, "Documents/1/Pages/1.fpage").is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
