//! Path and hostname sanitization.
//!
//! Security: All remote metadata is untrusted. This module ensures:
//! - No path traversal (`..`)
//! - No null bytes
//! - No control characters
//! - No absolute path escapes
//! - Safe hostname and MIME type components for filesystem paths

/// Sanitize a server hostname for use as a directory name.
///
/// Strips protocol prefix (https://, http://), takes only the host portion
/// (drops port, path, query), replaces unsafe characters, and limits to 255 chars.
#[allow(dead_code)]
pub fn sanitize_hostname(input: &str) -> String {
    if input.is_empty() {
        return "unknown".to_string();
    }

    let raw = input
        .strip_prefix("https://")
        .or_else(|| input.strip_prefix("http://"))
        .unwrap_or(input);

    let host = raw
        .split('/')
        .next()
        .unwrap_or(raw)
        .split(':')
        .next()
        .unwrap_or(raw)
        .split('?')
        .next()
        .unwrap_or(raw)
        .split('#')
        .next()
        .unwrap_or(raw);

    let unsafe_chars = ['\0', '\n', '\r', '*', '?', '"', '<', '>', '|', '\\', '/'];
    let mut result = String::with_capacity(host.len().min(255));
    for c in host.chars() {
        if unsafe_chars.contains(&c) {
            result.push('_');
        } else {
            result.push(c);
        }
    }

    result = result.replace("..", "__");
    result.truncate(255);

    if result.is_empty() {
        "unknown".to_string()
    } else {
        result
    }
}

/// Validate and normalize a SHA-256 hash string.
///
/// Returns the lowercase hex string if valid, or an error message if invalid.
#[allow(dead_code)]
pub fn sanitize_sha256(input: &str) -> Result<String, String> {
    if input.len() != 64 {
        return Err(format!(
            "SHA-256 hash must be exactly 64 characters, got {}",
            input.len()
        ));
    }

    if !input.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("SHA-256 hash must contain only hex characters (0-9, a-f, A-F)".to_string());
    }

    Ok(input.to_lowercase())
}

/// Sanitize any string for use as a single path component.
///
/// Rejects/replace unsafe characters and ensures no path traversal.
#[allow(dead_code)]
pub fn sanitize_path_component(input: &str) -> String {
    if input.is_empty() {
        return "unnamed".to_string();
    }

    let mut result = String::with_capacity(input.len().min(255));
    for c in input.chars() {
        match c {
            '/' | '\0' | '\\' => {
                result.push('_');
            }
            c if c.is_ascii_control() => {
                result.push('_');
            }
            _ => {
                result.push(c);
            }
        }
    }

    result = result.replace("..", "__");

    while result.len() > 255 {
        result.pop();
    }

    if result.is_empty() || result.chars().all(|c| c == '_') {
        "unnamed".to_string()
    } else {
        result
    }
}

/// Sanitize MIME type for use as directory name.
///
/// Replaces '/' with '_' and applies path component sanitization.
#[allow(dead_code)]
pub fn sanitize_mime_for_path(mime_type: &str) -> String {
    if mime_type.is_empty() {
        return "unknown".to_string();
    }

    let sanitized = mime_type.replace('/', "_");

    let result = sanitize_path_component(&sanitized);

    if result.is_empty() || result.chars().all(|c| c == '_') {
        "unknown".to_string()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // sanitize_hostname tests

    #[test]
    fn test_sanitize_hostname_simple() {
        assert_eq!(sanitize_hostname("cdn.example.com"), "cdn.example.com");
    }

    #[test]
    fn test_sanitize_hostname_with_url() {
        assert_eq!(
            sanitize_hostname("https://cdn.example.com:8080/path?q=1"),
            "cdn.example.com"
        );
    }

    #[test]
    fn test_sanitize_hostname_http_prefix() {
        assert_eq!(sanitize_hostname("http://example.com/path"), "example.com");
    }

    #[test]
    fn test_sanitize_hostname_no_port() {
        assert_eq!(
            sanitize_hostname("https://cdn.example.com/path"),
            "cdn.example.com"
        );
    }

    #[test]
    fn test_sanitize_hostname_empty() {
        assert_eq!(sanitize_hostname(""), "unknown");
    }

    #[test]
    fn test_sanitize_hostname_path_traversal() {
        let result = sanitize_hostname("../../../etc");
        assert!(!result.contains(".."));
        assert!(!result.contains("/"));
        assert_eq!(result, "__");
    }

    #[test]
    fn test_sanitize_hostname_unsafe_chars() {
        assert_eq!(sanitize_hostname("a*b?c\"d<e>f|g"), "a_b");
    }

    #[test]
    fn test_sanitize_hostname_null_and_control() {
        assert_eq!(sanitize_hostname("a\0b\nc\rd"), "a_b_c_d");
    }

    #[test]
    fn test_sanitize_hostname_colon_stripped() {
        assert_eq!(sanitize_hostname("host:8080"), "host");
    }

    #[test]
    fn test_sanitize_hostname_too_long() {
        let long_name = "a".repeat(300);
        let result = sanitize_hostname(&long_name);
        assert!(result.len() <= 255);
        // Should not be truncated to empty
        assert!(!result.is_empty());
    }

    // sanitize_sha256 tests

    #[test]
    fn test_sanitize_sha256_valid() {
        let hash = "ABCDEF1234567890ABCDEF1234567890ABCDEF1234567890ABCDEF1234567890";
        let expected = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        assert_eq!(sanitize_sha256(hash).unwrap(), expected);
    }

    #[test]
    fn test_sanitize_sha256_lowercase() {
        let hash = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        assert_eq!(
            sanitize_sha256(hash).unwrap(),
            "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890"
        );
    }

    #[test]
    fn test_sanitize_sha256_too_short() {
        let result = sanitize_sha256("short");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("64 characters"));
    }

    #[test]
    fn test_sanitize_sha256_too_long() {
        let hash = "a".repeat(65);
        let result = sanitize_sha256(&hash);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("64 characters"));
    }

    #[test]
    fn test_sanitize_sha256_invalid_hex() {
        let hash = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";
        let result = sanitize_sha256(hash);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("hex"));
    }

    #[test]
    fn test_sanitize_sha256_mixed_hex() {
        let hash = "AbCdEf1234567890aBcDeF1234567890AbCdEf1234567890aBcDeF1234567890";
        let expected = "abcdef1234567890abcdef1234567890abcdef1234567890abcdef1234567890";
        assert_eq!(sanitize_sha256(hash).unwrap(), expected);
    }

    // sanitize_path_component tests

    #[test]
    fn test_sanitize_path_component_normal() {
        assert_eq!(sanitize_path_component("normal_name"), "normal_name");
    }

    #[test]
    fn test_sanitize_path_component_with_dots() {
        assert_eq!(sanitize_path_component("file.name"), "file.name");
    }

    #[test]
    fn test_sanitize_path_component_path_traversal() {
        let result = sanitize_path_component("../../../etc/passwd");
        assert!(!result.contains(".."));
        assert!(!result.contains("/"));
        assert_eq!(result, "_________etc_passwd");
    }

    #[test]
    fn test_sanitize_path_component_null_bytes() {
        assert_eq!(sanitize_path_component("a\0b"), "a_b");
    }

    #[test]
    fn test_sanitize_path_component_control_chars() {
        let result = sanitize_path_component("a\nb\tc\rd");
        assert_eq!(result, "a_b_c_d");
    }

    #[test]
    fn test_sanitize_path_component_empty() {
        assert_eq!(sanitize_path_component(""), "unnamed");
    }

    #[test]
    fn test_sanitize_path_component_only_unsafe() {
        assert_eq!(sanitize_path_component("\0/\n\r"), "unnamed");
    }

    #[test]
    fn test_sanitize_path_component_too_long() {
        let long_name = "a".repeat(300);
        let result = sanitize_path_component(&long_name);
        assert!(result.len() <= 255);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_sanitize_path_component_literal_two_dots() {
        assert_eq!(sanitize_path_component(".."), "unnamed");
    }

    // sanitize_mime_for_path tests

    #[test]
    fn test_sanitize_mime_for_path_image_png() {
        assert_eq!(sanitize_mime_for_path("image/png"), "image_png");
    }

    #[test]
    fn test_sanitize_mime_for_path_application_pdf() {
        assert_eq!(sanitize_mime_for_path("application/pdf"), "application_pdf");
    }

    #[test]
    fn test_sanitize_mime_for_path_empty() {
        assert_eq!(sanitize_mime_for_path(""), "unknown");
    }

    #[test]
    fn test_sanitize_mime_for_path_with_unsafe_chars() {
        assert_eq!(sanitize_mime_for_path("a/b\0c"), "a_b_c");
    }
}
