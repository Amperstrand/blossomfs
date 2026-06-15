use crate::fuse::tree::Tree;

#[derive(Debug, Clone)]
pub struct TollgateRelease {
    pub sha256: String,
    pub urls: Vec<String>,
    pub filename: String,
    pub size: u64,
    pub namespace: String,
    pub channel: String,
    pub version: String,
    pub architecture: String,
    pub device_id: Option<String>,
    pub compression: Option<String>,
    pub format: Option<String>,
    pub mime_type: Option<String>,
}

impl TollgateRelease {
    pub fn best_url(&self) -> &str {
        for url in &self.urls {
            if url.contains("primal.net") {
                return url;
            }
        }
        for url in &self.urls {
            if url.contains("orangesync.tech") {
                return url;
            }
        }
        self.urls.first().map(|s| s.as_str()).unwrap_or("")
    }

    fn category_dir(&self) -> &str {
        match self.namespace.as_str() {
            "tollgate-os" => "os",
            "tollgate-wrt" => "packages",
            other => other,
        }
    }

    fn subdir(&self) -> &str {
        self.device_id.as_deref().unwrap_or(&self.architecture)
    }
}

pub fn parse_tollgate_release(tags: &[Vec<&str>]) -> Option<TollgateRelease> {
    let mut urls = Vec::new();
    let mut sha256 = None;
    let mut filename = None;
    let mut size = 0u64;
    let mut namespace = None;
    let mut channel = None;
    let mut version = None;
    let mut architecture = None;
    let mut device_id = None;
    let mut compression = None;
    let mut format = None;
    let mut mime_type = None;
    let mut is_tollgate = false;

    for tag in tags {
        if tag.len() < 2 {
            continue;
        }
        match tag[0] {
            "url" => urls.push(tag[1].to_string()),
            "x" => sha256 = Some(tag[1].to_string()),
            "ox" if sha256.is_none() => sha256 = Some(tag[1].to_string()),
            "filename" => filename = Some(tag[1].to_string()),
            "size" => size = tag[1].parse().unwrap_or(0),
            "n" => {
                namespace = Some(tag[1].to_string());
                if tag[1].starts_with("tollgate-") {
                    is_tollgate = true;
                }
            }
            "c" => channel = Some(tag[1].to_string()),
            "v" => version = Some(tag[1].to_string()),
            "A" => architecture = Some(tag[1].to_string()),
            "device_id" => device_id = Some(tag[1].to_string()),
            "compression" => compression = Some(tag[1].to_string()),
            "format" => format = Some(tag[1].to_string()),
            "m" => mime_type = Some(tag[1].to_string()),
            _ => {}
        }
    }

    if !is_tollgate {
        return None;
    }

    Some(TollgateRelease {
        sha256: sha256?,
        filename: filename?,
        namespace: namespace?,
        channel: channel?,
        version: version?,
        architecture: architecture?,
        urls,
        size,
        device_id,
        compression,
        format,
        mime_type,
    })
}

pub fn build_tollgate_tree(tree: &mut Tree, releases: &[TollgateRelease]) -> usize {
    if releases.is_empty() {
        return 0;
    }

    let tollgate_root = tree.add_directory(tree.root(), "tollgate");
    let mut count = 0;

    for release in releases {
        let category = tree.get_or_create_dir(tollgate_root, release.category_dir());
        let channel = tree.get_or_create_dir(category, &release.channel);
        let version = tree.get_or_create_dir(channel, &release.version);
        let subdir = tree.get_or_create_dir(version, release.subdir());

        let url = release.best_url().to_string();
        if url.is_empty() {
            tracing::warn!(
                "tollgate release {} has no fetchable URL, skipping",
                release.filename
            );
            continue;
        }

        tree.add_remote_file(
            subdir,
            &release.filename,
            url,
            release.sha256.clone(),
            release.size,
            release.mime_type.clone(),
            0,
            None,
        );
        count += 1;
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_tags() -> Vec<Vec<&'static str>> {
        vec![
            vec!["url", "https://blossom.primal.net/abc123.bin"],
            vec!["url", "https://blossom1.orangesync.tech/abc123.bin"],
            vec!["m", "application/octet-stream"],
            vec!["x", "abc123def456"],
            vec!["ox", "abc123def456"],
            vec!["size", "14828394"],
            vec!["filename", "tollgate-os-glinet_gl-mt3000-v0.4.0.bin"],
            vec!["A", "aarch64_cortex-a53"],
            vec!["device_id", "glinet_gl-mt3000"],
            vec!["d", "glinet_gl-mt3000"],
            vec!["supported_devices", "glinet,gl-mt3000"],
            vec!["openwrt_version", "24.10.4"],
            vec!["v", "v0.4.0"],
            vec!["c", "stable"],
            vec!["n", "tollgate-os"],
            vec!["compression", "none"],
        ]
    }

    fn pkg_tags() -> Vec<Vec<&'static str>> {
        vec![
            vec!["url", "https://blossom.primal.net/pkg123.ipk"],
            vec!["m", "application/octet-stream"],
            vec!["x", "pkg123hash"],
            vec!["filename", "tollgate-wrt_pr-118_aarch64_cortex-a53.ipk"],
            vec!["A", "aarch64_cortex-a53"],
            vec!["v", "pr-118"],
            vec!["c", "dev"],
            vec!["n", "tollgate-wrt"],
            vec!["compression", "none"],
            vec!["format", "ipk"],
        ]
    }

    #[test]
    fn t01_parse_os_image() {
        let rel = parse_tollgate_release(&os_tags()).expect("should parse");
        assert_eq!(rel.namespace, "tollgate-os");
        assert_eq!(rel.channel, "stable");
        assert_eq!(rel.version, "v0.4.0");
        assert_eq!(rel.device_id.as_deref(), Some("glinet_gl-mt3000"));
        assert_eq!(rel.architecture, "aarch64_cortex-a53");
        assert_eq!(rel.size, 14828394);
        assert_eq!(rel.filename, "tollgate-os-glinet_gl-mt3000-v0.4.0.bin");
    }

    #[test]
    fn t02_parse_package() {
        let rel = parse_tollgate_release(&pkg_tags()).expect("should parse");
        assert_eq!(rel.namespace, "tollgate-wrt");
        assert_eq!(rel.channel, "dev");
        assert_eq!(rel.version, "pr-118");
        assert!(rel.device_id.is_none());
        assert_eq!(rel.architecture, "aarch64_cortex-a53");
        assert_eq!(rel.format.as_deref(), Some("ipk"));
    }

    #[test]
    fn t03_reject_non_tollgate() {
        let tags = vec![
            vec!["x", "hash"],
            vec!["url", "https://example.com/hash"],
            vec!["n", "some-other-project"],
        ];
        assert!(parse_tollgate_release(&tags).is_none());
    }

    #[test]
    fn t04_best_url_prefers_primal() {
        let rel = parse_tollgate_release(&os_tags()).unwrap();
        assert!(rel.best_url().contains("primal.net"));
    }

    #[test]
    fn t05_best_url_falls_back_to_orangesync() {
        let mut tags = os_tags();
        tags.retain(|t| !(t[0] == "url" && t[1].contains("primal.net")));
        let rel = parse_tollgate_release(&tags).unwrap();
        assert!(rel.best_url().contains("orangesync.tech"));
    }

    #[test]
    fn t06_category_dir_os() {
        let rel = parse_tollgate_release(&os_tags()).unwrap();
        assert_eq!(rel.category_dir(), "os");
    }

    #[test]
    fn t07_category_dir_packages() {
        let rel = parse_tollgate_release(&pkg_tags()).unwrap();
        assert_eq!(rel.category_dir(), "packages");
    }

    #[test]
    fn t08_subdir_uses_device_id_for_os() {
        let rel = parse_tollgate_release(&os_tags()).unwrap();
        assert_eq!(rel.subdir(), "glinet_gl-mt3000");
    }

    #[test]
    fn t09_subdir_uses_architecture_for_packages() {
        let rel = parse_tollgate_release(&pkg_tags()).unwrap();
        assert_eq!(rel.subdir(), "aarch64_cortex-a53");
    }

    #[test]
    fn t10_build_tree_creates_hierarchy() {
        let mut tree = Tree::new();
        let releases = vec![
            parse_tollgate_release(&os_tags()).unwrap(),
            parse_tollgate_release(&pkg_tags()).unwrap(),
        ];
        let count = build_tollgate_tree(&mut tree, &releases);
        assert_eq!(count, 2);

        let tollgate = tree.lookup(tree.root(), "tollgate");
        assert!(tollgate.is_some(), "/tollgate should exist");

        let tollgate = tollgate.unwrap();
        let os = tree.lookup(tollgate, "os");
        assert!(os.is_some(), "/tollgate/os should exist");

        let os = os.unwrap();
        let stable = tree.lookup(os, "stable");
        assert!(stable.is_some(), "/tollgate/os/stable should exist");

        let stable = stable.unwrap();
        let version = tree.lookup(stable, "v0.4.0");
        assert!(version.is_some(), "/tollgate/os/stable/v0.4.0 should exist");

        let version = version.unwrap();
        let device = tree.lookup(version, "glinet_gl-mt3000");
        assert!(
            device.is_some(),
            "/tollgate/os/stable/v0.4.0/glinet_gl-mt3000 should exist"
        );
    }

    #[test]
    fn t11_build_tree_packages_hierarchy() {
        let mut tree = Tree::new();
        let releases = vec![parse_tollgate_release(&pkg_tags()).unwrap()];
        let count = build_tollgate_tree(&mut tree, &releases);
        assert_eq!(count, 1);

        let tollgate = tree.lookup(tree.root(), "tollgate").unwrap();
        let packages = tree.lookup(tollgate, "packages").unwrap();
        let dev = tree.lookup(packages, "dev").unwrap();
        let version = tree.lookup(dev, "pr-118").unwrap();
        let arch = tree.lookup(version, "aarch64_cortex-a53").unwrap();
        let file = tree.lookup(arch, "tollgate-wrt_pr-118_aarch64_cortex-a53.ipk");
        assert!(file.is_some(), "package file should exist at the leaf");
    }

    #[test]
    fn t12_build_tree_empty_is_noop() {
        let mut tree = Tree::new();
        let count = build_tollgate_tree(&mut tree, &[]);
        assert_eq!(count, 0);
        assert!(tree.lookup(tree.root(), "tollgate").is_none());
    }

    #[test]
    fn t13_multiple_urls_all_captured() {
        let rel = parse_tollgate_release(&os_tags()).unwrap();
        assert_eq!(rel.urls.len(), 2);
        assert!(rel.urls[0].contains("primal.net"));
        assert!(rel.urls[1].contains("orangesync.tech"));
    }

    #[test]
    fn t14_compression_and_format_parsed() {
        let os_rel = parse_tollgate_release(&os_tags()).unwrap();
        assert_eq!(os_rel.compression.as_deref(), Some("none"));

        let pkg_rel = parse_tollgate_release(&pkg_tags()).unwrap();
        assert_eq!(pkg_rel.format.as_deref(), Some("ipk"));
    }

    #[test]
    fn t15_skips_release_with_no_url() {
        let mut tree = Tree::new();
        let mut tags = os_tags();
        tags.retain(|t| t[0] != "url");
        let releases = vec![parse_tollgate_release(&tags).unwrap()];
        let count = build_tollgate_tree(&mut tree, &releases);
        assert_eq!(count, 0, "release with no URL should be skipped");
    }
}
