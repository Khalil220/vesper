//! Core library for Vesper (a webnovel crawler): fetching, source adapters, chapter
//! extraction, and EPUB packaging. See DESIGN.md for the architecture and the
//! decisions behind it.

pub mod config;
pub mod epub;
pub mod fetch;
pub mod freewebnovel;
pub mod lightnovelworld;
pub mod model;
pub mod royalroad;
pub mod paths;
pub mod profiles;
pub mod source;
pub mod store;
pub mod sync;
pub mod util;

pub use config::Config;
pub use epub::{build_epub, download_cover, Cover};
pub use fetch::{CurlFetcher, FetchConfig, Fetcher, ReqwestFetcher};
pub use freewebnovel::FreewebnovelSource;
pub use lightnovelworld::LightNovelWorldSource;
pub use royalroad::RoyalRoadSource;
pub use model::{Chapter, ChapterRef, DerivedState, NovelMeta, NovelStatus};
pub use paths::{epub_path, novel_dir};
pub use source::{GenericSource, SiteProfile, Source};
pub use store::{default_db_path, Store, StoredNovel, StoredSource};
pub use sync::{sync_novel, SyncReport};
pub use util::sanitize_filename;

/// Resolve a URL to the right source adapter, constructing the fetch *tier* that
/// site needs (Tier 1 reqwest for most; the curl tier for freewebnovel, whose
/// Cloudflare challenges reqwest's TLS fingerprint). `delay` is the base
/// politeness delay. Returns `None` if no adapter handles the URL's host.
pub fn build_source(url: &str, delay: std::time::Duration) -> Option<Box<dyn Source>> {
    let host = ::url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    match host.as_str() {
        "freewebnovel.com" | "www.freewebnovel.com" => Some(Box::new(FreewebnovelSource::new(
            CurlFetcher::new(delay),
        ))),
        "lightnovelworld.org" | "www.lightnovelworld.org" => {
            let fetcher = ReqwestFetcher::new(delay).ok()?;
            Some(Box::new(LightNovelWorldSource::new(fetcher)))
        }
        "royalroad.com" | "www.royalroad.com" => {
            let fetcher = ReqwestFetcher::new(delay).ok()?;
            Some(Box::new(RoyalRoadSource::new(fetcher)))
        }
        _ => {
            let profile = profiles::for_url(url)?;
            let fetcher = ReqwestFetcher::new(delay).ok()?;
            Some(Box::new(GenericSource::new(profile, fetcher)))
        }
    }
}
