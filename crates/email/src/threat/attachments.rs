// Copyright (c) 2026 Tyler Martin
// Licensed under FSL-1.1-ALv2 (see LICENSE)

//! Attachment analyzer: executable and script extensions, double
//! extensions (`invoice.pdf.exe`), executables disguised under a document
//! name, macro-enabled Office files, HTML/SVG attachments, and archives
//! (members inspected for ZIP; encrypted or uninspectable archives flagged).
//!
//! Malware-grade signals set [`Signal::malware`], which tags the message
//! `threat:malware` and makes every download chokepoint refuse its bytes.
//! [`gate`] runs the same checks on one attachment at a chokepoint, so bytes
//! are refused even when the message was never scanned.
//!
//! Evidence is `ext=... sha256=<first 16 hex>`: no filenames, no content.

use std::io::Cursor;

use sha2::{Digest, Sha256};

use super::{AttachmentInput, Signal, ThreatInput};

pub const DOUBLE_EXTENSION: u32 = 70;
pub const EXECUTABLE_DISGUISED: u32 = 70;
pub const DANGEROUS_EXTENSION: u32 = 60;
pub const ARCHIVE_DANGEROUS_MEMBER: u32 = 60;
pub const MACRO_OFFICE: u32 = 50;
pub const ENCRYPTED_ARCHIVE: u32 = 25;
pub const HTML_ATTACHMENT: u32 = 25;
pub const UNINSPECTABLE_ARCHIVE: u32 = 10;

/// Members inspected per ZIP before giving up (zip bombs).
const MAX_ZIP_MEMBERS: usize = 2000;

pub const DANGEROUS_EXTENSIONS: &[&str] = &[
    "exe",
    "scr",
    "com",
    "pif",
    "bat",
    "cmd",
    "vbs",
    "vbe",
    "js",
    "jse",
    "wsf",
    "wsh",
    "hta",
    "cpl",
    "msi",
    "msp",
    "jar",
    "ps1",
    "psm1",
    "lnk",
    "reg",
    "scf",
    "iso",
    "img",
    "vhd",
    "vhdx",
    "appx",
    "appxbundle",
    "msix",
    "dll",
    "application",
    "gadget",
    "sct",
    "chm",
    "inf",
    "url",
];
const MACRO_EXTENSIONS: &[&str] = &[
    "docm", "dotm", "xlsm", "xltm", "xlam", "pptm", "potm", "ppsm", "sldm",
];
const DOCUMENT_EXTENSIONS: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "rtf", "jpg", "jpeg", "png", "gif",
    "csv", "odt", "zip",
];
const ARCHIVE_EXTENSIONS: &[&str] = &[
    "zip", "rar", "7z", "gz", "tgz", "tar", "cab", "ace", "arj", "xz", "bz2",
];
const HTML_EXTENSIONS: &[&str] = &["html", "htm", "shtml", "xhtml", "svg", "mht", "mhtml"];

const OLE_MAGIC: &[u8] = &[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Lowercased extensions, last first, trailing spaces/dots ignored:
/// `"Invoice.PDF  .exe"` → `["exe", "pdf"]`.
fn extensions(filename: &str) -> Vec<String> {
    let name = filename.trim().trim_end_matches(['.', ' ']).to_lowercase();
    let parts: Vec<&str> = name.split('.').collect();
    if parts.len() < 2 {
        return Vec::new();
    }
    parts[1..]
        .iter()
        .rev()
        .map(|p| p.trim().to_string())
        .collect()
}

fn hash_suffix(bytes: Option<&[u8]>) -> String {
    bytes
        .map(|b| {
            let hex: String = Sha256::digest(b)
                .iter()
                .take(8)
                .map(|x| format!("{x:02x}"))
                .collect();
            format!(" sha256={hex}")
        })
        .unwrap_or_default()
}

/// `ext=.<last> sha256=<first 16 hex>`: the evidence form for one attachment.
pub(crate) fn fingerprint(att: &AttachmentInput) -> String {
    let last = extensions(&att.filename)
        .first()
        .cloned()
        .unwrap_or_default();
    format!(
        "ext=.{}{}",
        if last.is_empty() { "none" } else { &last },
        hash_suffix(att.bytes.as_deref())
    )
}

fn ooxml_has_macros(bytes: &[u8]) -> bool {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return false;
    };
    (0..zip.len().min(MAX_ZIP_MEMBERS)).any(|i| {
        zip.by_index_raw(i)
            .map(|f| f.name().to_lowercase().ends_with("vbaproject.bin"))
            .unwrap_or(false)
    })
}

fn ole_has_macros(bytes: &[u8]) -> bool {
    if !bytes.starts_with(OLE_MAGIC) {
        return false;
    }
    let utf16_vba: Vec<u8> = "_VBA_PROJECT"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let find = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    find(&utf16_vba) || find(b"_VBA_PROJECT")
}

/// What an inspected ZIP contained.
enum ZipFinding {
    Clean,
    DangerousMember(String),
    Encrypted,
    Unreadable,
}

fn inspect_zip(bytes: &[u8]) -> ZipFinding {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return ZipFinding::Unreadable;
    };
    let mut encrypted = false;
    for i in 0..zip.len().min(MAX_ZIP_MEMBERS) {
        let Ok(member) = zip.by_index_raw(i) else {
            return ZipFinding::Unreadable;
        };
        if member.encrypted() {
            encrypted = true;
        }
        let exts = extensions(member.name());
        if let Some(ext) = exts.first()
            && (DANGEROUS_EXTENSIONS.contains(&ext.as_str())
                || MACRO_EXTENSIONS.contains(&ext.as_str()))
        {
            return ZipFinding::DangerousMember(ext.clone());
        }
    }
    if zip.len() > MAX_ZIP_MEMBERS {
        return ZipFinding::Unreadable;
    }
    if encrypted {
        ZipFinding::Encrypted
    } else {
        ZipFinding::Clean
    }
}

/// Signals for one attachment.
pub fn analyze_attachment(att: &AttachmentInput) -> Vec<Signal> {
    let exts = extensions(&att.filename);
    let last = exts.first().cloned().unwrap_or_default();
    let bytes = att.bytes.as_deref();
    let hash = hash_suffix(bytes);
    let fp = fingerprint(att);
    let mut signals = Vec::new();

    if DANGEROUS_EXTENSIONS.contains(&last.as_str()) {
        let doubled = exts
            .get(1)
            .is_some_and(|prev| DOCUMENT_EXTENSIONS.contains(&prev.as_str()));
        if doubled {
            signals.push(
                Signal::new(
                    "double_extension",
                    DOUBLE_EXTENSION,
                    format!("ext=.{}.{last}{hash}", exts[1]),
                )
                .malware(),
            );
        } else {
            signals.push(
                Signal::new("dangerous_extension", DANGEROUS_EXTENSION, fp.clone()).malware(),
            );
        }
        return signals;
    }

    if let Some(bytes) = bytes
        && bytes.starts_with(b"MZ")
        && last != "exe"
        && last != "dll"
    {
        signals
            .push(Signal::new("executable_disguised", EXECUTABLE_DISGUISED, fp.clone()).malware());
        return signals;
    }

    let macro_by_name = MACRO_EXTENSIONS.contains(&last.as_str());
    let macro_by_bytes =
        bytes.is_some_and(|b| ole_has_macros(b) || (b.starts_with(b"PK") && ooxml_has_macros(b)));
    if macro_by_name || macro_by_bytes {
        signals.push(Signal::new("macro_office", MACRO_OFFICE, fp.clone()).malware());
        return signals;
    }

    if HTML_EXTENSIONS.contains(&last.as_str()) || att.content_type == "text/html" {
        signals.push(Signal::new("html_attachment", HTML_ATTACHMENT, fp.clone()));
        return signals;
    }

    let is_archive =
        ARCHIVE_EXTENSIONS.contains(&last.as_str()) || att.content_type.contains("zip");
    if is_archive {
        let zip_bytes = bytes.filter(|b| b.starts_with(b"PK"));
        match zip_bytes.map(inspect_zip) {
            Some(ZipFinding::DangerousMember(member_ext)) => signals.push(
                Signal::new(
                    "archive_dangerous_member",
                    ARCHIVE_DANGEROUS_MEMBER,
                    format!("member ext=.{member_ext}; archive {fp}"),
                )
                .malware(),
            ),
            Some(ZipFinding::Encrypted) => {
                signals.push(Signal::new(
                    "encrypted_archive",
                    ENCRYPTED_ARCHIVE,
                    fp.clone(),
                ));
            }
            Some(ZipFinding::Clean) => {}
            Some(ZipFinding::Unreadable) | None => {
                signals.push(Signal::new(
                    "uninspectable_archive",
                    UNINSPECTABLE_ARCHIVE,
                    fp.clone(),
                ));
            }
        }
    }
    signals
}

pub fn analyze(input: &ThreatInput) -> Vec<Signal> {
    input
        .attachments
        .iter()
        .flat_map(analyze_attachment)
        .collect()
}

/// Malware-grade signals for one attachment at a download or upload
/// chokepoint. Empty means the bytes may pass.
pub fn gate(filename: &str, content_type: &str, bytes: &[u8]) -> Vec<Signal> {
    analyze_attachment(&AttachmentInput {
        filename: filename.to_string(),
        content_type: content_type.to_lowercase(),
        size: bytes.len() as u64,
        bytes: Some(bytes.to_vec()),
    })
    .into_iter()
    .filter(|s| s.malware)
    .collect()
}

/// Build a stored ZIP fixture, optionally with the encryption bit set.
#[cfg(test)]
fn zip_with(members: &[(&str, &[u8])], encrypted_names: bool) -> Vec<u8> {
    use std::io::Write;
    let mut buf = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in members {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap();
    }
    let mut bytes = buf.into_inner();
    if encrypted_names {
        // Flip the "encrypted" general-purpose bit on every header.
        for sig in [&b"PK\x03\x04"[..], &b"PK\x01\x02"[..]] {
            let offset = if sig == b"PK\x03\x04" { 6 } else { 8 };
            let mut i = 0;
            while i + 4 <= bytes.len() {
                if &bytes[i..i + 4] == sig {
                    bytes[i + offset] |= 1;
                }
                i += 1;
            }
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::super::test_support::codes;
    use super::*;

    fn att(name: &str, ct: &str, bytes: &[u8]) -> AttachmentInput {
        AttachmentInput {
            filename: name.to_string(),
            content_type: ct.to_string(),
            size: bytes.len() as u64,
            bytes: Some(bytes.to_vec()),
        }
    }

    #[test]
    fn dangerous_and_double_extensions_are_malware() {
        let s = analyze_attachment(&att("setup.exe", "application/octet-stream", b"MZ\x90"));
        assert_eq!(codes(&s), vec!["dangerous_extension"]);
        assert!(s[0].malware);
        assert!(s[0].evidence.starts_with("ext=.exe sha256="));

        let s = analyze_attachment(&att("Invoice.PDF   .exe", "application/pdf", b"MZ"));
        assert_eq!(codes(&s), vec!["double_extension"]);
        assert!(s[0].evidence.starts_with("ext=.pdf.exe sha256="));
        assert!(s[0].malware);

        let s = analyze_attachment(&att("payload.js", "text/plain", b"var x"));
        assert_eq!(codes(&s), vec!["dangerous_extension"]);
    }

    #[test]
    fn executable_under_a_document_name_is_malware() {
        let s = analyze_attachment(&att("statement.pdf", "application/pdf", b"MZ\x90\x00\x03"));
        assert_eq!(codes(&s), vec!["executable_disguised"]);
        assert!(s[0].malware);
    }

    #[test]
    fn macro_office_by_extension_ooxml_and_ole() {
        let s = analyze_attachment(&att("budget.xlsm", "application/vnd.ms-excel", b"PK"));
        assert_eq!(codes(&s), vec!["macro_office"]);

        let docx = zip_with(
            &[
                ("word/document.xml", b"<w/>"),
                ("word/vbaProject.bin", b"\x00"),
            ],
            false,
        );
        let s = analyze_attachment(&att("report.docx", "application/vnd.openxmlformats", &docx));
        assert_eq!(codes(&s), vec!["macro_office"]);

        let mut ole = OLE_MAGIC.to_vec();
        ole.extend_from_slice(&[0; 64]);
        ole.extend("_VBA_PROJECT".encode_utf16().flat_map(u16::to_le_bytes));
        let s = analyze_attachment(&att("old.doc", "application/msword", &ole));
        assert_eq!(codes(&s), vec!["macro_office"]);
        assert!(s[0].malware);

        let clean_docx = zip_with(&[("word/document.xml", b"<w/>")], false);
        assert!(
            analyze_attachment(&att(
                "ok.docx",
                "application/vnd.openxmlformats",
                &clean_docx
            ))
            .is_empty()
        );
    }

    #[test]
    fn archives_are_inspected() {
        let bad = zip_with(&[("readme.txt", b"hi"), ("invoice.pdf.scr", b"MZ")], false);
        let s = analyze_attachment(&att("docs.zip", "application/zip", &bad));
        assert_eq!(codes(&s), vec!["archive_dangerous_member"]);
        assert!(s[0].malware);
        assert!(s[0].evidence.starts_with("member ext=.scr"));

        let locked = zip_with(&[("notes.txt", b"hi")], true);
        let s = analyze_attachment(&att("locked.zip", "application/zip", &locked));
        assert_eq!(codes(&s), vec!["encrypted_archive"]);
        assert!(!s[0].malware);

        let s = analyze_attachment(&att("bundle.rar", "application/x-rar", b"Rar!\x1a\x07"));
        assert_eq!(codes(&s), vec!["uninspectable_archive"]);

        let fine = zip_with(&[("photo.jpg", b"\xff\xd8")], false);
        assert!(analyze_attachment(&att("photos.zip", "application/zip", &fine)).is_empty());
    }

    #[test]
    fn html_attachments_are_suspicious_not_malware() {
        let s = analyze_attachment(&att("Secure-Message.html", "text/html", b"<form>"));
        assert_eq!(codes(&s), vec!["html_attachment"]);
        assert!(!s[0].malware);
    }

    #[test]
    fn metadata_only_attachments_still_check_names() {
        let s = analyze_attachment(&AttachmentInput {
            filename: "run.bat".to_string(),
            content_type: "application/octet-stream".to_string(),
            size: 10,
            bytes: None,
        });
        assert_eq!(codes(&s), vec!["dangerous_extension"]);
        assert_eq!(s[0].evidence, "ext=.bat");
    }

    #[test]
    fn gate_returns_only_malware_grade_signals() {
        assert!(gate("page.html", "text/html", b"<p>").is_empty());
        assert!(gate("report.pdf", "application/pdf", b"%PDF-1.7").is_empty());
        assert_eq!(
            codes(&gate("x.pdf.exe", "application/pdf", b"MZ")),
            vec!["double_extension"]
        );
    }
}
