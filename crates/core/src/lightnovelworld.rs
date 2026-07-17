//! Hand-written adapter for lightnovelworld.org.
//!
//! Why hand-written: the table of contents is JavaScript-rendered (the static
//! `/chapters/?page=N` pages don't carry the chapter `<a>` items), and the
//! metadata is *not* in `og:novel:*` tags. But chapter URLs are sequential
//! (`/novel/<slug>/chapter/<n>/`) and the total count is on the novel page, so
//! discovery generates the whole list from one request — like freewebnovel.
//!
//! Metadata comes from page elements (`h1.novel-title`, `a.author-link`,
//! `.status-badge`) plus `og:image`. Chapter bodies live in `#chapterText`; the
//! `data-protected` flag is JS copy-blocking, not server-side obfuscation — the
//! prose is served as plain, readable `<p>` text.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use scraper::{Html, Selector};
use url::Url;

use crate::fetch::Fetcher;
use crate::model::{Chapter, ChapterRef, NovelMeta, NovelStatus};
use crate::source::{parse_chapter_body, parse_status_hint, Source};
use crate::util::clean_chapter_title;

const CONTENT_SELECTOR: &str = "#chapterText";
const PARAGRAPH_SELECTOR: &str = "p";

pub struct LightNovelWorldSource<F: Fetcher> {
    fetcher: F,
}

impl<F: Fetcher> LightNovelWorldSource<F> {
    pub fn new(fetcher: F) -> Self {
        Self { fetcher }
    }
}

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

fn text_of(doc: &Html, selector: &str) -> Option<String> {
    let sel = Selector::parse(selector).ok()?;
    let t = doc
        .select(&sel)
        .next()?
        .text()
        .collect::<String>()
        .trim()
        .to_string();
    (!t.is_empty()).then_some(t)
}

fn meta_prop(doc: &Html, property: &str) -> Option<String> {
    let sel = Selector::parse(&format!("meta[property=\"{property}\"]")).ok()?;
    doc.select(&sel)
        .next()?
        .value()
        .attr("content")
        .map(|s| s.trim().to_string())
}

fn parse_novel(html: &str, source_url: &str) -> Result<NovelMeta> {
    let doc = Html::parse_document(html);
    let title = text_of(&doc, "h1.novel-title")
        .ok_or_else(|| anyhow!("could not find a novel title on {source_url}"))?;
    Ok(NovelMeta {
        title,
        author: text_of(&doc, "a.author-link"),
        cover_url: meta_prop(&doc, "og:image"),
        status_hint: text_of(&doc, ".status-badge")
            .map(|s| parse_status_hint(&s))
            .unwrap_or(NovelStatus::Unknown),
        source_url: source_url.to_string(),
    })
}

/// Total chapter count from `og:title` ("<title> by <author> - <N> Chapters").
fn parse_total_chapters(html: &str) -> Option<u32> {
    let doc = Html::parse_document(html);
    let og_title = meta_prop(&doc, "og:title")?;
    let last_segment = og_title.rsplit(" - ").next()?; // "3105 Chapters"
    last_segment.split_whitespace().next()?.parse().ok()
}

/// The chapter's name from `h1.chapter-title`, which is
/// "Chapter N - [N:] Name"; strip our own "Chapter N" prefix and a duplicated
/// leading "N:".
fn parse_chapter_title(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let raw = text_of(&doc, "h1.chapter-title")?;
    let stripped = clean_chapter_title(&raw); // removes "Chapter N -/:"
    let cleaned = strip_leading_number_colon(&stripped);
    (!cleaned.is_empty()).then_some(cleaned)
}

fn strip_leading_number_colon(s: &str) -> String {
    let t = s.trim_start();
    let digits: usize = t.chars().take_while(|c| c.is_ascii_digit()).count();
    let rest = t[digits..].trim_start();
    if digits > 0 && rest.starts_with(':') {
        rest[1..].trim().to_string()
    } else {
        t.to_string()
    }
}

#[async_trait]
impl<F: Fetcher> Source for LightNovelWorldSource<F> {
    fn name(&self) -> &str {
        "lightnovelworld"
    }

    fn matches(&self, url: &str) -> bool {
        Url::parse(url)
            .ok()
            .and_then(|u| {
                u.host_str()
                    .map(|h| h.eq_ignore_ascii_case("lightnovelworld.org"))
            })
            .unwrap_or(false)
    }

    async fn fetch_novel(&self, url: &str) -> Result<NovelMeta> {
        let html = self.fetcher.get(url).await?;
        parse_novel(&html, url)
    }

    async fn discover_chapters(&self, url: &str, _needed: Option<usize>) -> Result<Vec<ChapterRef>> {
        let html = self.fetcher.get(url).await?;
        let total = parse_total_chapters(&html)
            .ok_or_else(|| anyhow!("could not read the chapter count on {url}"))?;
        let base = novel_base(url);
        Ok((1..=total)
            .map(|n| ChapterRef {
                number: n,
                title: format!("Chapter {n}"),
                url: format!("{base}/chapter/{n}/"),
            })
            .collect())
    }

    async fn fetch_chapter(&self, chapter: &ChapterRef) -> Result<Chapter> {
        let html = self.fetcher.get(&chapter.url).await?;
        let paragraphs = parse_chapter_body(&html, CONTENT_SELECTOR, PARAGRAPH_SELECTOR)?;
        let title = parse_chapter_title(&html).unwrap_or_else(|| chapter.title.clone());
        Ok(Chapter {
            number: chapter.number,
            title,
            paragraphs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOVEL_HTML: &str = r#"
        <html><head>
          <meta property="og:title" content="Shadow Slave by Guiltythree - 3105 Chapters">
          <meta property="og:image" content="https://lightnovelworld.org/cover.jpg">
        </head><body>
          <h1 class="novel-title">Shadow Slave</h1>
          <a href="/author/guiltythree/" class="author-link">Guiltythree</a>
          <span class="status-badge ongoing">Ongoing</span>
        </body></html>"#;

    #[test]
    fn parses_novel_metadata() {
        let meta = parse_novel(NOVEL_HTML, "https://lightnovelworld.org/novel/shadow-slave/").unwrap();
        assert_eq!(meta.title, "Shadow Slave");
        assert_eq!(meta.author.as_deref(), Some("Guiltythree"));
        assert_eq!(meta.cover_url.as_deref(), Some("https://lightnovelworld.org/cover.jpg"));
        assert_eq!(meta.status_hint, NovelStatus::Ongoing);
    }

    #[test]
    fn reads_total_chapters() {
        assert_eq!(parse_total_chapters(NOVEL_HTML), Some(3105));
    }

    #[test]
    fn generates_sequential_chapter_urls() {
        assert_eq!(
            novel_base("https://lightnovelworld.org/novel/shadow-slave/"),
            "https://lightnovelworld.org/novel/shadow-slave"
        );
    }

    #[test]
    fn cleans_doubled_chapter_title() {
        let html = "<html><body><h1 class=\"chapter-title\">Chapter 1 - 1: Nightmare Begins</h1></body></html>";
        assert_eq!(parse_chapter_title(html).as_deref(), Some("Nightmare Begins"));
    }

    #[test]
    fn strip_leading_number_colon_cases() {
        assert_eq!(strip_leading_number_colon("1: Nightmare Begins"), "Nightmare Begins");
        assert_eq!(strip_leading_number_colon("Just a Title"), "Just a Title");
    }
}
