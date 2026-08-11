//! Core library for Vesper (a webnovel crawler): fetching, source adapters, chapter
//! extraction, and EPUB packaging. See DESIGN.md for the architecture and the
//! decisions behind it.

pub mod chikari;
pub mod config;
pub mod epub;
pub mod fetch;
pub mod freewebnovel;
pub mod lightnovelworld;
pub mod migrate;
pub mod model;
pub mod royalroad;
pub mod scribblehub;
pub mod paths;
pub mod profiles;
pub mod repair;
pub mod source;
pub mod store;
pub mod sync;
pub mod util;

pub use chikari::ChikariSource;
pub use config::Config;
pub use epub::{build_epub, download_cover, Cover};
pub use fetch::{is_not_found, CurlFetcher, FetchConfig, Fetcher, NotFound, ReqwestFetcher};
pub use freewebnovel::FreewebnovelSource;
pub use lightnovelworld::LightNovelWorldSource;
pub use migrate::{migrate_lightnovelworld, MigrationOutcome, MigrationReport};
pub use royalroad::RoyalRoadSource;
pub use scribblehub::ScribbleHubSource;
pub use model::{Chapter, ChapterRef, DerivedState, NovelMeta, NovelStatus};
pub use paths::{epub_path, novel_dir};
pub use repair::{looks_like_gate_stub, repair_novel, RepairReport};
pub use source::{GenericSource, SiteProfile, Source};
pub use store::{default_db_path, Store, StoredNovel, StoredSource};
pub use sync::{sync_novel, SyncProgress, SyncReport};
pub use util::sanitize_filename;

/// Resolve a URL to the right source adapter, constructing the fetch *tier* that
/// site needs (Tier 1 reqwest for most; the curl tier for freewebnovel, whose
/// Cloudflare challenges reqwest's TLS fingerprint). `delay` is the base
/// politeness delay. Returns `None` if no adapter handles the URL's host.
pub fn build_source(url: &str, delay: std::time::Duration) -> Option<Box<dyn Source>> {
    let host = ::url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    match host.as_str() {
        // chikari serves plain JSON through its public API, so Tier 1 is enough
        // (its Cloudflare front doesn't challenge a normal browser header set).
        "chikari.moe" | "www.chikari.moe" => {
            let fetcher = ReqwestFetcher::new(delay).ok()?;
            Some(Box::new(ChikariSource::new(fetcher)))
        }
        "freewebnovel.com" | "www.freewebnovel.com" => Some(Box::new(FreewebnovelSource::new(
            CurlFetcher::new(delay),
        ))),
        // Kept so a lingering subscription still resolves to an adapter and
        // fails with a real message, but the site is frozen and shutting down
        // (see the site-notice redirect in `lightnovelworld`).
        "lightnovelworld.org" | "www.lightnovelworld.org" => {
            let fetcher = ReqwestFetcher::new(delay).ok()?;
            Some(Box::new(LightNovelWorldSource::new(fetcher)))
        }
        "royalroad.com" | "www.royalroad.com" => {
            let fetcher = ReqwestFetcher::new(delay).ok()?;
            Some(Box::new(RoyalRoadSource::new(fetcher)))
        }
        // ScribbleHub needs the curl tier (Cloudflare + POST-based AJAX ToC).
        "scribblehub.com" | "www.scribblehub.com" => {
            Some(Box::new(ScribbleHubSource::new(CurlFetcher::new(delay))))
        }
        _ => {
            let profile = profiles::for_url(url)?;
            let fetcher = ReqwestFetcher::new(delay).ok()?;
            Some(Box::new(GenericSource::new(profile, fetcher)))
        }
    }
}
