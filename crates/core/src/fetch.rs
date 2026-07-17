//! HTTP fetching, abstracted behind a trait so higher tiers (TLS-fingerprint
//! impersonation, headless browsers) can slot in per-site later.

use std::time::Duration;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use tokio::time::sleep;

/// Anything that can fetch a URL's HTML body.
///
/// Tier 1 is [`ReqwestFetcher`] (plain HTTP). Future tiers implement the same
/// trait, so callers never care which tier a site needs.
#[async_trait]
pub trait Fetcher: Send + Sync {
    async fn get(&self, url: &str) -> Result<String>;
}

/// Default browser-like User-Agent. novgo serves us fine even without this, but
/// sending a real one is basic politeness and avoids trivial UA filters.
const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Tier 1: a plain `reqwest` client with a fixed politeness delay applied
/// before every request.
///
/// The adaptive per-host backoff described in DESIGN.md is a later refinement;
/// this fixed delay is the honest floor for the first slice.
pub struct ReqwestFetcher {
    client: reqwest::Client,
    delay: Duration,
}

impl ReqwestFetcher {
    pub fn new(delay: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(DEFAULT_UA)
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self { client, delay })
    }
}

#[async_trait]
impl Fetcher for ReqwestFetcher {
    async fn get(&self, url: &str) -> Result<String> {
        sleep(self.delay).await;
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = resp.status();
        ensure!(status.is_success(), "GET {url} returned HTTP {status}");
        let body = resp
            .text()
            .await
            .with_context(|| format!("reading body of {url}"))?;
        Ok(body)
    }
}
