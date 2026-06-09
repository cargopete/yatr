//! HTTP client for a shared (remote) cache.
//!
//! Speaks a deliberately small REST protocol so it works against a plain object
//! store or a tiny server, and shares the path layout of Bazel's HTTP cache:
//!
//! - `GET  <url>/ac/<key>`   → action-result JSON, or 404
//! - `PUT  <url>/ac/<key>`   ← action-result JSON
//! - `HEAD <url>/cas/<blob>` → 200 if the blob exists, else 404
//! - `GET  <url>/cas/<blob>` → blob bytes, or 404
//! - `PUT  <url>/cas/<blob>` ← blob bytes
//!
//! All calls are fallible and the caller treats failures as non-fatal: a flaky
//! or absent remote must never break a build, only forgo the cache.

#![allow(clippy::missing_errors_doc)]

use std::time::Duration;

use reqwest::{Client, StatusCode};

use crate::config::RemoteCacheConfig;
use crate::error::{Result, YatrError};

/// Client for a remote cache backend.
#[derive(Debug, Clone)]
pub struct RemoteCache {
    client: Client,
    base: String,
    token: Option<String>,
    /// Read from the remote on a local miss
    pub read: bool,
    /// Write to the remote after a successful run
    pub write: bool,
}

impl RemoteCache {
    /// Build a remote cache client from config. The bearer token, if any, is
    /// read from the environment variable named by `token_env`.
    pub fn from_config(config: &RemoteCacheConfig) -> Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(15))
            // Fail fast when the remote is unreachable rather than stalling builds.
            .connect_timeout(Duration::from_secs(3))
            .user_agent(concat!("yatr/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| YatrError::Cache {
                message: format!("Failed to build remote cache client: {e}"),
            })?;

        let token = config
            .token_env
            .as_ref()
            .and_then(|var| std::env::var(var).ok());

        Ok(Self {
            client,
            base: config.url.trim_end_matches('/').to_string(),
            token,
            read: config.read,
            write: config.write,
        })
    }

    fn url(&self, kind: &str, id: &str) -> String {
        format!("{}/{kind}/{id}", self.base)
    }

    fn auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.token {
            Some(token) => req.bearer_auth(token),
            None => req,
        }
    }

    /// Fetch an action-result blob by cache key. `None` on a 404.
    pub async fn get_ac(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.get_bytes(&self.url("ac", key)).await
    }

    /// Upload an action-result blob under a cache key.
    pub async fn put_ac(&self, key: &str, body: Vec<u8>) -> Result<()> {
        self.put_bytes(&self.url("ac", key), body).await
    }

    /// Fetch a CAS blob by digest. `None` on a 404.
    pub async fn get_cas(&self, blob: &str) -> Result<Option<Vec<u8>>> {
        self.get_bytes(&self.url("cas", blob)).await
    }

    /// Upload a CAS blob.
    pub async fn put_cas(&self, blob: &str, body: Vec<u8>) -> Result<()> {
        self.put_bytes(&self.url("cas", blob), body).await
    }

    /// Check whether a CAS blob already exists remotely.
    pub async fn has_cas(&self, blob: &str) -> Result<bool> {
        let resp = self
            .auth(self.client.head(self.url("cas", blob)))
            .send()
            .await
            .map_err(Self::io)?;
        Ok(resp.status().is_success())
    }

    async fn get_bytes(&self, url: &str) -> Result<Option<Vec<u8>>> {
        let resp = self
            .auth(self.client.get(url))
            .send()
            .await
            .map_err(Self::io)?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp.error_for_status().map_err(Self::io)?;
        let bytes = resp.bytes().await.map_err(Self::io)?;
        Ok(Some(bytes.to_vec()))
    }

    async fn put_bytes(&self, url: &str, body: Vec<u8>) -> Result<()> {
        self.auth(self.client.put(url))
            .body(body)
            .send()
            .await
            .map_err(Self::io)?
            .error_for_status()
            .map_err(Self::io)?;
        Ok(())
    }

    fn io(e: reqwest::Error) -> YatrError {
        YatrError::Io(std::io::Error::other(e))
    }
}
