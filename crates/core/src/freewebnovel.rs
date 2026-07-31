//! Hand-written adapter for freewebnovel.com.
//!
//! freewebnovel doesn't fit the generic profile: its table of contents is
//! paginated by JavaScript/AJAX (the static dropdown options are placeholder
//! URLs), so there's no scrapable `?page=N`. But it doesn't need one — chapter
//! URLs are sequential (`/novel/<slug>/chapter-<n>`) and the landing page
//! exposes `data-total-chapters`, so we generate the whole chapter list from a
//! single request. Metadata and chapter bodies reuse the shared extractors.
//!
//! Cloudflare here gates on User-Agent (a browser UA returns 200; a bot UA gets
//! a challenge), which the Tier-1 fetcher already sends — so no higher tier is
//! needed.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use scraper::{Html, Selector};
use url::Url;

use crate::fetch::Fetcher;
use crate::model::{Chapter, ChapterRef, NovelMeta};
use crate::source::{parse_chapter_body, parse_novel_meta, Source};
use crate::util::clean_chapter_title;

const CONTENT_SELECTOR: &str = ".txt";
const PARAGRAPH_SELECTOR: &str = "p";

pub struct FreewebnovelSource<F: Fetcher> {
    fetcher: F,
}

impl<F: Fetcher> FreewebnovelSource<F> {
    pub fn new(fetcher: F) -> Self {
        Self { fetcher }
    }
}

/// Strip any query/fragment from the novel URL so we can append `/chapter-N`.
fn novel_base(url: &str) -> String {
    match Url::parse(url) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            u.as_str().trim_end_matches('/').to_string()
        }
        Err(_) => url.trim_end_matches('/').to_string(),
    }
}

#[async_trait]
impl<F: Fetcher> Source for FreewebnovelSource<F> {
    fn name(&self) -> &str {
        "freewebnovel"
    }

    fn matches(&self, url: &str) -> bool {
        Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.eq_ignore_ascii_case("freewebnovel.com")))
            .unwrap_or(false)
    }

    async fn fetch_novel(&self, url: &str) -> Result<NovelMeta> {
        let html = self.fetcher.get(url).await?;
        parse_novel_meta(&html, url)
    }

    async fn discover_chapters(&self, url: &str, _needed: Option<usize>) -> Result<Vec<ChapterRef>> {
        let html = self.fetcher.get(url).await?;
        let total = parse_total_chapters(&html)
            .ok_or_else(|| anyhow!("could not read data-total-chapters on {url}"))?;
        let base = novel_base(url);
        Ok((1..=total)
            .map(|n| ChapterRef {
                number: n,
                title: format!("Chapter {n}"),
                url: format!("{base}/chapter-{n}"),
            })
            .collect())
    }

    async fn fetch_chapter(&self, chapter: &ChapterRef) -> Result<Chapter> {
        let html = self.fetcher.get(&chapter.url).await?;
        let paragraphs = parse_chapter_body(&html, CONTENT_SELECTOR, PARAGRAPH_SELECTOR)?;
        // Prefer the real title from the page; fall back to the placeholder.
        let title = parse_chapter_title(&html).unwrap_or_else(|| chapter.title.clone());
        Ok(Chapter {
            number: chapter.number,
            title,
            paragraphs,
        })
    }
}

/// Read `data-total-chapters="N"` from the landing page.
fn parse_total_chapters(html: &str) -> Option<u32> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("[data-total-chapters]").ok()?;
    doc.select(&sel)
        .next()?
        .value()
        .attr("data-total-chapters")?
        .trim()
        .parse()
        .ok()
}

/// Pull the chapter's name from the `<title>`, which looks like
/// "Novel - Chapter N | Name | Free Web Novel" (some older chapters separate
/// the name with a space or a dash instead of the pipe). Anchors on
/// " - Chapter " so a novel name containing " - " doesn't break it.
///
/// The site name is trimmed off the *end*, not by splitting on the first `|` —
/// the pipe before the chapter name is the same character, so splitting from
/// the front threw the name away and left a bare "Chapter N".
fn parse_chapter_title(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("title").ok()?;
    let raw = doc.select(&sel).next()?.text().collect::<String>();
    let no_suffix = strip_site_suffix(raw.trim());
    let after = no_suffix.find(" - Chapter ").map(|i| &no_suffix[i + 3..])?;
    let cleaned = clean_chapter_title(after.trim());
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Drop the trailing "| Free Web Novel" branding, leaving any earlier `|` (the
/// one separating "Chapter N" from its name) intact.
fn strip_site_suffix(title: &str) -> &str {
    match title.rsplit_once('|') {
        Some((head, tail)) if tail.trim().eq_ignore_ascii_case("Free Web Novel") => head.trim(),
        _ => title,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOVEL_HTML: &str = r#"
        <html><head>
          <meta property="og:novel:novel_name" content="The Bloodline System">
          <meta property="og:novel:author" content="Timvic">
          <meta property="og:novel:status" content="Completed">
          <meta property="og:image" content="https://freewebnovel.com/cover.jpg">
        </head><body>
          <div id="indexListPage" data-total-chapters="1688" data-total-page="43"></div>
        </body></html>"#;

    #[test]
    fn reads_total_chapters() {
        assert_eq!(parse_total_chapters(NOVEL_HTML), Some(1688));
    }

    #[test]
    fn novel_base_strips_query() {
        assert_eq!(
            novel_base("https://freewebnovel.com/novel/the-bloodline-system?page=2"),
            "https://freewebnovel.com/novel/the-bloodline-system"
        );
    }

    fn title_html(inner: &str) -> String {
        format!("<html><head><title>{inner}</title></head><body></body></html>")
    }

    #[test]
    fn parses_chapter_title_from_title_tag() {
        let html = title_html("The Bloodline System - Chapter 1 - How It All Began | Free Web Novel");
        assert_eq!(parse_chapter_title(&html).as_deref(), Some("How It All Began"));
    }

    /// The common freewebnovel shape: the chapter name is separated from
    /// "Chapter N" by a pipe, the same character the site-name suffix uses.
    #[test]
    fn parses_chapter_title_separated_by_pipe() {
        let html = title_html(
            "Investing In My Three Crippled Wives Get 10,000x Times Return - Chapter 1 | The Three Wives | Free Web Novel",
        );
        assert_eq!(parse_chapter_title(&html).as_deref(), Some("The Three Wives"));
    }

    /// A name containing a comma/pipe-free run still survives the end-anchored
    /// suffix strip.
    #[test]
    fn parses_chapter_title_with_punctuation() {
        let html = title_html(
            "Investing In My Three Crippled Wives Get 10,000x Times Return - Chapter 62 | Trouble, Heading To The Hero Association | Free Web Novel",
        );
        assert_eq!(
            parse_chapter_title(&html).as_deref(),
            Some("Trouble, Heading To The Hero Association")
        );
    }

    /// Some chapters omit the separator entirely.
    #[test]
    fn parses_chapter_title_separated_by_space() {
        let html = title_html(
            "Investing In My Three Crippled Wives Get 10,000x Times Return - Chapter 61 Gifts & Forgiveness | Free Web Novel",
        );
        assert_eq!(parse_chapter_title(&html).as_deref(), Some("Gifts & Forgiveness"));
    }

    /// No name on the page — the caller's "Chapter N" placeholder is what we
    /// end up with either way.
    #[test]
    fn unnamed_chapter_falls_back_to_number() {
        let html = title_html("The Bloodline System - Chapter 5 | Free Web Novel");
        assert_eq!(parse_chapter_title(&html).as_deref(), Some("Chapter 5"));
    }

    #[test]
    fn strips_only_the_site_suffix() {
        assert_eq!(strip_site_suffix("A - Chapter 1 | Name | Free Web Novel"), "A - Chapter 1 | Name");
        assert_eq!(strip_site_suffix("A - Chapter 1 | Name"), "A - Chapter 1 | Name");
    }

    #[test]
    fn status_word_form_parses_completed() {
        let meta = parse_novel_meta(NOVEL_HTML, "https://freewebnovel.com/novel/the-bloodline-system").unwrap();
        assert_eq!(meta.title, "The Bloodline System");
        assert_eq!(meta.author.as_deref(), Some("Timvic"));
        assert_eq!(meta.status_hint, crate::model::NovelStatus::Completed);
    }
}
