// SPDX-License-Identifier: GPL-3.0-only

//! The client driver's options (Print Schema), for CUPS and back.
//!
//! `AddPrinter` gives the client driver's PrintCapabilities and default
//! PrintTicket. The guest printer is a CUPS queue whose PPD offers each of
//! the driver's features as an option (paper size, two-sided printing,
//! color, trays, the driver's own features...), with the default ticket's
//! choices as defaults. For each job, the options the user chose become the
//! job's PrintTicket again: the default ticket, its features replaced by
//! the options chosen, the rest (among it the driver's
//! `PageDevmodeSnapshot`) kept as it is.
//!
//! Prefixes may differ between documents: names are compared by namespace
//! URI. The XML of an option is taken from the capabilities as it is, with
//! the capabilities' namespace declarations, so that it means the same in
//! the ticket.

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::reader::Reader;

/// Print Schema Framework.
pub const PSF: &str = "http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework";
/// Print Schema Keywords.
pub const PSK: &str = "http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords";

/// A name with its namespace URI.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QName {
    pub uri: String,
    pub local: String,
}

impl QName {
    fn psk(local: &str) -> Self {
        Self {
            uri: PSK.into(),
            local: local.into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Opt {
    pub name: QName,
    pub display: String,
    /// The option's element, as the capabilities have it.
    pub xml: String,
    /// `psk:MediaSizeWidth` and `MediaSizeHeight`, in microns.
    pub size: Option<(u32, u32)>,
    /// Its PPD keyword.
    pub keyword: String,
}

#[derive(Clone, Debug)]
pub struct Feature {
    pub name: QName,
    pub display: String,
    pub options: Vec<Opt>,
    /// Its PPD keyword.
    pub keyword: String,
    /// The default option (from the default ticket), an index of `options`.
    pub default: usize,
}

/// What the PPD offers and how it maps back to the driver's features.
#[derive(Clone, Debug, Default)]
pub struct Caps {
    /// The capabilities' namespace declarations: `(prefix, uri)`.
    pub ns: Vec<(String, String)>,
    pub features: Vec<Feature>,
    /// The default ticket, as the host gave it.
    pub ticket: String,
    /// The printer's margins, in points: left, top, right, bottom.
    pub margins: [f64; 4],
}

/// The smallest margin: 12 pt (4.2 mm), what most printers need, and the
/// margin when the driver tells none. Many Linux apps (Qt ones among them)
/// print the document from the printer's margins on: with the 0 a PDF
/// printer gives, the text would touch the edge of the page.
const DEFAULT_MARGIN: f64 = 12.0;

/// The integer value of the property `local` (any prefix), in `xml`.
fn number(xml: &str, local: &str) -> Option<i64> {
    let at = xml.find(&format!(":{local}\""))?;
    let rest = &xml[at..];
    let start = rest.find("Value")?;
    let rest = &rest[start..];
    let open = rest.find('>')? + 1;
    let close = rest[open..].find('<')? + open;
    rest[open..close].trim().parse().ok()
}

/// The margins of the driver's `psk:PageImageableSize`, which it gives for
/// its default paper: left, top, right, bottom, in points.
fn margins(caps: &str) -> Option<[f64; 4]> {
    let at = caps.find(":PageImageableSize\"")?;
    let xml = &caps[at..];
    let (w, h) = (
        number(xml, "ImageableSizeWidth")?,
        number(xml, "ImageableSizeHeight")?,
    );
    let (ox, oy) = (number(xml, "OriginWidth")?, number(xml, "OriginHeight")?);
    let (ex, ey) = (number(xml, "ExtentWidth")?, number(xml, "ExtentHeight")?);
    let pt = |um: i64| (um as f64 / 25_400.0 * 72.0).clamp(0.0, 72.0);
    Some([pt(ox), pt(oy), pt(w - ox - ex), pt(h - oy - ey)])
}

/// Namespace scopes while reading.
#[derive(Default)]
struct Scopes(Vec<Vec<(String, String)>>);

impl Scopes {
    fn push(&mut self, e: &quick_xml::events::BytesStart) -> Vec<(String, String)> {
        let mut decls = Vec::new();
        for a in e.attributes().flatten() {
            let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
            let value = a
                .unescape_value()
                .map(|v| v.into_owned())
                .unwrap_or_default();
            if let Some(prefix) = key.strip_prefix("xmlns:") {
                decls.push((prefix.to_string(), value));
            } else if key == "xmlns" {
                decls.push((String::new(), value));
            }
        }
        self.0.push(decls.clone());
        decls
    }

    fn pop(&mut self) {
        self.0.pop();
    }

    fn uri(&self, prefix: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .flat_map(|s| s.iter())
            .find(|(p, _)| p == prefix)
            .map(|(_, u)| u.as_str())
    }

    /// `prefix:local` (an element's or an attribute value's) resolved.
    fn resolve(&self, qname: &str) -> Option<QName> {
        let (prefix, local) = qname.split_once(':').unwrap_or(("", qname));
        Some(QName {
            uri: self.uri(prefix)?.to_string(),
            local: local.to_string(),
        })
    }
}

fn attr(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.as_ref() == name.as_bytes())
        .and_then(|a| a.unescape_value().ok().map(|v| v.into_owned()))
}

fn tag(e: &quick_xml::events::BytesStart) -> String {
    String::from_utf8_lossy(e.name().as_ref()).into_owned()
}

/// A top-level element of a Print Schema document.
#[derive(Debug)]
struct Element {
    /// `psf:Feature`, `psf:ParameterDef`...
    kind: QName,
    /// Its `name` attribute, resolved.
    name: Option<QName>,
    /// Byte range in the document.
    start: usize,
    end: usize,
}

/// An option of a parsed document: `(feature index in the elements,
/// option name, display name, byte range, media size)`.
type OptionAt = (
    usize,
    Option<QName>,
    String,
    (usize, usize),
    Option<(u32, u32)>,
);

/// The `psf:Feature` elements of a capabilities document or a ticket, with
/// their options; the position of the root's end tag; the root's
/// namespace declarations.
struct Parsed {
    ns: Vec<(String, String)>,
    elements: Vec<Element>,
    /// `(feature index in elements, option name, display, range, size)`.
    options: Vec<OptionAt>,
    /// Display names of the top-level features, by element index.
    displays: HashMap<usize, String>,
    root_end: usize,
}

fn parse(xml: &str) -> Result<Parsed, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut scopes = Scopes::default();
    let mut parsed = Parsed {
        ns: Vec::new(),
        elements: Vec::new(),
        options: Vec::new(),
        displays: HashMap::new(),
        root_end: xml.len(),
    };
    // The open elements: (resolved kind, name attribute).
    let mut stack: Vec<(Option<QName>, Option<QName>, usize)> = Vec::new();
    let mut text = String::new();
    // In an option: the display name and size being read.
    let mut opt_display = String::new();
    let mut opt_size: (Option<u32>, Option<u32>) = (None, None);
    let mut feat_display = String::new();
    // The top-level feature being read: options outside one are not its.
    let mut feature: Option<usize> = None;
    loop {
        let before = reader.buffer_position() as usize;
        let ev = reader.read_event().map_err(|e| e.to_string())?;
        let after = reader.buffer_position() as usize;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                let decls = scopes.push(e);
                if stack.is_empty() {
                    parsed.ns = decls;
                }
                let kind = scopes.resolve(&tag(e));
                let name = attr(e, "name").and_then(|n| scopes.resolve(&n));
                let depth = stack.len();
                let is = |k: &Option<QName>, l: &str| {
                    k.as_ref().is_some_and(|k| k.uri == PSF && k.local == l)
                };
                if depth == 1 && is(&kind, "Feature") || depth == 1 && is(&kind, "ParameterInit") {
                    parsed.elements.push(Element {
                        kind: kind.clone().unwrap(),
                        name: name.clone(),
                        start: before,
                        end: after,
                    });
                    feat_display.clear();
                    feature = (is(&kind, "Feature") && !empty).then(|| parsed.elements.len() - 1);
                }
                if depth == 2
                    && is(&kind, "Option")
                    && let Some(f) = feature
                {
                    opt_display.clear();
                    opt_size = (None, None);
                    if empty {
                        parsed.options.push((
                            f,
                            name.clone(),
                            String::new(),
                            (before, after),
                            None,
                        ));
                    }
                }
                text.clear();
                if empty {
                    scopes.pop();
                } else {
                    stack.push((kind, name, before));
                }
            }
            Event::Text(t) => {
                if let Ok(s) = t.decode() {
                    text.push_str(&quick_xml::escape::unescape(&s).unwrap_or(s.clone()));
                }
            }
            Event::CData(t) => text.push_str(&String::from_utf8_lossy(&t)),
            Event::End(_) => {
                let Some((kind, name, start)) = stack.pop() else {
                    break;
                };
                scopes.pop();
                let depth = stack.len();
                let local = kind.as_ref().map(|k| k.local.as_str()).unwrap_or("");
                // A psf:Value: what its parent property means.
                if local == "Value" && depth >= 1 {
                    let parent = &stack[depth - 1].1;
                    let value = text.trim().to_string();
                    match parent.as_ref().map(|p| (p.uri.as_str(), p.local.as_str())) {
                        Some((PSK, "DisplayName")) => {
                            // Value < Property < Option < Feature < root
                            match depth {
                                4 => opt_display = value,
                                3 => feat_display = value,
                                _ => {}
                            }
                        }
                        Some((PSK, "MediaSizeWidth")) if depth == 4 => {
                            opt_size.0 = value.parse().ok()
                        }
                        Some((PSK, "MediaSizeHeight")) if depth == 4 => {
                            opt_size.1 = value.parse().ok()
                        }
                        _ => {}
                    }
                }
                if depth == 2
                    && local == "Option"
                    && let Some(f) = feature
                {
                    let size = match opt_size {
                        (Some(w), Some(h)) => Some((w, h)),
                        _ => None,
                    };
                    parsed.options.push((
                        f,
                        name,
                        std::mem::take(&mut opt_display),
                        (start, after),
                        size,
                    ));
                }
                if depth == 1 {
                    feature = None;
                }
                if depth == 1 && !parsed.elements.is_empty() {
                    let i = parsed.elements.len() - 1;
                    if parsed.elements[i].start == start {
                        parsed.elements[i].end = after;
                        parsed.displays.insert(i, std::mem::take(&mut feat_display));
                    }
                }
                if depth == 0 {
                    parsed.root_end = before;
                }
                text.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(parsed)
}

/// PPD keywords: letters and digits, at most 40 characters.
fn keyword(s: &str) -> String {
    let k: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(40)
        .collect();
    if k.is_empty() { "X".into() } else { k }
}

/// The PPD keyword of a feature: CUPS's own for the standard ones.
fn feature_keyword(name: &QName) -> String {
    if name.uri == PSK {
        match name.local.as_str() {
            "PageMediaSize" => return "PageSize".into(),
            "JobDuplexAllDocumentsContiguously" | "DocumentDuplex" => return "Duplex".into(),
            "PageOutputColor" => return "ColorModel".into(),
            "JobInputBin" | "DocumentInputBin" | "PageInputBin" => return "InputSlot".into(),
            "PageMediaType" => return "MediaType".into(),
            "JobOutputBin" | "DocumentOutputBin" | "PageOutputBin" => return "OutputBin".into(),
            _ => {}
        }
    }
    keyword(&name.local)
}

fn option_keyword(feature: &QName, option: &QName) -> String {
    if option.uri == PSK {
        let std = match (feature.local.as_str(), option.local.as_str()) {
            ("PageMediaSize", "ISOA3") => "A3",
            ("PageMediaSize", "ISOA4") => "A4",
            ("PageMediaSize", "ISOA5") => "A5",
            ("PageMediaSize", "ISOA6") => "A6",
            ("PageMediaSize", "JISB4") => "B4",
            ("PageMediaSize", "JISB5") => "B5",
            ("PageMediaSize", "NorthAmericaLetter") => "Letter",
            ("PageMediaSize", "NorthAmericaLegal") => "Legal",
            ("PageMediaSize", "NorthAmericaExecutive") => "Executive",
            ("PageMediaSize", "NorthAmericaTabloid") => "Tabloid",
            ("PageMediaSize", "NorthAmericaStatement") => "Statement",
            ("PageMediaSize", "ISODLEnvelope") => "EnvDL",
            ("PageMediaSize", "ISOC5Envelope") => "EnvC5",
            ("PageMediaSize", "NorthAmericaNumber10Envelope") => "Env10",
            (_, "OneSided") => "None",
            (_, "TwoSidedLongEdge") => "DuplexNoTumble",
            (_, "TwoSidedShortEdge") => "DuplexTumble",
            ("PageOutputColor", "Color") => "RGB",
            ("PageOutputColor", "Grayscale") => "Gray",
            ("PageOutputColor", "Monochrome") => "KGray",
            _ => "",
        };
        if !std.is_empty() {
            return std.into();
        }
    }
    keyword(&option.local)
}

/// Features left to CUPS: it makes the copies, n-up and orientation in
/// the document itself.
fn left_to_cups(name: &QName) -> bool {
    name.uri == PSK
        && matches!(
            name.local.as_str(),
            "JobCollateAllDocuments"
                | "DocumentCollate"
                | "JobNUpAllDocumentsContiguously"
                | "DocumentNUp"
                | "PageOrientation"
                | "JobCopiesAllDocuments"
                | "DocumentCopiesAllPages"
                | "PageCopies"
                | "DocumentPageRanges"
                | "JobPageOrder"
                | "DocumentPageOrder"
                | "JobPresentationDirection"
                | "DocumentPresentationDirection"
        )
}

/// A driver feature for itself, not for users: its display name is its
/// own name (`JobBigData`: `BigData`, `JobStringsCluster0`: `Strings`) or
/// has `_` in it (`_GeneralEveryday`, `TouchByUser_ID`).
fn internal(name: &QName, display: &str) -> bool {
    if display.contains('_') {
        return true;
    }
    let local = ["Job", "Document", "Page"]
        .iter()
        .find_map(|p| name.local.strip_prefix(p))
        .unwrap_or(&name.local);
    !display.contains(' ') && !display.is_empty() && local.starts_with(display)
}

/// Names for the standard features a driver gives none.
fn standard_display(keyword: &str) -> Option<&'static str> {
    Some(match keyword {
        "PageSize" => "Media Size",
        "Duplex" => "Two-Sided",
        "ColorModel" => "Color Mode",
        "InputSlot" => "Media Source",
        "MediaType" => "Media Type",
        "OutputBin" => "Output Tray",
        "PageOutputQuality" => "Print Quality",
        "PageResolution" => "Resolution",
        "JobStapleAllDocuments" => "Staple",
        "JobHolePunch" => "Hole Punch",
        _ => return None,
    })
}

impl Caps {
    /// The options of `capabilities`, with the defaults of
    /// `default_ticket`. Empty capabilities: generic paper sizes only.
    pub fn new(capabilities: &str, default_ticket: &str) -> Result<Self, String> {
        let mut caps = Caps {
            ticket: default_ticket.to_string(),
            margins: margins(capabilities)
                .unwrap_or_default()
                .map(|m| m.max(DEFAULT_MARGIN)),
            ..Default::default()
        };
        if capabilities.trim().is_empty() {
            return Ok(caps);
        }
        let p = parse(capabilities)?;
        caps.ns = p.ns.clone();
        let mut used: HashMap<String, u32> = HashMap::new();
        for (i, el) in p.elements.iter().enumerate() {
            if el.kind.local != "Feature" {
                continue;
            }
            let Some(name) = el.name.clone() else {
                continue;
            };
            if left_to_cups(&name) {
                continue;
            }
            let mut options: Vec<Opt> = Vec::new();
            for (_, oname, display, (s, e), size) in p.options.iter().filter(|o| o.0 == i) {
                let Some(oname) = oname.clone() else { continue };
                let is_size = name == QName::psk("PageMediaSize");
                if is_size && size.is_none() {
                    continue; // a size the PPD cannot describe
                }
                let mut kw = option_keyword(&name, &oname);
                if options.iter().any(|o| o.keyword == kw) {
                    kw = format!("{kw}{}", options.len());
                }
                options.push(Opt {
                    display: if display.is_empty() {
                        oname.local.clone()
                    } else {
                        display.clone()
                    },
                    name: oname,
                    xml: capabilities[*s..*e].to_string(),
                    size: *size,
                    keyword: kw,
                });
            }
            if options.is_empty() {
                continue;
            }
            let mut kw = feature_keyword(&name);
            let n = used.entry(kw.clone()).or_default();
            *n += 1;
            if *n > 1 {
                kw = format!("{kw}{n}");
            }
            let mut display = p.displays.get(&i).cloned().unwrap_or_default();
            // The driver's own features without a name for users, or
            // without a choice, are its internals (a v4 driver's locale,
            // strings, driver type...).
            let standard = kw != keyword(&name.local) || name.uri == PSK;
            if options.len() < 2
                || display.is_empty() && !standard
                || !standard && internal(&name, &display)
            {
                continue;
            }
            if display.is_empty() || display == name.local {
                display = standard_display(&kw).unwrap_or(&name.local).to_string();
            }
            caps.features.push(Feature {
                display: if display.is_empty() {
                    name.local.clone()
                } else {
                    display
                },
                name,
                options,
                keyword: kw,
                default: 0,
            });
        }
        // The defaults: the default ticket's choices.
        if let Ok(t) = parse(default_ticket) {
            for (i, el) in t.elements.iter().enumerate() {
                let Some(fname) = &el.name else { continue };
                let Some(f) = caps.features.iter_mut().find(|f| &f.name == fname) else {
                    continue;
                };
                if let Some((_, Some(oname), ..)) = t.options.iter().find(|o| o.0 == i)
                    && let Some(d) = f.options.iter().position(|o| &o.name == oname)
                {
                    f.default = d;
                }
            }
        }
        Ok(caps)
    }

    /// The PPD of the queue, in Latin-1 as PPDs are.
    pub fn ppd(&self, name: &str) -> Vec<u8> {
        // PPD lines are at most 255 bytes: names at most 80 characters.
        let clean = |s: &str| -> String {
            s.chars()
                .map(|c| match c {
                    ':' | '"' | '<' | '>' | '/' | '\n' | '\r' => ' ',
                    c if (c as u32) < 0x20 => ' ',
                    c => c,
                })
                .take(80)
                .collect::<String>()
                .trim()
                .to_string()
        };
        let name = clean(name);
        let mut ppd = String::new();
        let mut line = |s: String| {
            ppd.push_str(&s);
            ppd.push('\n');
        };
        for l in [
            "*PPD-Adobe: \"4.3\"",
            "*FormatVersion: \"4.3\"",
            "*FileVersion: \"1.0\"",
            "*LanguageVersion: English",
            "*LanguageEncoding: ISOLatin1",
            "*PCFileName: \"AMELEO.PPD\"",
            "*Manufacturer: \"ameleo\"",
            "*Product: \"(ameleo client printer)\"",
            "*ShortNickName: \"ameleo client printer\"",
            "*PSVersion: \"(3010.000) 0\"",
            "*LanguageLevel: \"3\"",
            "*ColorDevice: True",
            "*DefaultColorSpace: RGB",
            "*FileSystem: False",
            "*Throughput: \"1\"",
            "*LandscapeOrientation: Plus90",
            "*TTRasterizer: Type42",
            // CUPS makes the copies; the backend gets the PDF.
            "*cupsManualCopies: True",
            "*cupsFilter: \"application/vnd.cups-pdf 0 -\"",
        ] {
            line(l.to_string());
        }
        line(format!("*ModelName: \"{name}\""));
        line(format!("*NickName: \"{name}, client printer\""));
        let size = |um: u32| f64::from(um) / 25_400.0 * 72.0;
        let mut have_size = false;
        for f in &self.features {
            let pick = f.options.get(f.default).map_or("", |o| o.keyword.as_str());
            line(format!(
                "*OpenUI *{}/{}: PickOne",
                f.keyword,
                clean(&f.display)
            ));
            line(format!("*Default{}: {pick}", f.keyword));
            for o in &f.options {
                line(format!(
                    "*{} {}/{}: \"\"",
                    f.keyword,
                    o.keyword,
                    clean(&o.display)
                ));
            }
            line(format!("*CloseUI: *{}", f.keyword));
            if f.keyword == "PageSize" {
                have_size = true;
                line("*OpenUI *PageRegion: PickOne".into());
                line(format!("*DefaultPageRegion: {pick}"));
                for o in &f.options {
                    line(format!(
                        "*PageRegion {}/{}: \"\"",
                        o.keyword,
                        clean(&o.display)
                    ));
                }
                line("*CloseUI: *PageRegion".into());
                line(format!("*DefaultImageableArea: {pick}"));
                line(format!("*DefaultPaperDimension: {pick}"));
                for o in &f.options {
                    let (w, h) = o.size.map_or((0.0, 0.0), |(w, h)| (size(w), size(h)));
                    // `llx lly urx ury`, from the bottom left corner.
                    let [l, t, r, b] = self.margins;
                    line(format!(
                        "*ImageableArea {}: \"{l:.2} {b:.2} {:.2} {:.2}\"",
                        o.keyword,
                        (w - r).max(l),
                        (h - t).max(b)
                    ));
                    line(format!("*PaperDimension {}: \"{w:.2} {h:.2}\"", o.keyword));
                }
            }
        }
        if !have_size {
            line("*OpenUI *PageSize/Media Size: PickOne".into());
            line("*DefaultPageSize: A4".into());
            line("*PageSize A4/A4: \"\"".into());
            line("*PageSize Letter/Letter: \"\"".into());
            line("*CloseUI: *PageSize".into());
            line("*OpenUI *PageRegion: PickOne".into());
            line("*DefaultPageRegion: A4".into());
            line("*PageRegion A4/A4: \"\"".into());
            line("*PageRegion Letter/Letter: \"\"".into());
            line("*CloseUI: *PageRegion".into());
            line("*DefaultImageableArea: A4".into());
            let [l, t, r, b] = self.margins;
            for (k, w, h) in [("A4", 595.28, 841.89), ("Letter", 612.0, 792.0)] {
                line(format!(
                    "*ImageableArea {k}: \"{l:.2} {b:.2} {:.2} {:.2}\"",
                    w - r,
                    h - t
                ));
            }
            line("*DefaultPaperDimension: A4".into());
            line("*PaperDimension A4: \"595.28 841.89\"".into());
            line("*PaperDimension Letter: \"612 792\"".into());
        }
        // Latin-1: what cannot be written in it becomes `?`.
        ppd.chars()
            .map(|c| if (c as u32) < 256 { c as u8 } else { b'?' })
            .collect()
    }

    /// The job's PrintTicket: the default ticket, with the features of the
    /// options the job chose (CUPS job options) replaced.
    pub fn ticket(&self, options: &HashMap<String, String>) -> String {
        let mut chosen: Vec<(&Feature, &Opt)> = Vec::new();
        for f in &self.features {
            let mut value = options.get(&f.keyword).cloned();
            // IPP attributes, when CUPS left them as they are.
            if value.is_none() && f.keyword == "Duplex" {
                value = options.get("sides").map(|s| {
                    match s.as_str() {
                        "two-sided-long-edge" => "DuplexNoTumble",
                        "two-sided-short-edge" => "DuplexTumble",
                        _ => "None",
                    }
                    .to_string()
                });
            }
            if value.is_none() && f.keyword == "ColorModel" {
                value = options.get("print-color-mode").and_then(|m| {
                    let want = if m == "monochrome" {
                        ["Gray", "KGray"]
                    } else {
                        ["RGB", "RGB"]
                    };
                    f.options
                        .iter()
                        .find(|o| want.contains(&o.keyword.as_str()))
                        .map(|o| o.keyword.clone())
                });
            }
            if let Some(v) = value
                && let Some(o) = f.options.iter().find(|o| o.keyword == v)
            {
                chosen.push((f, o));
            }
        }
        let mut extra = Vec::new();
        // Landscape: CUPS turns the pages, the driver is told.
        if matches!(
            options.get("orientation-requested").map(String::as_str),
            Some("4" | "5")
        ) || options.get("landscape").is_some_and(|v| v != "false")
        {
            extra.push(self.feature_xml(
                &QName::psk("PageOrientation"),
                "<psf:Option name=\"psk:Landscape\"/>",
                true,
            ));
        }
        let base = if self.ticket.trim().is_empty() {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<psf:PrintTicket xmlns:psf=\"{PSF}\" xmlns:psk=\"{PSK}\" version=\"1\">\n</psf:PrintTicket>\n"
            )
        } else {
            self.ticket.clone()
        };
        let Ok(t) = parse(&base) else {
            return base;
        };
        // Spliced from the end, so that earlier positions hold.
        let mut cuts: Vec<(usize, usize, String)> = Vec::new();
        let mut added = String::new();
        for (f, o) in &chosen {
            let new = self.feature_xml(&f.name, &ticket_option(&o.xml), false);
            match t
                .elements
                .iter()
                .find(|e| e.kind.local == "Feature" && e.name.as_ref() == Some(&f.name))
            {
                Some(e) => cuts.push((e.start, e.end, new)),
                None => added.push_str(&new),
            }
        }
        // PageOrientation: replaced, or added.
        for x in extra {
            match t.elements.iter().find(|e| {
                e.kind.local == "Feature" && e.name.as_ref() == Some(&QName::psk("PageOrientation"))
            }) {
                Some(e) => cuts.push((e.start, e.end, x)),
                None => added.push_str(&x),
            }
        }
        // Copies the backend was left to make.
        if let Some(n) = options
            .get("ameleo-copies")
            .and_then(|n| n.parse::<u32>().ok())
            .filter(|&n| n > 1)
        {
            let name = QName::psk("JobCopiesAllDocuments");
            let x = format!(
                "<psf:ParameterInit xmlns:psf=\"{PSF}\" xmlns:psk=\"{PSK}\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\" name=\"psk:JobCopiesAllDocuments\"><psf:Value xsi:type=\"xsd:integer\">{n}</psf:Value></psf:ParameterInit>"
            );
            match t
                .elements
                .iter()
                .find(|e| e.kind.local == "ParameterInit" && e.name.as_ref() == Some(&name))
            {
                Some(e) => cuts.push((e.start, e.end, x)),
                None => added.push_str(&x),
            }
        }
        if !added.is_empty() {
            cuts.push((t.root_end, t.root_end, added));
        }
        cuts.sort_by_key(|c| std::cmp::Reverse(c.0));
        let mut out = base;
        for (s, e, new) in cuts {
            out.replace_range(s..e, &new);
        }
        out
    }

    /// A `psf:Feature` element for `name` holding `option`, with the
    /// declarations its names need. `own`: the option uses `psf:` and
    /// `psk:` only.
    fn feature_xml(&self, name: &QName, option: &str, own: bool) -> String {
        let mut ns: Vec<(String, String)> = if own {
            vec![]
        } else {
            self.ns
                .iter()
                .filter(|(p, _)| !p.is_empty())
                .cloned()
                .collect()
        };
        for (p, u) in [("psf", PSF), ("psk", PSK)] {
            if !ns.iter().any(|(q, _)| q == p) {
                ns.push((p.into(), u.into()));
            }
        }
        // The prefix of the feature's namespace, declared above or new.
        let prefix = match ns.iter().find(|(_, u)| *u == name.uri) {
            Some((p, _)) => p.clone(),
            None => {
                ns.push(("ameleo".into(), name.uri.clone()));
                "ameleo".into()
            }
        };
        let psf = ns
            .iter()
            .find(|(_, u)| u == PSF)
            .map(|(p, _)| p.clone())
            .unwrap_or("psf".into());
        let decls: String = ns
            .iter()
            .map(|(p, u)| format!(" xmlns:{p}=\"{}\"", quick_xml::escape::escape(u)))
            .collect();
        format!(
            "<{psf}:Feature{decls} name=\"{prefix}:{}\">{option}</{psf}:Feature>",
            name.local
        )
    }
}

/// An option of the capabilities as a ticket has it: without what only
/// capabilities say (`constrained`, display names). A driver refuses a
/// ticket with them: Microsoft Print to PDF wrote an empty file.
fn ticket_option(xml: &str) -> String {
    use quick_xml::events::BytesStart;
    use quick_xml::writer::Writer;
    let local =
        |name: &[u8]| -> Vec<u8> { name.rsplit(|&b| b == b':').next().unwrap_or(name).to_vec() };
    let clean = |e: &BytesStart| -> BytesStart<'static> {
        let mut out = BytesStart::new(String::from_utf8_lossy(e.name().as_ref()).into_owned());
        for a in e.attributes().flatten() {
            if local(a.key.as_ref()) != b"constrained" {
                out.push_attribute(a);
            }
        }
        out
    };
    // A psf:Property for display: psk:DisplayName.
    let display = |e: &BytesStart| {
        local(e.name().as_ref()) == b"Property"
            && attr(e, "name").is_some_and(|n| n.ends_with(":DisplayName"))
    };
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut w = Writer::new(Vec::new());
    let mut skip = 0usize;
    loop {
        let ev = match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(ev) => ev,
        };
        let res = match ev {
            Event::Start(e) => {
                if skip > 0 || display(&e) {
                    skip += 1;
                    continue;
                }
                w.write_event(Event::Start(clean(&e)))
            }
            Event::Empty(e) => {
                if skip > 0 || display(&e) {
                    continue;
                }
                w.write_event(Event::Empty(clean(&e)))
            }
            Event::End(e) => {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                w.write_event(Event::End(e))
            }
            ev if skip > 0 => {
                let _ = ev;
                continue;
            }
            ev => w.write_event(ev),
        };
        if res.is_err() {
            return xml.to_string();
        }
    }
    String::from_utf8(w.into_inner()).unwrap_or_else(|_| xml.to_string())
}

/// CUPS job options (`argv[5]` of a backend): `name=value` and `name`
/// (true), separated by spaces, with quotes and backslashes.
pub fn job_options(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut word = String::new();
        let mut quote: Option<char> = None;
        while let Some(c) = chars.next() {
            match (c, quote) {
                ('\\', _) => {
                    if let Some(n) = chars.next() {
                        word.push(n);
                    }
                }
                ('"' | '\'', None) => quote = Some(c),
                (c, Some(q)) if c == q => quote = None,
                (c, None) if c.is_whitespace() => break,
                (c, _) => word.push(c),
            }
        }
        match word.split_once('=') {
            Some((k, v)) => out.insert(k.to_string(), v.to_string()),
            None => match word.strip_prefix("no") {
                Some(k) if !k.is_empty() => out.insert(k.to_string(), "false".into()),
                _ => out.insert(word, "true".into()),
            },
        };
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A client driver's capabilities, shortened, with the prefixes a
    /// real driver uses and a feature of its own.
    pub const CAPS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<psf:PrintCapabilities xmlns:psf="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:psk="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:ns0000="http://schemas.hp.com/ptpc/2006/1" version="1">
  <psf:Feature name="psk:PageMediaSize">
    <psf:Property name="psf:SelectionType"><psf:Value xsi:type="xsd:QName">psk:PickOne</psf:Value></psf:Property>
    <psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">Taille du papier</psf:Value></psf:Property>
    <psf:Option name="psk:ISOA4" constrained="psk:None">
      <psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">A4</psf:Value></psf:Property>
      <psf:ScoredProperty name="psk:MediaSizeWidth"><psf:Value xsi:type="xsd:integer">210000</psf:Value></psf:ScoredProperty>
      <psf:ScoredProperty name="psk:MediaSizeHeight"><psf:Value xsi:type="xsd:integer">297000</psf:Value></psf:ScoredProperty>
    </psf:Option>
    <psf:Option name="psk:NorthAmericaLetter">
      <psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">Letter</psf:Value></psf:Property>
      <psf:ScoredProperty name="psk:MediaSizeWidth"><psf:Value xsi:type="xsd:integer">215900</psf:Value></psf:ScoredProperty>
      <psf:ScoredProperty name="psk:MediaSizeHeight"><psf:Value xsi:type="xsd:integer">279400</psf:Value></psf:ScoredProperty>
    </psf:Option>
  </psf:Feature>
  <psf:Feature name="psk:JobDuplexAllDocumentsContiguously">
    <psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">Impression recto verso</psf:Value></psf:Property>
    <psf:Option name="psk:OneSided"><psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">Recto</psf:Value></psf:Property></psf:Option>
    <psf:Option name="psk:TwoSidedLongEdge"><psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">Recto verso</psf:Value></psf:Property></psf:Option>
  </psf:Feature>
  <psf:Feature name="psk:PageOrientation">
    <psf:Option name="psk:Portrait"/>
    <psf:Option name="psk:Landscape"/>
  </psf:Feature>
  <psf:Feature name="ns0000:EconoMode">
    <psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">Mode économique</psf:Value></psf:Property>
    <psf:Option name="ns0000:Off"><psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">Désactivé</psf:Value></psf:Property></psf:Option>
    <psf:Option name="ns0000:On"><psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">Activé</psf:Value></psf:Property></psf:Option>
  </psf:Feature>
  <psf:Feature name="ns0000:JobLocale">
    <psf:Option name="ns0000:fr-FR"/>
    <psf:Option name="ns0000:en-US"/>
  </psf:Feature>
  <psf:Feature name="ns0000:JobV4Driver">
    <psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">V4</psf:Value></psf:Property>
    <psf:Option name="ns0000:Yes"/>
  </psf:Feature>
  <psf:Feature name="ns0000:LongNames">
    <psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">LONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAME</psf:Value></psf:Property>
    <psf:Option name="ns0000:AVeryLongOptionNameThatGoesOnAndOnAndOnAndOnAndOnForeverAndEver"><psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">LONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAMELONGNAME</psf:Value></psf:Property></psf:Option>
    <psf:Option name="ns0000:Short"/>
  </psf:Feature>
  <psf:Feature name="ns0000:JobBigData">
    <psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">BigData</psf:Value></psf:Property>
    <psf:Option name="ns0000:ON"/><psf:Option name="ns0000:OFF"/>
  </psf:Feature>
  <psf:Feature name="ns0000:JobGeneralEveryday">
    <psf:Property name="psk:DisplayName"><psf:Value xsi:type="xsd:string">_GeneralEveryday</psf:Value></psf:Property>
    <psf:Option name="ns0000:A"/><psf:Option name="ns0000:B"/>
  </psf:Feature>
  <psf:Feature name="psk:PageOutputColor">
    <psf:Option name="psk:Color"/><psf:Option name="psk:Monochrome"/>
  </psf:Feature>
  <psf:Feature name="psk:DocumentPageRanges">
    <psf:Option name="psk:AllPages"/><psf:Option name="psk:OddPages"/>
  </psf:Feature>
  <psf:Property name="psk:PageImageableSize">
    <psf:Property name="psk:ImageableSizeWidth"><psf:Value xsi:type="xsd:integer">215900</psf:Value></psf:Property>
    <psf:Property name="psk:ImageableSizeHeight"><psf:Value xsi:type="xsd:integer">279400</psf:Value></psf:Property>
    <psf:Property name="psk:ImageableArea">
      <psf:Property name="psk:OriginWidth"><psf:Value xsi:type="xsd:integer">6350</psf:Value></psf:Property>
      <psf:Property name="psk:OriginHeight"><psf:Value xsi:type="xsd:integer">4233</psf:Value></psf:Property>
      <psf:Property name="psk:ExtentWidth"><psf:Value xsi:type="xsd:integer">203200</psf:Value></psf:Property>
      <psf:Property name="psk:ExtentHeight"><psf:Value xsi:type="xsd:integer">270934</psf:Value></psf:Property>
    </psf:Property>
  </psf:Property>
  <psf:ParameterDef name="psk:JobCopiesAllDocuments"/>
</psf:PrintCapabilities>"#;

    /// Its default ticket, with other prefixes and a DEVMODE snapshot.
    pub const TICKET: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<a:PrintTicket xmlns:a="http://schemas.microsoft.com/windows/2003/08/printing/printschemaframework" xmlns:b="http://schemas.microsoft.com/windows/2003/08/printing/printschemakeywords" xmlns:hp="http://schemas.hp.com/ptpc/2006/1" xmlns:xsd="http://www.w3.org/2001/XMLSchema" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xmlns:oem="http://schemas.microsoft.com/windows/printing/oemdriverpt/x" version="1">
  <a:Feature name="b:PageMediaSize"><a:Option name="b:NorthAmericaLetter"/></a:Feature>
  <a:Feature name="b:JobDuplexAllDocumentsContiguously"><a:Option name="b:OneSided"/></a:Feature>
  <a:Feature name="hp:EconoMode"><a:Option name="hp:On"/></a:Feature>
  <a:ParameterInit name="oem:PageDevmodeSnapshot"><a:Value xsi:type="xsd:string">QUJD</a:Value></a:ParameterInit>
</a:PrintTicket>"#;

    #[test]
    fn features_from_capabilities() {
        let caps = Caps::new(CAPS, TICKET).unwrap();
        let k: Vec<_> = caps.features.iter().map(|f| f.keyword.as_str()).collect();
        // Orientation is left to CUPS; the driver's internals left out.
        assert_eq!(
            k,
            ["PageSize", "Duplex", "EconoMode", "LongNames", "ColorModel"]
        );
        // A standard feature without a name gets one.
        assert_eq!(caps.features[4].display, "Color Mode");
        let size = &caps.features[0];
        assert_eq!(size.display, "Taille du papier");
        assert_eq!(size.options[0].keyword, "A4");
        assert_eq!(size.options[0].size, Some((210_000, 297_000)));
        // Defaults from the ticket, whatever its prefixes.
        assert_eq!(size.default, 1);
        assert_eq!(caps.features[2].default, 1);
        assert_eq!(caps.features[2].options[1].display, "Activé");
    }

    #[test]
    fn ppd_for_cups() {
        let caps = Caps::new(CAPS, TICKET).unwrap();
        let ppd = caps.ppd("HP LaserJet: bureau");
        let text: String = ppd.iter().map(|&b| b as char).collect();
        assert!(text.contains("*DefaultPageSize: Letter\n"));
        assert!(text.contains("*PaperDimension A4: \"595.28 841.89\"\n"));
        // The driver's margins: 6.35 mm left and right, 4.233 mm top and bottom.
        assert!(
            text.contains("*ImageableArea A4: \"18.00 12.00 577.28 829.89\"\n"),
            "{text}"
        );
        // A printer without margins gets the smallest ones.
        let pdf = CAPS
            .replace(">6350<", ">0<")
            .replace(">203200<", ">215900<");
        let pdf_text: String = Caps::new(&pdf, TICKET)
            .unwrap()
            .ppd("PDF")
            .iter()
            .map(|&b| b as char)
            .collect();
        assert!(
            pdf_text.contains("*ImageableArea A4: \"12.00 12.00 583.28 829.89\"\n"),
            "{pdf_text}"
        );
        assert!(text.contains("*Duplex DuplexNoTumble/Recto verso: \"\"\n"));
        assert!(text.contains("*OpenUI *EconoMode/Mode économique: PickOne\n"));
        assert!(text.contains("*ModelName: \"HP LaserJet  bureau\"\n"));
        // Latin-1.
        assert!(ppd.contains(&0xe9));
        assert!(text.lines().all(|l| l.len() <= 255));
        if let Ok(path) = std::env::var("AMELEO_PPD_OUT") {
            std::fs::write(path, &ppd).unwrap();
        }
    }

    #[test]
    fn ticket_for_a_job() {
        let caps = Caps::new(CAPS, TICKET).unwrap();
        let opts = job_options(
            "PageSize=A4 sides=two-sided-long-edge EconoMode=Off orientation-requested=4 job-uuid=x",
        );
        let t = caps.ticket(&opts);
        // The snapshot and the root stay.
        assert!(t.contains("<a:ParameterInit name=\"oem:PageDevmodeSnapshot\">"));
        assert!(t.starts_with("<?xml"));
        assert!(!t.contains("b:NorthAmericaLetter"));
        assert!(t.contains("<psf:Option name=\"psk:ISOA4\">"));
        if std::env::var_os("AMELEO_SHOW_TICKET").is_some() {
            eprintln!("{t}");
        }
        // Nothing that only capabilities say: drivers refuse the ticket.
        assert!(!t.contains("constrained"), "{t}");
        assert!(!t.contains("DisplayName"), "{t}");
        assert!(t.contains("<psf:Value xsi:type=\"xsd:integer\">210000</psf:Value>"));
        assert!(t.contains("name=\"psk:TwoSidedLongEdge\""));
        assert!(t.contains("name=\"ns0000:Off\""));
        assert!(t.contains("name=\"psk:Landscape\""));
        // What was chosen means the same once parsed again.
        let again = Caps::new(CAPS, &t).unwrap();
        let defaults: Vec<_> = again
            .features
            .iter()
            .map(|f| f.options[f.default].keyword.as_str())
            .collect();
        assert_eq!(
            defaults,
            [
                "A4",
                "DuplexNoTumble",
                "Off",
                "AVeryLongOptionNameThatGoesOnAndOnAndOnA",
                "RGB"
            ]
        );
        // Nothing chosen: the default ticket as it is.
        assert_eq!(caps.ticket(&HashMap::new()), TICKET);
    }

    #[test]
    fn no_capabilities() {
        let caps = Caps::new("", "").unwrap();
        let text: String = caps.ppd("P").iter().map(|&b| b as char).collect();
        assert!(text.contains("*DefaultPageSize: A4"));
        assert!(text.contains("*ImageableArea A4: \"12.00 12.00 583.28 829.89\"\n"));
        let t = caps.ticket(&job_options("landscape"));
        assert!(t.contains("psk:Landscape"));
        assert!(parse(&t).is_ok());
    }

    #[test]
    fn options_outside_features() {
        // An option under a property, first and after a feature: neither
        // panics nor joins the feature.
        let caps = CAPS.replace(
            "<psf:Feature name=\"psk:PageMediaSize\">",
            "<psf:Property name=\"psk:Odd\"><psf:Option name=\"psk:Stray\"/></psf:Property><psf:Feature name=\"psk:PageMediaSize\">",
        );
        let caps = caps.replace(
            "<psf:ParameterDef",
            "<psf:Property name=\"psk:Odd2\"><psf:Option name=\"psk:Stray2\"/></psf:Property><psf:ParameterDef",
        );
        let c = Caps::new(&caps, TICKET).unwrap();
        for f in &c.features {
            assert!(
                f.options.iter().all(|o| !o.name.local.starts_with("Stray")),
                "{f:?}"
            );
        }
        // Copies left to the backend go to the driver.
        let t = c.ticket(&job_options("ameleo-copies=3"));
        assert!(t.contains(
            "name=\"psk:JobCopiesAllDocuments\"><psf:Value xsi:type=\"xsd:integer\">3</psf:Value>"
        ));
        assert!(parse(&t).is_ok());
        // nolandscape is portrait.
        let t = c.ticket(&job_options("nolandscape"));
        assert!(!t.contains("Landscape"));
    }

    #[test]
    fn options_parsed() {
        let o = job_options(r#"a=1 b="x y" c=it\'s nofit d"#);
        assert_eq!(o["a"], "1");
        assert_eq!(o["b"], "x y");
        assert_eq!(o["c"], "it's");
        assert_eq!(o["fit"], "false");
        assert_eq!(o["d"], "true");
    }
}
