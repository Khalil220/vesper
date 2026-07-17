//! Built-in site profiles for the generic adapter.
//!
//! Each function returns a [`SiteProfile`] describing one site. These are the
//! first candidates to eventually move into external config files, but keeping
//! them in code for now makes them trivially testable.

use crate::source::SiteProfile;

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
