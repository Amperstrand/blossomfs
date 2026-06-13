//! MIME type to file extension inference.
//!
//! Uses `mime_guess` crate to map MIME types (e.g. "image/png") to
//! file extensions (e.g. "png"). Extensions are UX-only — they are
//! not canonical names in Blossom.

use mime_guess::mime;

/// Infer file extension from MIME type using mime_guess crate.
///
/// Returns Some(extension) for known MIME types, None for octet-stream or unknown.
#[allow(dead_code)]
fn preferred_extension(mime_type: &str) -> Option<String> {
    match mime_type {
        "text/plain" => Some("txt".to_string()),
        "text/html" => Some("html".to_string()),
        "text/css" => Some("css".to_string()),
        "text/javascript" | "application/javascript" => Some("js".to_string()),
        "application/json" => Some("json".to_string()),
        "application/xml" | "text/xml" => Some("xml".to_string()),
        "image/jpeg" => Some("jpg".to_string()),
        "image/svg+xml" => Some("svg".to_string()),
        "audio/mpeg" => Some("mp3".to_string()),
        "video/mp4" => Some("mp4".to_string()),
        "video/webm" => Some("webm".to_string()),
        "application/zip" => Some("zip".to_string()),
        "application/gzip" => Some("gz".to_string()),
        "application/x-tar" => Some("tar".to_string()),
        _ => None,
    }
}

pub fn mime_to_extension(mime_type: &str) -> Option<String> {
    if mime_type.is_empty() {
        return None;
    }

    if let Some(ext) = preferred_extension(mime_type) {
        return Some(ext);
    }

    let mime_type: mime::Mime = mime_type.parse().ok()?;

    if mime_type.type_() == mime::APPLICATION && mime_type.subtype() == mime::OCTET_STREAM {
        return None;
    }

    mime_guess::get_mime_extensions(&mime_type)
        .and_then(|exts| exts.first())
        .map(|s| s.to_string())
}

/// Get file extension from MIME type or URL.
///
/// Tries MIME type first, then extracts from URL path.
/// Returns empty string if no extension found.
#[allow(dead_code)]
pub fn extension_for_descriptor(mime_type: Option<&str>, url: &str) -> String {
    mime_type
        .and_then(mime_to_extension)
        .unwrap_or_else(|| extract_extension_from_url(url))
}

/// Extract extension from URL path.
///
/// Returns lowercase extension if valid (1-5 alphanumeric chars), empty string otherwise.
#[allow(dead_code)]
fn extract_extension_from_url(url: &str) -> String {
    let path = url
        .split('?')
        .next()
        .unwrap_or(url)
        .split('#')
        .next()
        .unwrap_or(url);

    let last_segment = path.rsplit('/').next().unwrap_or("");

    if let Some(dot_pos) = last_segment.rfind('.') {
        let ext = &last_segment[dot_pos + 1..];

        if !ext.is_empty() && ext.len() <= 5 && ext.chars().all(|c| c.is_alphanumeric()) {
            return ext.to_lowercase();
        }
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    // mime_to_extension tests

    #[test]
    fn test_mime_to_extension_image_png() {
        assert_eq!(mime_to_extension("image/png"), Some("png".to_string()));
    }

    #[test]
    fn test_mime_to_extension_application_pdf() {
        assert_eq!(
            mime_to_extension("application/pdf"),
            Some("pdf".to_string())
        );
    }

    #[test]
    fn test_mime_to_extension_octet_stream() {
        assert_eq!(mime_to_extension("application/octet-stream"), None);
    }

    #[test]
    fn test_mime_to_extension_empty() {
        assert_eq!(mime_to_extension(""), None);
    }

    #[test]
    fn test_mime_to_extension_unknown() {
        assert_eq!(mime_to_extension("x-custom/type"), None);
    }

    #[test]
    fn test_mime_to_extension_jpeg() {
        let ext = mime_to_extension("image/jpeg");
        assert!(ext.is_some());
        assert_eq!(ext.unwrap(), "jpg");
    }

    #[test]
    fn test_mime_to_extension_text_plain() {
        assert_eq!(mime_to_extension("text/plain"), Some("txt".to_string()));
    }

    #[test]
    fn test_mime_to_extension_json() {
        assert_eq!(
            mime_to_extension("application/json"),
            Some("json".to_string())
        );
    }

    // extension_for_descriptor tests

    #[test]
    fn test_extension_for_descriptor_with_mime() {
        assert_eq!(
            extension_for_descriptor(Some("image/png"), "https://x.com/abc"),
            "png"
        );
    }

    #[test]
    fn test_extension_for_descriptor_from_url() {
        assert_eq!(
            extension_for_descriptor(None, "https://x.com/abc.PDF"),
            "pdf"
        );
    }

    #[test]
    fn test_extension_for_descriptor_no_extension() {
        assert_eq!(extension_for_descriptor(None, "https://x.com/abc"), "");
    }

    #[test]
    fn test_extension_for_descriptor_mime_priority() {
        assert_eq!(
            extension_for_descriptor(Some("image/png"), "https://x.com/abc.jpg"),
            "png"
        );
    }

    #[test]
    fn test_extension_for_descriptor_multiple_dots() {
        assert_eq!(
            extension_for_descriptor(None, "https://x.com/abc.def.123.pdf"),
            "pdf"
        );
    }

    #[test]
    fn test_extension_for_descriptor_extension_too_long() {
        assert_eq!(
            extension_for_descriptor(None, "https://x.com/abc.toolongext"),
            ""
        );
    }

    #[test]
    fn test_extension_for_descriptor_non_alphanumeric() {
        assert_eq!(extension_for_descriptor(None, "https://x.com/abc.12$"), "");
    }

    #[test]
    fn test_extension_for_descriptor_mime_none() {
        assert_eq!(
            extension_for_descriptor(Some("application/octet-stream"), "https://x.com/abc.png"),
            "png"
        );
    }

    #[test]
    fn test_extension_for_descriptor_both_none() {
        assert_eq!(extension_for_descriptor(None, "https://x.com/abc"), "");
    }
}
