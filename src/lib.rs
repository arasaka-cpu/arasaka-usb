use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub mod download;
pub mod flash;
pub mod gui;

/// True when ARASAKA_DEBUG is set (non-empty and not "0"). The flatpak
/// sandbox strips unknown env vars, so run with
/// `flatpak run --env=ARASAKA_DEBUG=1 org.arasaka.usb`.
pub fn dbg_enabled() -> bool {
    std::env::var("ARASAKA_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0")
}

pub fn dbg(args: std::fmt::Arguments) {
    if dbg_enabled() {
        eprintln!("[arasaka] {}", args);
    }
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        $crate::dbg(format_args!($($arg)*))
    };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub file: String,
    pub total: u64,
    pub sha256: String,
    pub parts: Vec<String>,
}

pub struct Source {
    pub repo: String,
    pub tag: String,
}

impl Default for Source {
    fn default() -> Self {
        Source {
            repo: "arasaka-cpu/arasaka".into(),
            tag: "rolling".into(),
        }
    }
}

const GH_UA: &str = concat!("arasaka-usb/", env!("CARGO_PKG_VERSION"));

pub fn client() -> Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .user_agent(GH_UA)
        .connect_timeout(std::time::Duration::from_secs(20))
        .build()?)
}

pub fn fetch_manifest(c: &reqwest::blocking::Client, src: &Source) -> Result<Manifest> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/tags/{}",
        src.repo, src.tag
    );
    let release: serde_json::Value = c
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .with_context(|| format!("release lookup {url}"))?
        .error_for_status()
        .with_context(|| format!("release lookup HTTP {}", url))?
        .json()
        .context("parse release JSON")?;

    let assets = release
        .get("assets")
        .and_then(|a| a.as_array())
        .context("release has no assets")?;

    let manifest_asset = assets
        .iter()
        .find(|a| {
            a.get("name")
                .and_then(|n| n.as_str())
                .map(|n| n.ends_with(".iso.parts.json"))
                .unwrap_or(false)
        })
        .context("no .iso.parts.json asset in release")?;

    let dl_url = manifest_asset
        .get("browser_download_url")
        .and_then(|u| u.as_str())
        .context("manifest asset has no download url")?;

    let m: Manifest = c
        .get(dl_url)
        .send()
        .with_context(|| format!("fetch manifest {dl_url}"))?
        .error_for_status()?
        .json()
        .context("parse manifest JSON")?;

    if m.parts.is_empty() {
        bail!("manifest lists no parts");
    }
    if m.total == 0 {
        bail!("manifest total is 0");
    }
    if !m.sha256.is_empty() && m.sha256.len() != 64 {
        bail!("manifest sha256 has invalid length {}", m.sha256.len());
    }
    Ok(m)
}

pub fn part_url(src: &Source, name: &str) -> String {
    format!(
        "https://github.com/{}/releases/download/{}/{}",
        src.repo,
        src.tag,
        urlencoding::encode(name)
    )
}

pub fn part_download_url(src: &Source, name: &str) -> String {
    part_url(src, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_manifest() {
        let j = r#"{
          "file": "arasaka-20260808.iso",
          "total": 4936945664,
          "sha256": "4e88472f9d00c1a5ce2356f3085e55b56e1afc17c59f1ed129cb0e4ee515b114",
          "parts": ["a.part.00", "a.part.01"]
        }"#;
        let m: Manifest = serde_json::from_str(j).unwrap();
        assert_eq!(m.file, "arasaka-20260808.iso");
        assert_eq!(m.total, 4936945664);
        assert_eq!(m.parts.len(), 2);
    }

    #[test]
    fn part_url_escapes() {
        let s = Source {
            repo: "r/o".into(),
            tag: "t".into(),
        };
        assert_eq!(
            part_url(&s, "a b.part.0"),
            "https://github.com/r/o/releases/download/t/a%20b.part.0"
        );
    }
}
