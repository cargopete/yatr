//! Toolchain management: pin, download, and expose language runtimes.
//!
//! A `[toolchain.<name>]` entry declares a `version` and a download `url`
//! template. yatr downloads the archive once into a local toolchain cache,
//! extracts it, and prepends its `bin` directory to the `PATH` used to run
//! tasks — so a fresh checkout runs green with no manual runtime installs.
//!
//! URL/`bin` templates may use `{version}`, `{os}` (`linux`/`darwin`/`win`) and
//! `{arch}` (`x64`/`arm64`). Only `.tar.gz`/`.tgz` archives are supported today.

// Placeholder strings like "{version}" are template tokens, and `HashMap` comes
// straight from the parsed config — neither pedantic lint applies here.
#![allow(
    clippy::missing_errors_doc,
    clippy::literal_string_with_formatting_args,
    clippy::implicit_hasher,
    // We lowercase the URL before the extension check, so this is a false positive.
    clippy::case_sensitive_file_extension_comparisons
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;

use crate::config::ToolchainConfig;
use crate::error::{Result, YatrError};

/// Map Rust's OS name to the common toolchain naming.
fn target_os() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        "windows" => "win",
        other => other,
    }
}

/// Map Rust's arch name to the common toolchain naming.
fn target_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Substitute `{version}`, `{os}`, and `{arch}` in a template.
fn render(template: &str, version: &str) -> String {
    template
        .replace("{version}", version)
        .replace("{os}", target_os())
        .replace("{arch}", target_arch())
}

/// Directory where toolchains are installed. Overridable via
/// `YATR_TOOLCHAIN_DIR` (useful for tests and reproducible CI).
#[must_use]
pub fn toolchains_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("YATR_TOOLCHAIN_DIR") {
        return PathBuf::from(dir);
    }
    directories::ProjectDirs::from("", "", "yatr").map_or_else(
        || PathBuf::from(".yatr/toolchains"),
        |d| d.cache_dir().join("toolchains"),
    )
}

/// Ensure every declared toolchain is installed, returning the `bin` directories
/// to prepend to `PATH`. Toolchains are installed once and reused.
pub async fn ensure_all(
    toolchains: &HashMap<String, ToolchainConfig>,
    dir: &Path,
) -> Result<Vec<PathBuf>> {
    // Deterministic order so PATH precedence is stable.
    let mut names: Vec<&String> = toolchains.keys().collect();
    names.sort();

    let mut bins = Vec::new();
    for name in names {
        let bin = ensure_one(name, &toolchains[name], dir).await?;
        bins.push(bin);
    }
    Ok(bins)
}

async fn ensure_one(name: &str, tc: &ToolchainConfig, dir: &Path) -> Result<PathBuf> {
    let install_dir = dir.join(name).join(&tc.version);
    let marker = install_dir.join(".yatr-installed");
    let bin = tc.bin.as_ref().map_or_else(
        || install_dir.clone(),
        |b| install_dir.join(render(b, &tc.version)),
    );

    if marker.exists() {
        return Ok(bin);
    }

    let url = render(&tc.url, &tc.version);
    download_and_extract(name, &url, &install_dir).await?;
    std::fs::write(&marker, &url).map_err(|e| err(name, format!("cannot write marker: {e}")))?;
    Ok(bin)
}

async fn download_and_extract(name: &str, url: &str, dest: &Path) -> Result<()> {
    let lower = url.to_ascii_lowercase();
    if !(lower.ends_with(".tar.gz") || lower.ends_with(".tgz")) {
        return Err(err(
            name,
            format!("unsupported archive format for '{url}' (only .tar.gz is supported)"),
        ));
    }

    let resp = reqwest::get(url)
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| err(name, format!("failed to download {url}: {e}")))?;
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| err(name, format!("failed to read {url}: {e}")))?;

    std::fs::create_dir_all(dest).map_err(|e| err(name, format!("cannot create dir: {e}")))?;
    let mut archive = tar::Archive::new(GzDecoder::new(&bytes[..]));
    archive
        .unpack(dest)
        .map_err(|e| err(name, format!("failed to extract archive: {e}")))?;
    Ok(())
}

fn err(tool: &str, message: String) -> YatrError {
    YatrError::Toolchain {
        tool: tool.to_string(),
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_templates() {
        let out = render("tool-{version}-{os}-{arch}.tar.gz", "1.2.3");
        assert!(out.starts_with("tool-1.2.3-"));
        assert!(out.ends_with(".tar.gz"));
        assert!(out.contains(target_os()) && out.contains(target_arch()));
    }

    #[tokio::test]
    async fn installs_and_caches_a_toolchain() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Build a tiny .tar.gz containing bin/hello.
        let mut tar_buf = Vec::new();
        {
            let enc = flate2::write::GzEncoder::new(&mut tar_buf, flate2::Compression::default());
            let mut builder = tar::Builder::new(enc);
            let body = b"#!/bin/sh\necho hi\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(body.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "bin/hello", &body[..])
                .unwrap();
            builder.into_inner().unwrap().finish().unwrap();
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/tool.tar.gz"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tar_buf))
            .expect(1) // installed once, then cached
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let mut toolchains = HashMap::new();
        toolchains.insert(
            "demo".to_string(),
            ToolchainConfig {
                version: "1.0.0".to_string(),
                url: format!("{}/tool.tar.gz", server.uri()),
                bin: Some("bin".to_string()),
            },
        );

        let bins = ensure_all(&toolchains, dir.path()).await.unwrap();
        assert_eq!(bins.len(), 1);
        assert!(
            bins[0].join("hello").is_file(),
            "extracted bin/hello should exist"
        );

        // Second call is served from cache (mock expects exactly one GET).
        let bins2 = ensure_all(&toolchains, dir.path()).await.unwrap();
        assert_eq!(bins, bins2);
    }
}
