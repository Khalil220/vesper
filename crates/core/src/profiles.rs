//! Built-in site profiles for the generic adapter.
//!
//! Each function returns a [`SiteProfile`] describing one site. These are the
//! first candidates to eventually move into external config files, but keeping
//! them in code for now makes them trivially testable.

use url::Url;

use crate::source::SiteProfile;

/// All built-in site profiles.
pub fn all() -> Vec<SiteProfile> {
    vec![novgo()]
}

/// The profile whose host matches `url`, if any.
pub fn for_url(url: &str) -> Option<SiteProfile> {
    let host = Url::parse(url).ok()?.host_str()?.to_string();
    all()
        .into_iter()
        .find(|p| host.eq_ignore_ascii_case(p.host))
}

/// Profile for novgo.net.
///
/// Verified against the live site: Cloudflare is CDN-only (Tier 1 fetch works),
/// pages are server-rendered, chapter bodies live in `#chapter-content`, and
/// ads are separate `div.ads*` blocks with no prose `<p>`, so selecting `<p>`
/// within the container excludes them.
pub fn novgo() -> SiteProfile {
    SiteProfile {
        name: "novgo",
        host: "novgo.net",
        content_selector: "#chapter-content",
        paragraph_selector: "p",
        max_pages: 500,
    }
}
