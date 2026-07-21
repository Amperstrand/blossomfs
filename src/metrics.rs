//! Prometheus metrics for BlossomFS.
//!
//! Exposes 10 metrics covering cache hit/miss, upload/download counts and bytes,
//! errors by bounded category, cache size/file-count (running gauges), and
//! in-flight uploads. The /metrics HTTP endpoint is opt-in via `metrics_port`.

use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use lazy_static::lazy_static;
use prometheus::{
    Encoder, IntCounter, IntCounterVec, IntGauge, TextEncoder, gather, register_int_counter,
    register_int_counter_vec, register_int_gauge,
};

lazy_static! {
    pub static ref CACHE_HITS: IntCounter =
        register_int_counter!("blossomfs_cache_hits_total", "Cache hit count")
            .expect("register CACHE_HITS");
    pub static ref CACHE_MISSES: IntCounter =
        register_int_counter!("blossomfs_cache_misses_total", "Cache miss count")
            .expect("register CACHE_MISSES");
    pub static ref UPLOADS_TOTAL: IntCounter =
        register_int_counter!("blossomfs_uploads_total", "Total upload count")
            .expect("register UPLOADS_TOTAL");
    pub static ref UPLOADS_BYTES: IntCounter =
        register_int_counter!("blossomfs_uploads_bytes_total", "Total upload bytes")
            .expect("register UPLOADS_BYTES");
    pub static ref DOWNLOADS_TOTAL: IntCounter =
        register_int_counter!("blossomfs_downloads_total", "Total download count")
            .expect("register DOWNLOADS_TOTAL");
    pub static ref DOWNLOADS_BYTES: IntCounter =
        register_int_counter!("blossomfs_downloads_bytes_total", "Total download bytes")
            .expect("register DOWNLOADS_BYTES");
    pub static ref ERRORS: IntCounterVec =
        register_int_counter_vec!("blossomfs_errors_total", "Errors by category", &["type"])
            .expect("register ERRORS");
    pub static ref CACHE_SIZE_BYTES: IntGauge = register_int_gauge!(
        "blossomfs_cache_size_bytes",
        "Current cache size in bytes (running counter)"
    )
    .expect("register CACHE_SIZE_BYTES");
    pub static ref CACHE_FILE_COUNT: IntGauge = register_int_gauge!(
        "blossomfs_cache_file_count",
        "Current cache file count (running counter)"
    )
    .expect("register CACHE_FILE_COUNT");
    pub static ref ACTIVE_UPLOADS: IntGauge =
        register_int_gauge!("blossomfs_active_uploads", "Currently in-flight uploads")
            .expect("register ACTIVE_UPLOADS");
}

/// Touch each static to force lazy registration at startup.
/// Call once early in main() so all metrics appear on the first scrape.
pub fn init() {
    let _ = &*CACHE_HITS;
    let _ = &*CACHE_MISSES;
    let _ = &*UPLOADS_TOTAL;
    let _ = &*UPLOADS_BYTES;
    let _ = &*DOWNLOADS_TOTAL;
    let _ = &*DOWNLOADS_BYTES;
    let _ = &*ERRORS;
    let _ = &*CACHE_SIZE_BYTES;
    let _ = &*CACHE_FILE_COUNT;
    let _ = &*ACTIVE_UPLOADS;
}

/// Record an error under the given category label.
pub fn record_error(category: &'static str) {
    ERRORS.with_label_values(&[category]).inc();
}

/// Categorize a `FetchError` into a bounded label for `blossomfs_errors_total{type=...}`.
pub fn categorize_fetch_error(e: &crate::cache::fetch::FetchError) -> &'static str {
    use crate::cache::fetch::FetchError;
    match e {
        FetchError::HashMismatch { .. } => "hash_mismatch",
        FetchError::ResponseTooLarge { .. } => "size_limit",
        FetchError::Http(_) => "network",
        FetchError::Io(_) => "io",
        FetchError::Cache(_) => "cache",
    }
}

/// Categorize a `BlossomClientError` into a bounded label.
pub fn categorize_blossom_error(e: &crate::blossom::client::BlossomClientError) -> &'static str {
    use crate::blossom::client::BlossomClientError;
    match e {
        BlossomClientError::HashMismatch { .. } => "hash_mismatch",
        BlossomClientError::ServerError { status, .. } => match *status {
            401 | 403 => "auth",
            404 => "not_found",
            413 => "size_limit",
            _ => "server_error",
        },
        BlossomClientError::PaymentRequired { .. } | BlossomClientError::Payment(_) => "payment",
        BlossomClientError::Http(_) => "network",
        BlossomClientError::Json(_) => "parse",
        BlossomClientError::Io(_) => "io",
    }
}

/// Scope guard that increments `blossomfs_active_uploads` on creation
/// and decrements it on drop. Use around upload call sites so multipart
/// uploads (many PUTs for one file) count as a single in-flight upload.
pub struct ActiveUploadGuard;

impl ActiveUploadGuard {
    pub fn new() -> Self {
        ACTIVE_UPLOADS.inc();
        Self
    }
}

impl Default for ActiveUploadGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ActiveUploadGuard {
    fn drop(&mut self) {
        ACTIVE_UPLOADS.dec();
    }
}

async fn metrics_handler() -> Response {
    let encoder = TextEncoder::new();
    let metric_families = gather();
    let mut buffer = Vec::new();
    match encoder.encode(&metric_families, &mut buffer) {
        Ok(()) => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            buffer,
        )
            .into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

/// Start the Prometheus /metrics HTTP server bound to 127.0.0.1:{port}.
/// Blocks the caller; spawn in a dedicated tokio task.
pub async fn start_metrics_server(port: u16) {
    let app = Router::new().route("/metrics", get(metrics_handler));
    let addr: std::net::SocketAddr = ([127, 0, 0, 1], port).into();
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => {
            tracing::info!("metrics server listening on 127.0.0.1:{port}");
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("metrics server exited: {e}");
            }
        }
        Err(e) => {
            tracing::error!("metrics server failed to bind 127.0.0.1:{port}: {e}");
        }
    }
}

/// Bind to an OS-assigned port (127.0.0.1:0), spawn the server in a background
/// task, and return the actual port. For integration tests.
pub async fn start_metrics_server_on_ephemeral() -> u16 {
    let app = Router::new().route("/metrics", get(metrics_handler));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("ephemeral bind");
    let port = listener.local_addr().expect("local_addr").port();
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("ephemeral metrics server exited: {e}");
        }
    });
    port
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_produces_nonempty_buffer() {
        init();
        let encoder = TextEncoder::new();
        let mut buffer = Vec::new();
        encoder
            .encode(&gather(), &mut buffer)
            .expect("encode must succeed");
        assert!(!buffer.is_empty(), "encoder buffer must not be empty");
        let text = String::from_utf8_lossy(&buffer);
        assert!(
            text.contains("blossomfs_cache_hits_total"),
            "encoded output must include our metrics, got: {text}"
        );
    }

    #[test]
    fn categorize_fetch_error_variants() {
        use crate::cache::fetch::FetchError;

        assert_eq!(
            categorize_fetch_error(&FetchError::HashMismatch {
                expected: "abc".into(),
                actual: "def".into(),
            }),
            "hash_mismatch"
        );
        assert_eq!(
            categorize_fetch_error(&FetchError::ResponseTooLarge { size: 999 }),
            "size_limit"
        );
        assert_eq!(
            categorize_fetch_error(&FetchError::Io(std::io::Error::other("x"))),
            "io"
        );
    }

    #[test]
    fn categorize_blossom_error_variants() {
        use crate::blossom::client::BlossomClientError;

        assert_eq!(
            categorize_blossom_error(&BlossomClientError::HashMismatch {
                expected: "a".into(),
                actual: "b".into(),
            }),
            "hash_mismatch"
        );
        assert_eq!(
            categorize_blossom_error(&BlossomClientError::ServerError {
                status: 404,
                body: "nf".into(),
            }),
            "not_found"
        );
        assert_eq!(
            categorize_blossom_error(&BlossomClientError::ServerError {
                status: 401,
                body: "unauth".into(),
            }),
            "auth"
        );
        assert_eq!(
            categorize_blossom_error(&BlossomClientError::ServerError {
                status: 500,
                body: "ise".into(),
            }),
            "server_error"
        );
    }

    #[test]
    fn active_upload_guard_incs_and_decs() {
        init();
        let baseline = ACTIVE_UPLOADS.get();
        {
            let _guard = ActiveUploadGuard::new();
            assert_eq!(
                ACTIVE_UPLOADS.get(),
                baseline + 1,
                "guard must inc on creation"
            );
        }
        assert_eq!(ACTIVE_UPLOADS.get(), baseline, "guard must dec on drop");
    }

    #[tokio::test]
    async fn http_endpoint_serves_prometheus_format() {
        init();
        let port = start_metrics_server_on_ephemeral().await;

        let resp = reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
            .await
            .expect("reqwest get");
        assert_eq!(resp.status(), 200, "GET /metrics must return 200");

        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .expect("content-type header")
            .to_str()
            .expect("content-type ascii");
        assert!(
            content_type.contains("text/plain"),
            "content-type must be text/plain, got: {content_type}"
        );

        let body = resp.text().await.expect("body");
        assert!(
            body.contains("blossomfs_cache_hits_total"),
            "body must contain our metrics, got first 200 chars: {}",
            body.chars().take(200).collect::<String>()
        );
    }
}
