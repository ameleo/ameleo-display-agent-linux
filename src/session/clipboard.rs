// SPDX-License-Identifier: GPL-3.0-or-later

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
const GUEST_IMAGES: &[&str] = &[
    "image/png",
    "image/bmp",
    "image/x-bmp",
    "image/jpeg",
];

pub fn is_text(m: &str) -> bool {
    m.eq_ignore_ascii_case(mime::TEXT)
        || GUEST_TEXT.iter().any(|t| t.eq_ignore_ascii_case(m))
}

fn is_image(m: &str) -> bool {
    m == mime::PNG || m == mime::BMP
}

/// The wire types the guest clipboard can be exported as, from what the
/// guest app offers.
pub fn exportable(guest: &[String]) -> Vec<String> {
    let mut out = Vec::new();
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
    if wanted == mime::PNG || wanted == mime::BMP {
        // The type asked for as it is, else any image we can convert.
        if guest.iter().any(|g| g == wanted) {
            return Some(wanted.to_string());
        }
        return pick(GUEST_IMAGES);
    }
    None
}

/// The guest types to offer for a host grab of `host` types.
pub fn offer_for(host: &[String]) -> Vec<String> {
    let mut out = Vec::new();
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
        assert_eq!(
            exportable(&v(&["image/jpeg"])),
            v(&[mime::PNG, mime::BMP])
        );
        assert!(exportable(&v(&["text/uri-list", "x-special/gnome-copied-files"])).is_empty());
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
        assert!(offer_for(&v(&[mime::PNG])).contains(&"image/png".to_string()));
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
