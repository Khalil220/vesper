//! HTTP fetching, abstracted behind a trait so higher tiers (TLS-fingerprint
//! impersonation, headless browsers) can slot in per-site later.
//!
//! [`ReqwestFetcher`] is Tier 1: a plain `reqwest` client with **adaptive,
//! per-host politeness**. Each host has a current request spacing that starts at
//! a base delay, grows when the server pushes back (HTTP 429/503, `Retry-After`)
//! and relaxes toward the base on sustained success. Throttling and transient
//! network errors are retried with exponential backoff, bounded by
//! `max_retries`. This is the polite-and-optimal strategy from DESIGN.md: never
//! hammer, back off on resistance, and never escalate the site into a harder
//! anti-bot tier.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, ACCEPT_LANGUAGE, RETRY_AFTER};
use tokio::time::sleep;

/// Anything that can fetch a URL's HTML body.
///
/// Tier 1 is [`ReqwestFetcher`]. Future tiers implement the same trait, so
/// callers never care which tier a site needs.
#[async_trait]
pub trait Fetcher: Send + Sync {
    async fn get(&self, url: &str) -> Result<String>;
}

/// Default browser-like User-Agent. novgo serves us fine even without this, but
/// sending a real one is basic politeness and avoids trivial UA filters.
const DEFAULT_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
    AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Tunable fetch behaviour.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// Polite floor for request spacing per host.
    pub base_delay: Duration,
    /// Ceiling that adaptive backoff never exceeds.
    pub max_delay: Duration,
    /// Maximum retry attempts for throttling / transient errors.
    pub max_retries: u32,
}

impl FetchConfig {
    /// Sensible defaults derived from a base delay.
    pub fn new(base_delay: Duration) -> Self {
        let max_delay = std::cmp::max(base_delay.saturating_mul(16), Duration::from_secs(30));
        Self {
            base_delay,
            max_delay,
            max_retries: 4,
        }
    }
}

/// Tier 1: a plain `reqwest` client with adaptive per-host rate control.
pub struct ReqwestFetcher {
    client: reqwest::Client,
    config: FetchConfig,
    /// Current polite spacing per host; grows on throttling, relaxes on success.
    host_delay: Mutex<HashMap<String, Duration>>,
}

impl ReqwestFetcher {
    pub fn new(base_delay: Duration) -> Result<Self> {
        Self::with_config(FetchConfig::new(base_delay))
    }

    pub fn with_config(config: FetchConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .user_agent(DEFAULT_UA)
            .default_headers(browser_headers())
            .timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            config,
            host_delay: Mutex::new(HashMap::new()),
        })
    }

    fn current_delay(&self, host: &str) -> Duration {
        let map = self.host_delay.lock().expect("host_delay poisoned");
        map.get(host).copied().unwrap_or(self.config.base_delay)
    }

    fn bump_delay(&self, host: &str) {
        let mut map = self.host_delay.lock().expect("host_delay poisoned");
        let cur = map.get(host).copied().unwrap_or(self.config.base_delay);
        let next = std::cmp::min(self.config.max_delay, cur.saturating_mul(2));
        map.insert(host.to_string(), next);
    }

    fn relax_delay(&self, host: &str) {
        let mut map = self.host_delay.lock().expect("host_delay poisoned");
        let cur = map.get(host).copied().unwrap_or(self.config.base_delay);
        if cur > self.config.base_delay {
            let relaxed = std::cmp::max(self.config.base_delay, cur.mul_f64(0.85));
            map.insert(host.to_string(), relaxed);
        }
    }
}

#[async_trait]
impl Fetcher for ReqwestFetcher {
    async fn get(&self, url: &str) -> Result<String> {
        let host = host_of(url).unwrap_or_default();
        let mut attempt = 0u32;

        loop {
            let spacing = self.current_delay(&host);
            sleep(spacing + jitter(spacing)).await;

            match self.client.get(url).send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        self.relax_delay(&host);
                        return resp
                            .text()
                            .await
                            .with_context(|| format!("reading body of {url}"));
                    }
                    if is_retryable_status(status.as_u16()) && attempt < self.config.max_retries {
                        let wait = resp
                            .headers()
                            .get(RETRY_AFTER)
                            .and_then(|v| v.to_str().ok())
                            .and_then(parse_retry_after)
                            .unwrap_or_else(|| {
                                backoff_delay(self.config.base_delay, attempt, self.config.max_delay)
                            });
                        self.bump_delay(&host);
                        attempt += 1;
                        eprintln!(
                            "  (throttled HTTP {} on {url}; backing off {:.1}s, retry {}/{})",
                            status.as_u16(),
                            wait.as_secs_f64(),
                            attempt,
                            self.config.max_retries
                        );
                        sleep(wait).await;
                        continue;
                    }
                    bail!("GET {url} returned HTTP {status}");
                }
                Err(e) => {
                    let transient = e.is_timeout() || e.is_connect() || e.is_request();
                    if transient && attempt < self.config.max_retries {
                        let wait =
                            backoff_delay(self.config.base_delay, attempt, self.config.max_delay);
                        attempt += 1;
                        eprintln!(
                            "  (network error on {url}: {e}; retry {}/{} in {:.1}s)",
                            attempt,
                            self.config.max_retries,
                            wait.as_secs_f64()
                        );
                        sleep(wait).await;
                        continue;
                    }
                    return Err(anyhow!(e).context(format!("GET {url}")));
                }
            }
        }
    }
}

/// Tier 2: shell out to the system `curl`.
///
/// Some Cloudflare-fronted sites (freewebnovel) challenge reqwest's TLS
/// ClientHello fingerprint but accept curl's — even though both use Schannel on
/// Windows. Rather than pull in a heavyweight TLS-impersonation stack, we defer
/// to `curl`, which ships with Windows 10+ (and virtually everywhere else). Same
/// politeness contract as Tier 1: base delay + jitter and bounded backoff
/// retries on throttling / transient failures.
pub struct CurlFetcher {
    config: FetchConfig,
}

impl CurlFetcher {
    pub fn new(base_delay: Duration) -> Self {
        Self {
            config: FetchConfig::new(base_delay),
        }
    }
}

#[async_trait]
impl Fetcher for CurlFetcher {
    async fn get(&self, url: &str) -> Result<String> {
        let mut attempt = 0u32;
        loop {
            sleep(self.config.base_delay + jitter(self.config.base_delay)).await;

            let out = tokio::process::Command::new("curl")
                .args([
                    "-sS",
                    "--compressed",
                    "--http1.1",
                    "--max-time",
                    "30",
                    "-A",
                    DEFAULT_UA,
                    "-H",
                    "Accept: text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
                    "-H",
                    "Accept-Language: en-US,en;q=0.9",
                    "-w",
                    "\n%{http_code}",
                    url,
                ])
                .output()
                .await
                .context("running curl (is it installed and on PATH?)")?;

            // stdout is the body followed by "\n<status>" (from -w).
            let stdout = String::from_utf8_lossy(&out.stdout);
            let (body, status) = stdout.rsplit_once('\n').unwrap_or((stdout.as_ref(), ""));
            let code: u16 = status.trim().parse().unwrap_or(0);

            if code / 100 == 2 {
                return Ok(body.to_string());
            }

            // code == 0 means curl itself failed (network/timeout).
            let retryable = code == 0 || is_retryable_status(code);
            if retryable && attempt < self.config.max_retries {
                let wait = backoff_delay(self.config.base_delay, attempt, self.config.max_delay);
                attempt += 1;
                let what = if code == 0 {
                    format!("error: {}", String::from_utf8_lossy(&out.stderr).trim())
                } else {
                    format!("HTTP {code}")
                };
                eprintln!(
                    "  (curl {what} on {url}; retry {}/{} in {:.1}s)",
                    attempt,
                    self.config.max_retries,
                    wait.as_secs_f64()
                );
                sleep(wait).await;
                continue;
            }

            if code == 0 {
                bail!(
                    "curl failed for {url}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            bail!("GET {url} returned HTTP {code}");
        }
    }
}

/// A consistent, browser-like default header set. Some Cloudflare-fronted sites
/// (e.g. freewebnovel) reject requests that carry a browser User-Agent but not
/// the matching navigation and client-hint headers. Keep these consistent with
/// `DEFAULT_UA` (Chrome 126 on Windows).
fn browser_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        ACCEPT,
        HeaderValue::from_static(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        ),
    );
    h.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en;q=0.9"));
    h.insert("Upgrade-Insecure-Requests", HeaderValue::from_static("1"));
    h.insert("Sec-Fetch-Dest", HeaderValue::from_static("document"));
    h.insert("Sec-Fetch-Mode", HeaderValue::from_static("navigate"));
    h.insert("Sec-Fetch-Site", HeaderValue::from_static("none"));
    h.insert("Sec-Fetch-User", HeaderValue::from_static("?1"));
    h.insert(
        "sec-ch-ua",
        HeaderValue::from_static("\"Chromium\";v=\"126\", \"Google Chrome\";v=\"126\", \"Not.A/Brand\";v=\"24\""),
    );
    h.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
    h.insert("sec-ch-ua-platform", HeaderValue::from_static("\"Windows\""));
    h
}

/// Host portion of a URL, for keying per-host rate state.
fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(str::to_string)
}

/// Statuses worth retrying: rate-limit and transient server errors.
fn is_retryable_status(code: u16) -> bool {
    matches!(code, 429 | 500 | 502 | 503 | 504)
}

/// Exponential backoff: `base * 2^(attempt+1)`, capped at `max`.
fn backoff_delay(base: Duration, attempt: u32, max: Duration) -> Duration {
    let shift = (attempt + 1).min(16);
    let factor = 1u32 << shift;
    std::cmp::min(max, base.saturating_mul(factor))
}

/// Parse a `Retry-After` header. Only the delta-seconds form is honoured; the
/// HTTP-date form is ignored (the caller falls back to computed backoff).
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Add up to +40% random jitter to a delay, so requests aren't metronomic
/// (a bot tell). Time-seeded — quality is irrelevant here.
fn jitter(base: Duration) -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let frac = (nanos % 1000) as f64 / 1000.0;
    base.mul_f64(0.4 * frac)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_host() {
        assert_eq!(
            host_of("https://novgo.net/x/chapter-1.html").as_deref(),
            Some("novgo.net")
        );
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn retryable_statuses() {
        for code in [429, 500, 502, 503, 504] {
            assert!(is_retryable_status(code), "{code} should retry");
        }
        for code in [200, 301, 400, 403, 404] {
            assert!(!is_retryable_status(code), "{code} should not retry");
        }
    }

    #[test]
    fn backoff_grows_and_caps() {
        let base = Duration::from_millis(1000);
        let max = Duration::from_secs(30);
        assert_eq!(backoff_delay(base, 0, max), Duration::from_secs(2));
        assert_eq!(backoff_delay(base, 1, max), Duration::from_secs(4));
        assert_eq!(backoff_delay(base, 2, max), Duration::from_secs(8));
        // Caps at max rather than overflowing.
        assert_eq!(backoff_delay(base, 20, max), max);
    }

    #[test]
    fn parses_retry_after_seconds() {
        assert_eq!(parse_retry_after("30"), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after("  5 "), Some(Duration::from_secs(5)));
        // HTTP-date form is not parsed.
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let base = Duration::from_millis(1000);
        for _ in 0..100 {
            let j = jitter(base);
            assert!(j <= base.mul_f64(0.4), "jitter {j:?} exceeded 40%");
        }
    }
}
