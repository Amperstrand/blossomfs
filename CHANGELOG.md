# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

## [Unreleased]

### Added
- **Prometheus metrics endpoint (#24)** — opt-in `/metrics` HTTP server (Axum) on `127.0.0.1:{port}`. 10 metrics: cache hits/misses, upload/download count + bytes, errors by type, cache size/file count (running gauges), active uploads (gauge). Configurable via `--metrics-port` CLI flag, `BLOSSOMFS_METRICS_PORT` env var, or `metrics_port` TOML field. Default: disabled.
- **BUD-04 blob mirroring (#21)** — `BlossomClient::mirror_blob(source_url, auth_header)` method. Server-to-server blob copy via `PUT /mirror` without local download + re-upload. 4 wiremock tests (happy path, header verification, body JSON, 404 error).
