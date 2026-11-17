//! Upload preflight. The relay remains authoritative for content validation.
use crate::error::CliError;

// Mirrored from crates/buzz-media/src/validation.rs (BLOCKED_FILE_MIME_TYPES,
// looks_like_iso_bmff, validate_file_content). Importing buzz-media would bring
// S3, image codecs and video processing into the standalone CLI.
const BLOCKED_FILE_MIME_TYPES: &[&str] = &[
    // Active web content — stored-XSS vectors.
    "application/xhtml+xml",
    "image/svg+xml",
    "application/javascript",
    "text/javascript",
    // Native executables / installers.
    "application/x-msdownload", // .exe / .dll
    "application/x-executable", // ELF
    "application/vnd.microsoft.portable-executable",
    "application/x-mach-binary", // Mach-O
    "application/x-sharedlib",
    "application/x-elf",
    "application/x-msi",
    "application/vnd.android.package-archive", // .apk
    "application/x-apple-diskimage",           // .dmg
];

fn iso_bmff_ftyp_payload(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 16 || &bytes[4..8] != b"ftyp" {
        return None;
    }
    let compact = u32::from_be_bytes(bytes[..4].try_into().ok()?) as u64;
    let (declared_size, header_size) = if compact == 1 {
        if bytes.len() < 24 {
            return None;
        }
        (u64::from_be_bytes(bytes[8..16].try_into().ok()?), 16usize)
    } else if compact == 0 {
        (bytes.len() as u64, 8usize)
    } else {
        (compact, 8usize)
    };
    if declared_size < (header_size + 8) as u64 {
        return None;
    }
    let available_end = usize::try_from(declared_size)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    (available_end >= header_size + 8).then_some(&bytes[header_size..available_end])
}

/// Return whether the leading bytes contain a structurally valid ISO-BMFF
/// `ftyp` box, independent of the request MIME type or `infer`'s brand list.
fn looks_like_iso_bmff(bytes: &[u8]) -> bool {
    iso_bmff_ftyp_payload(bytes).is_some()
}

const MAX_IMAGE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 500 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 100 * 1024 * 1024;

pub(crate) fn validate_filename(path: &str) -> Result<(), CliError> {
    // infer 0.19 does not sniff SVG. Keep .svg uploads rejected even when their
    // bytes would otherwise become text/xml or application/octet-stream.
    if std::path::Path::new(path)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
    {
        return Err(CliError::Usage(
            "unsupported file type: image/svg+xml".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_upload(bytes: &[u8]) -> Result<String, CliError> {
    let mime = infer::get(bytes)
        .map(|kind| kind.mime_type())
        .unwrap_or("application/octet-stream");
    validate_type_and_size(mime, bytes.len() as u64, looks_like_iso_bmff(bytes))?;
    Ok(mime.to_string())
}

fn validate_type_and_size(mime: &str, size: u64, iso_bmff: bool) -> Result<(), CliError> {
    let max = match mime {
        "image/jpeg" | "image/png" | "image/gif" | "image/webp" => MAX_IMAGE_BYTES,
        "video/mp4" => MAX_VIDEO_BYTES,
        _ => {
            if iso_bmff
                || mime.starts_with("image/")
                || mime.starts_with("video/")
                || mime.starts_with("audio/")
                || BLOCKED_FILE_MIME_TYPES.contains(&mime)
            {
                return Err(CliError::Usage(format!("unsupported file type: {mime}")));
            }
            MAX_FILE_BYTES
        }
    };
    if size > max {
        return Err(CliError::Usage(format!(
            "file too large: {size} bytes (max {max})"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffed_upload_policy() {
        let mut elf = [0u8; 64];
        elf[..4].copy_from_slice(b"\x7fELF");
        for (bytes, mime) in [
            (b"{\"event\":1}\n".as_slice(), "application/octet-stream"),
            // infer 0.19 has no Parquet matcher; accept it as an opaque download.
            (b"PAR1dataPAR1".as_slice(), "application/octet-stream"),
            (b"%PDF-1.7\n".as_slice(), "application/pdf"),
            (b"\xff\xd8\xff\xe0".as_slice(), "image/jpeg"),
            (
                b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00isommp42".as_slice(),
                "video/mp4",
            ),
        ] {
            assert_eq!(validate_upload(bytes).unwrap(), mime);
        }
        for bytes in [
            elf.as_slice(),
            b"MZexecutable".as_slice(),
            b"ID3\x04\x00\x00\x00\x00\x00\x00".as_slice(),
            b"BMbitmap".as_slice(),
            b"\x00\x00\x00\x10ftypzzzz\x00\x00\x00\x00".as_slice(),
        ] {
            assert!(validate_upload(bytes).is_err(), "accepted {bytes:?}");
        }
    }

    #[test]
    fn svg_filename_is_rejected_without_a_magic_signature() {
        for name in ["drawing.svg", "drawing.SVG", "dir/drawing.SvG"] {
            assert!(validate_filename(name).is_err(), "{name}");
        }
        for name in [
            "drawing.svg.jsonl",
            "bitgo-events-test.jsonl",
            "table.parquet",
        ] {
            assert!(validate_filename(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn deny_list_and_size_boundaries() {
        for mime in BLOCKED_FILE_MIME_TYPES {
            assert!(validate_type_and_size(mime, 1, false).is_err(), "{mime}");
        }
        for (mime, cap) in [
            ("image/jpeg", MAX_IMAGE_BYTES),
            ("video/mp4", MAX_VIDEO_BYTES),
            ("application/octet-stream", MAX_FILE_BYTES),
            ("application/vnd.apache.parquet", MAX_FILE_BYTES),
        ] {
            assert!(validate_type_and_size(mime, cap, false).is_ok());
            assert!(validate_type_and_size(mime, cap + 1, false).is_err());
        }
    }
}
