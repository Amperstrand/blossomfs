use figment::Figment;
use figment::providers::{Env, Format, Toml};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::cli;

const ENV_FIELDS: &[&str] = &[
    "npub",
    "pubkey",
    "server",
    "manifest",
    "cache_dir",
    "read_only",
    "nsec_file",
    "relay",
    "nip34_relay",
    "nip34_pubkey",
    "nip34_clone",
    "ttl_secs",
    "max_write_mb",
    "free_period_days",
    "max_free_size_mb",
    "max_cache_size",
    "multipart_threshold_mb",
    "metrics_port",
];

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlossomConfig {
    pub npub: Option<String>,
    pub pubkey: Option<String>,
    #[serde(default)]
    pub server: Vec<String>,
    pub manifest: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub read_only: Option<bool>,
    pub nsec_file: Option<PathBuf>,
    #[serde(default)]
    pub relay: Vec<String>,
    #[serde(default)]
    pub nip34_relay: Vec<String>,
    pub nip34_pubkey: Option<String>,
    pub nip34_clone: Option<bool>,
    pub ttl_secs: Option<u64>,
    pub max_write_mb: Option<u64>,
    pub free_period_days: Option<u64>,
    pub max_free_size_mb: Option<u64>,
    pub max_cache_size: Option<u64>,
    pub multipart_threshold_mb: Option<u64>,
    pub metrics_port: Option<u16>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file {0}: {1}")]
    Read(String, #[source] std::io::Error),
    #[error("failed to parse config file {0}: {1}")]
    Parse(String, #[source] toml::de::Error),
    #[error("configuration error: {0}")]
    Figment(String),
    #[error("validation error: {0}")]
    Validation(String),
}

impl BlossomConfig {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Read(path.display().to_string(), e))?;
        toml::from_str(&content).map_err(|e| ConfigError::Parse(path.display().to_string(), e))
    }

    pub fn load_merged(
        config_path: Option<&Path>,
        args: &mut cli::MountArgs,
        is_explicit: impl Fn(&str) -> bool,
    ) -> Result<(), ConfigError> {
        let mut figment = Figment::new();

        if let Some(path) = config_path {
            figment = figment.merge(Toml::file(path));
        }

        figment = figment.merge(Env::prefixed("BLOSSOMFS_").only(ENV_FIELDS));

        let config: BlossomConfig = figment
            .extract()
            .map_err(|e| ConfigError::Figment(e.to_string()))?;

        config.merge_into(args, is_explicit);
        Ok(())
    }

    pub fn merge_into<F>(&self, args: &mut cli::MountArgs, is_explicit: F)
    where
        F: Fn(&str) -> bool,
    {
        if !is_explicit("npub")
            && let Some(ref n) = self.npub
        {
            args.npub = vec![n.clone()];
        }
        if !is_explicit("pubkey")
            && let Some(ref p) = self.pubkey
        {
            args.pubkey = vec![p.clone()];
        }
        if !is_explicit("manifest") && self.manifest.is_some() {
            args.manifest = self.manifest.clone();
        }
        if !is_explicit("nsec-file") && self.nsec_file.is_some() {
            args.nsec_file = self.nsec_file.clone();
        }
        if !is_explicit("nip34-pubkey") && self.nip34_pubkey.is_some() {
            args.nip34_pubkey = self.nip34_pubkey.clone();
        }

        if !is_explicit("server") && !self.server.is_empty() {
            args.server = self.server.clone();
        }
        if !is_explicit("relay") && !self.relay.is_empty() {
            args.relay = self.relay.clone();
        }
        if !is_explicit("nip34-relay") && !self.nip34_relay.is_empty() {
            args.nip34_relay = self.nip34_relay.clone();
        }

        if !is_explicit("cache-dir")
            && let Some(ref v) = self.cache_dir
        {
            args.cache_dir = v.clone();
        }
        if !is_explicit("read-only")
            && let Some(v) = self.read_only
        {
            args.read_only = v;
        }
        if !is_explicit("nip34-clone")
            && let Some(v) = self.nip34_clone
        {
            args.nip34_clone = v;
        }
        if !is_explicit("ttl-secs")
            && let Some(v) = self.ttl_secs
        {
            args.ttl_secs = v;
        }
        if !is_explicit("max-write-mb")
            && let Some(v) = self.max_write_mb
        {
            args.max_write_mb = v;
        }
        if !is_explicit("free-period-days")
            && let Some(v) = self.free_period_days
        {
            args.free_period_days = v;
        }
        if !is_explicit("max-free-size-mb")
            && let Some(v) = self.max_free_size_mb
        {
            args.max_free_size_mb = v;
        }
        if !is_explicit("max-cache-size")
            && let Some(v) = self.max_cache_size
        {
            args.max_cache_size = v;
        }
        if !is_explicit("multipart-threshold-mb")
            && let Some(v) = self.multipart_threshold_mb
        {
            args.multipart_threshold_mb = v;
        }
        if !is_explicit("metrics-port")
            && let Some(v) = self.metrics_port
        {
            args.metrics_port = Some(v);
        }
    }
}

pub fn validate_args(args: &cli::MountArgs) -> Result<(), ConfigError> {
    if args.npub.is_empty()
        && args.pubkey.is_empty()
        && args.manifest.is_none()
        && args.nip34_pubkey.is_none()
    {
        return Err(ConfigError::Validation(
            "at least one of --npub, --pubkey, --manifest, or --nip34-pubkey must be provided"
                .into(),
        ));
    }

    for n in &args.npub {
        if !n.starts_with("npub1") && !is_hex_64(n) {
            return Err(ConfigError::Validation(format!(
                "invalid npub '{n}': must start with 'npub1' or be 64-char hex"
            )));
        }
    }

    for p in &args.pubkey {
        if !is_hex_64(p) {
            return Err(ConfigError::Validation(format!(
                "invalid pubkey '{p}': must be 64-char hexadecimal"
            )));
        }
    }

    for s in &args.server {
        if !s.starts_with("http://") && !s.starts_with("https://") {
            return Err(ConfigError::Validation(format!(
                "invalid server URL '{s}': must start with http:// or https://"
            )));
        }
    }

    for r in &args.relay {
        if !r.starts_with("ws://") && !r.starts_with("wss://") {
            return Err(ConfigError::Validation(format!(
                "invalid relay URL '{r}': must start with ws:// or wss://"
            )));
        }
    }

    if !args.read_only && args.server.is_empty() {
        return Err(ConfigError::Validation(
            "read-write mode requires --server URL".into(),
        ));
    }

    if args.ttl_secs == 0 {
        return Err(ConfigError::Validation("ttl-secs must be > 0".into()));
    }

    Ok(())
}

fn is_hex_64(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_args() -> cli::MountArgs {
        cli::MountArgs::default()
    }

    #[test]
    fn s1_config_parse_valid_toml() {
        let toml = r#"
npub = "npub1test"
server = ["https://blossom.example.com"]
relay = ["wss://relay.example.com"]
ttl_secs = 3600
read_only = false
"#;
        let config: BlossomConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.npub.as_deref(), Some("npub1test"));
        assert_eq!(config.server.len(), 1);
        assert_eq!(config.server[0], "https://blossom.example.com");
        assert_eq!(config.relay.len(), 1);
        assert_eq!(config.ttl_secs, Some(3600));
        assert_eq!(config.read_only, Some(false));
    }

    #[test]
    fn s2_config_parse_empty_file() {
        let config: BlossomConfig = toml::from_str("").unwrap();
        assert!(config.npub.is_none());
        assert!(config.server.is_empty());
    }

    #[test]
    fn s3_config_merge_fills_gaps() {
        let config = BlossomConfig {
            npub: Some("npub1test".to_string()),
            server: vec!["https://blossom.example.com".to_string()],
            ttl_secs: Some(3600),
            ..Default::default()
        };

        let mut args = default_args();
        config.merge_into(&mut args, |_| false);

        assert_eq!(args.npub.first().map(|s| s.as_str()), Some("npub1test"));
        assert_eq!(args.server.len(), 1);
        assert_eq!(args.ttl_secs, 3600);
    }

    #[test]
    fn s4_config_merge_respects_explicit_cli() {
        let config = BlossomConfig {
            npub: Some("npub1config".to_string()),
            server: vec!["https://config.example.com".to_string()],
            ttl_secs: Some(3600),
            ..Default::default()
        };

        let mut args = default_args();
        config.merge_into(&mut args, |_| true);

        assert!(args.npub.is_empty());
        assert_ne!(args.ttl_secs, 3600);
    }

    #[test]
    fn s5_config_merge_partial_override() {
        let config = BlossomConfig {
            npub: Some("npub1config".to_string()),
            server: vec!["https://config.example.com".to_string()],
            ttl_secs: Some(3600),
            ..Default::default()
        };

        let mut args = default_args();
        config.merge_into(&mut args, |name| name == "npub");

        assert!(args.npub.is_empty());
        assert_eq!(args.server.len(), 1);
        assert_eq!(args.server[0], "https://config.example.com");
        assert_eq!(args.ttl_secs, 3600);
    }

    #[test]
    fn s6_config_merge_nip34_fields() {
        let config = BlossomConfig {
            nip34_relay: vec!["wss://relay.ngit.dev".to_string()],
            nip34_pubkey: Some("npub1git".to_string()),
            nip34_clone: Some(true),
            ..Default::default()
        };

        let mut args = default_args();
        config.merge_into(&mut args, |_| false);

        assert_eq!(args.nip34_relay.len(), 1);
        assert_eq!(args.nip34_relay[0], "wss://relay.ngit.dev");
        assert_eq!(args.nip34_pubkey.as_deref(), Some("npub1git"));
        assert!(args.nip34_clone);
    }

    #[test]
    fn s7_config_load_missing_file() {
        let result = BlossomConfig::load(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    #[test]
    fn s8_config_load_from_tempfile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.toml");
        std::fs::write(&path, r#"npub = "npub1test""#).unwrap();

        let config = BlossomConfig::load(&path).unwrap();
        assert_eq!(config.npub.as_deref(), Some("npub1test"));
    }

    #[test]
    fn s9_config_rejects_unknown_fields() {
        let toml = r#"dangerous_nsec_arg = "nsec1...""#;
        let result: Result<BlossomConfig, _> = toml::from_str(toml);
        assert!(result.is_err());
    }

    #[test]
    fn s10_config_merge_cache_dir() {
        let config = BlossomConfig {
            cache_dir: Some(PathBuf::from("/var/cache/blossomfs")),
            ..Default::default()
        };

        let mut args = default_args();
        config.merge_into(&mut args, |_| false);

        assert_eq!(args.cache_dir, PathBuf::from("/var/cache/blossomfs"));
    }

    #[test]
    fn s11_config_merge_read_only_false() {
        let config = BlossomConfig {
            read_only: Some(false),
            ..Default::default()
        };

        let mut args = default_args();
        assert!(args.read_only);
        config.merge_into(&mut args, |_| false);
        assert!(!args.read_only);
    }

    #[test]
    fn s12_config_merge_write_limits() {
        let config = BlossomConfig {
            max_write_mb: Some(200),
            free_period_days: Some(60),
            max_free_size_mb: Some(5),
            ..Default::default()
        };

        let mut args = default_args();
        config.merge_into(&mut args, |_| false);

        assert_eq!(args.max_write_mb, 200);
        assert_eq!(args.free_period_days, 60);
        assert_eq!(args.max_free_size_mb, 5);
    }

    #[test]
    fn s13_config_env_var_override() {
        unsafe {
            std::env::set_var("BLOSSOMFS_TTL_SECS", "99");
        }
        let mut args = default_args();
        BlossomConfig::load_merged(None, &mut args, |_| false).unwrap();
        unsafe {
            std::env::remove_var("BLOSSOMFS_TTL_SECS");
        }
        assert_eq!(args.ttl_secs, 99);
    }

    #[test]
    fn s14_config_env_var_npub() {
        unsafe {
            std::env::set_var("BLOSSOMFS_NPUB", "npub1envtest");
        }
        let mut args = default_args();
        BlossomConfig::load_merged(None, &mut args, |_| false).unwrap();
        unsafe {
            std::env::remove_var("BLOSSOMFS_NPUB");
        }
        assert_eq!(args.npub.first().map(|s| s.as_str()), Some("npub1envtest"));
    }

    #[test]
    fn s15_config_cli_overrides_env() {
        unsafe {
            std::env::set_var("BLOSSOMFS_TTL_SECS", "99");
        }
        let mut args = default_args();
        BlossomConfig::load_merged(None, &mut args, |_| true).unwrap();
        unsafe {
            std::env::remove_var("BLOSSOMFS_TTL_SECS");
        }
        assert_ne!(args.ttl_secs, 99);
    }

    #[test]
    fn s16_config_metrics_port_default() {
        let cfg = BlossomConfig::default();
        assert_eq!(cfg.metrics_port, None);
    }

    #[test]
    fn s17_config_env_var_metrics_port() {
        unsafe {
            std::env::set_var("BLOSSOMFS_METRICS_PORT", "9999");
        }
        let mut args = default_args();
        BlossomConfig::load_merged(None, &mut args, |_| false).unwrap();
        unsafe {
            std::env::remove_var("BLOSSOMFS_METRICS_PORT");
        }
        assert_eq!(args.metrics_port, Some(9999));
    }

    #[test]
    fn v1_validate_rejects_no_identity() {
        let args = cli::MountArgs {
            mountpoint: PathBuf::new(),
            ..default_args()
        };
        let err = validate_args(&args).unwrap_err();
        assert!(err.to_string().contains("npub"));
    }

    #[test]
    fn v2_validate_accepts_valid_npub() {
        let mut args = default_args();
        args.npub = vec!["npub1valid".to_string()];
        validate_args(&args).unwrap();
    }

    #[test]
    fn v3_validate_accepts_valid_hex_pubkey() {
        let mut args = default_args();
        args.pubkey = vec!["a".repeat(64)];
        validate_args(&args).unwrap();
    }

    #[test]
    fn v4_validate_rejects_short_pubkey() {
        let mut args = default_args();
        args.pubkey = vec!["abc".to_string()];
        let err = validate_args(&args).unwrap_err();
        assert!(err.to_string().contains("64-char"));
    }

    #[test]
    fn v5_validate_rejects_bad_server_url() {
        let mut args = default_args();
        args.npub = vec!["npub1test".to_string()];
        args.server = vec!["ftp://bad.example.com".to_string()];
        let err = validate_args(&args).unwrap_err();
        assert!(err.to_string().contains("http"));
    }

    #[test]
    fn v6_validate_rejects_rw_without_server() {
        let mut args = default_args();
        args.npub = vec!["npub1test".to_string()];
        args.read_only = false;
        let err = validate_args(&args).unwrap_err();
        assert!(err.to_string().contains("server"));
    }

    #[test]
    fn v7_validate_accepts_rw_with_server() {
        let mut args = default_args();
        args.npub = vec!["npub1test".to_string()];
        args.read_only = false;
        args.server = vec!["https://blossom.example.com".to_string()];
        validate_args(&args).unwrap();
    }

    #[test]
    fn v8_validate_rejects_bad_relay_url() {
        let mut args = default_args();
        args.npub = vec!["npub1test".to_string()];
        args.relay = vec!["https://not-a-relay.com".to_string()];
        let err = validate_args(&args).unwrap_err();
        assert!(err.to_string().contains("ws://"));
    }

    #[test]
    fn v9_validate_rejects_zero_ttl() {
        let mut args = default_args();
        args.npub = vec!["npub1test".to_string()];
        args.ttl_secs = 0;
        let err = validate_args(&args).unwrap_err();
        assert!(err.to_string().contains("ttl"));
    }
}
