// SPDX-License-Identifier: GPL-3.0-only

//! Clipboard types between the wire ([`proto::mime`]) and the guest apps.
//!
//! * Text is UTF-8 with `\n` line endings on both sides: no conversion.
//! * Images: the wire has PNG and BMP (a BMP file). Linux apps mostly offer
//!   and want PNG; the agent converts when one side has only the other.

use std::io::Cursor;

use proto::mime;

/// Text types guest apps offer and ask for, best first.
pub const GUEST_TEXT: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// Image types we can read from a guest app, best first.
const GUEST_IMAGES: &[&str] = &["image/png", "image/bmp", "image/x-bmp", "image/jpeg"];

/// File lists of the file managers: Dolphin and most apps, then Nautilus.
pub const GUEST_FILES: &[&str] = &["text/uri-list", "x-special/gnome-copied-files"];

pub fn is_file_list(m: &str) -> bool {
    GUEST_FILES.contains(&m)
}

/// The file list of `paths` as the guest type `mime`.
pub fn file_list(mime: &str, paths: &[std::path::PathBuf]) -> Vec<u8> {
    let uris: Vec<String> = paths.iter().map(|p| super::filelist::file_uri(p)).collect();
    if mime == "x-special/gnome-copied-files" {
        format!("copy\n{}", uris.join("\n")).into_bytes()
    } else {
        uris.iter()
            .map(|u| format!("{u}\r\n"))
            .collect::<String>()
            .into_bytes()
    }
}

pub fn is_text(m: &str) -> bool {
    m.eq_ignore_ascii_case(mime::TEXT) || GUEST_TEXT.iter().any(|t| t.eq_ignore_ascii_case(m))
}

fn is_image(m: &str) -> bool {
    m == mime::PNG || m == mime::BMP
}

/// The wire types the guest clipboard can be exported as, from what the
/// guest app offers.
pub fn exportable(guest: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if guest.iter().any(|m| is_file_list(m)) {
        out.push(mime::FILE_LIST.to_string());
    }
    if guest.iter().any(|m| is_text(m)) {
        out.push(mime::TEXT.to_string());
    }
    if guest.iter().any(|m| GUEST_IMAGES.contains(&m.as_str())) {
        out.push(mime::PNG.to_string());
        out.push(mime::BMP.to_string());
    }
    out
}

/// The guest type to read to answer a host request for `wanted`.
pub fn guest_source(wanted: &str, guest: &[String]) -> Option<String> {
    let pick = |list: &[&str]| {
        list.iter()
            .find(|c| guest.iter().any(|g| g.eq_ignore_ascii_case(c)))
            .map(|c| c.to_string())
    };
    if wanted.eq_ignore_ascii_case(mime::TEXT) {
        return pick(GUEST_TEXT);
    }
    if wanted == mime::FILE_LIST {
        return pick(GUEST_FILES);
    }
    if wanted == mime::PNG || wanted == mime::BMP {
        // The type asked for as it is, else any image we can convert.
        if guest.iter().any(|g| g == wanted) {
            return Some(wanted.to_string());
        }
        return pick(GUEST_IMAGES);
    }
    None
}

/// The guest types to offer for a host grab of `host` types; `files`: host
/// files can be pasted.
pub fn offer_for(host: &[String], files: bool) -> Vec<String> {
    let mut out = Vec::new();
    if files && host.iter().any(|m| m == mime::FILE_LIST) {
        out.extend(GUEST_FILES.iter().map(|s| s.to_string()));
    }
    if host.iter().any(|m| is_text(m)) {
        out.extend(GUEST_TEXT.iter().map(|s| s.to_string()));
    }
    if host.iter().any(|m| is_image(m)) {
        out.push("image/png".to_string());
        out.push("image/bmp".to_string());
    }
    out
}

/// The host types to ask for when a guest app pastes `guest`, best first.
pub fn host_types_for(guest: &str, host: &[String]) -> Vec<String> {
    if is_text(guest) {
        return host.iter().filter(|m| is_text(m)).cloned().collect();
    }
    if is_file_list(guest) {
        return host
            .iter()
            .filter(|m| *m == mime::FILE_LIST)
            .cloned()
            .collect();
    }
    let order: &[&str] = match guest {
        "image/png" => &[mime::PNG, mime::BMP],
        "image/bmp" | "image/x-bmp" => &[mime::BMP, mime::PNG],
        _ => &[],
    };
    order
        .iter()
        .filter(|m| host.iter().any(|h| h == *m))
        .map(|m| m.to_string())
        .collect()
}

/// Converts clipboard data from `from` to `to` (both MIME types). Same
/// type, or text to text: as it is.
pub fn convert(from: &str, to: &str, data: Vec<u8>) -> Result<Vec<u8>, String> {
    if from.eq_ignore_ascii_case(to) || (is_text(from) && is_text(to)) {
        return Ok(data);
    }
    let format = |m: &str| match m {
        "image/png" => Some(image::ImageFormat::Png),
        "image/bmp" | "image/x-bmp" => Some(image::ImageFormat::Bmp),
        "image/jpeg" => Some(image::ImageFormat::Jpeg),
        _ => None,
    };
    let (Some(src), Some(dst)) = (format(from), format(to)) else {
        return Err(format!("no conversion from {from} to {to}"));
    };
    let img = image::load_from_memory_with_format(&data, src).map_err(|e| e.to_string())?;
    // BMP keeps the alpha channel only as 32 bit, which some readers
    // ignore: fine, PNG is the type that keeps it.
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, dst).map_err(|e| e.to_string())?;
    Ok(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn export_types() {
        assert_eq!(exportable(&v(&["UTF8_STRING", "TEXT"])), v(&[mime::TEXT]));
        assert_eq!(exportable(&v(&["image/jpeg"])), v(&[mime::PNG, mime::BMP]));
        assert_eq!(
            exportable(&v(&["text/uri-list", "text/plain"])),
            v(&[mime::FILE_LIST, mime::TEXT])
        );
        assert_eq!(
            guest_source(mime::FILE_LIST, &v(&["x-special/gnome-copied-files"])).as_deref(),
            Some("x-special/gnome-copied-files")
        );
        let paths = [std::path::PathBuf::from("/a b"), "/c".into()];
        assert_eq!(
            file_list("text/uri-list", &paths),
            b"file:///a%20b\r\nfile:///c\r\n"
        );
        assert_eq!(
            file_list("x-special/gnome-copied-files", &paths),
            b"copy\nfile:///a%20b\nfile:///c"
        );
    }

    #[test]
    fn sources() {
        let g = v(&["text/plain", "UTF8_STRING"]);
        assert_eq!(guest_source(mime::TEXT, &g).as_deref(), Some("UTF8_STRING"));
        let g = v(&["image/jpeg", "image/png"]);
        assert_eq!(guest_source(mime::BMP, &g).as_deref(), Some("image/png"));
        assert_eq!(guest_source(mime::PNG, &g).as_deref(), Some("image/png"));
        assert_eq!(guest_source("FileGroupDescriptorW", &g), None);
    }

    #[test]
    fn host_types() {
        let h = v(&[mime::BMP]);
        assert_eq!(host_types_for("image/png", &h), v(&[mime::BMP]));
        let h = v(&[mime::TEXT, "text/plain"]);
        assert_eq!(host_types_for("STRING", &h), h);
        assert!(offer_for(&v(&[mime::PNG]), true).contains(&"image/png".to_string()));
        let h = v(&[mime::FILE_LIST, mime::TEXT]);
        assert_eq!(offer_for(&h, true)[..2], GUEST_FILES[..]);
        assert!(!offer_for(&h, false).iter().any(|m| is_file_list(m)));
        assert_eq!(host_types_for("text/uri-list", &h), v(&[mime::FILE_LIST]));
    }

    #[test]
    fn png_to_bmp_and_back() {
        let img = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        let mut png = Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let bmp = convert("image/png", "image/bmp", png.into_inner()).unwrap();
        assert_eq!(&bmp[..2], b"BM");
        let back = convert("image/bmp", "image/png", bmp).unwrap();
        let img2 = image::load_from_memory(&back).unwrap().to_rgba8();
        assert_eq!(img2.get_pixel(2, 1), &image::Rgba([10, 20, 30, 255]));
    }
}
