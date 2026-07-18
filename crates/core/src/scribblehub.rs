//! Hand-written adapter for scribblehub.com.
//!
//! ScribbleHub is the hardest of the sources: Cloudflare rejects requests
//! without a full browser header set (so it runs on the curl tier), and its
//! table of contents is a WordPress admin-ajax **POST** (`wi_getreleases_
//! pagination`), 15 chapters/page, newest-first. Chapter URLs use non-sequential
//! ids and the "Chapter N" numbers don't match the count (prologues/interludes),
//! so chapters are numbered by position, oldest-first.
//!
//! Series page gives `mypostid` and the total (`span.cnt_toc`); content is
//! `#chp_raw`.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use scraper::{Html, Selector};
use url::Url;

use crate::fetch::Fetcher;
use crate::model::{Chapter, ChapterRef, NovelMeta, NovelStatus};
use crate::source::{parse_chapter_body, parse_status_hint, Source};

const CONTENT_SELECTOR: &str = "#chp_raw";

pub struct ScribbleHubSource<F: Fetcher> {
    fetcher: F,
}

impl<F: Fetcher> ScribbleHubSource<F> {
    pub fn new(fetcher: F) -> Self {
        Self { fetcher }
    }
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("valid selector")
}

fn text_of(doc: &Html, selector: &str) -> Option<String> {
    let t = doc
        .select(&sel(selector))
        .next()?
        .text()
        .collect::<String>()
        .trim()
        .to_string();
    (!t.is_empty()).then_some(t)
}

fn meta_prop(doc: &Html, property: &str) -> Option<String> {
    doc.select(&sel(&format!("meta[property=\"{property}\"]")))
        .next()?
        .value()
        .attr("content")
        .map(|s| s.trim().to_string())
}

/// Strip a leading "Chapter N." / "N." numbering (we render our own prefix).
fn clean_title(raw: &str) -> String {
    let mut t = raw.trim();
    if t.len() >= 8 && t[..8].eq_ignore_ascii_case("chapter ") {
        t = t[8..].trim_start();
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &t[digits..];
        if let Some(after) = rest
            .strip_prefix('.')
            .or_else(|| rest.strip_prefix(':'))
            .or_else(|| rest.strip_prefix('-'))
        {
            let after = after.trim_start();
            if !after.is_empty() {
                return after.to_string();
            }
        }
    }
    t.to_string()
}

fn parse_novel(html: &str, source_url: &str) -> Result<NovelMeta> {
    let doc = Html::parse_document(html);
    let title = meta_prop(&doc, "og:title")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("could not find a novel title on {source_url}"))?;
    let cover_url = meta_prop(&doc, "og:image")
        .filter(|s| !s.is_empty() && !s.contains("noimagefound"));
    // Status is a `span.rnd_stats` whose text is a status keyword.
    let status_hint = doc
        .select(&sel("span.rnd_stats"))
        .map(|e| parse_status_hint(&e.text().collect::<String>()))
        .find(|s| *s != NovelStatus::Unknown)
        .unwrap_or(NovelStatus::Unknown);
    Ok(NovelMeta {
        title,
        author: text_of(&doc, "a[href*=\"/profile/\"]"),
        cover_url,
        genre: None,
        status_hint,
        source_url: source_url.to_string(),
    })
}

fn parse_post_id(html: &str) -> Option<String> {
    Html::parse_document(html)
        .select(&sel("#mypostid"))
        .next()?
        .value()
        .attr("value")
        .map(|s| s.trim().to_string())
}

/// Total chapter count from `span.cnt_toc`.
fn parse_total(html: &str) -> Option<u32> {
    let doc = Html::parse_document(html);
    let raw = text_of(&doc, ".cnt_toc")?;
    let digits: String = raw.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Parse `a.toc_a` chapter links from a ToC page: (absolute url, raw title).
fn parse_toc_links(html: &str) -> Vec<(String, String)> {
    let doc = Html::parse_document(html);
    doc.select(&sel("a.toc_a"))
        .filter_map(|e| {
            let href = e.value().attr("href")?.to_string();
            let title = e.text().collect::<String>().trim().to_string();
            Some((href, title))
        })
        .collect()
}

fn admin_ajax_url(url: &str) -> Result<String> {
    let host = Url::parse(url)?
        .host_str()
        .ok_or_else(|| anyhow!("no host in {url}"))?
        .to_string();
    Ok(format!("https://{host}/wp-admin/admin-ajax.php"))
}

fn toc_body(page: u32, post_id: &str) -> String {
    format!("action=wi_getreleases_pagination&pagenum={page}&mypostid={post_id}")
}

/// Turn a newest-first list of (url, raw_title) into oldest-first chapter refs
/// numbered from `start`.
fn to_refs(mut newest_first: Vec<(String, String)>, start: u32) -> Vec<ChapterRef> {
    newest_first.reverse();
    newest_first
        .into_iter()
        .enumerate()
        .map(|(i, (url, title))| ChapterRef {
            number: start + i as u32,
            title: clean_title(&title),
            url,
        })
        .collect()
}

#[async_trait]
impl<F: Fetcher> Source for ScribbleHubSource<F> {
    fn name(&self) -> &str {
        "scribblehub"
    }

    fn matches(&self, url: &str) -> bool {
        Url::parse(url)
            .ok()
            .and_then(|u| {
                u.host_str().map(|h| {
                    h.trim_start_matches("www.")
                        .eq_ignore_ascii_case("scribblehub.com")
                })
            })
            .unwrap_or(false)
    }

    async fn fetch_novel(&self, url: &str) -> Result<NovelMeta> {
        let html = self.fetcher.get(url).await?;
        parse_novel(&html, url)
    }

    async fn discover_chapters(&self, url: &str, _needed: Option<usize>) -> Result<Vec<ChapterRef>> {
        let series = self.fetcher.get(url).await?;
        let post_id = parse_post_id(&series).ok_or_else(|| anyhow!("no mypostid on {url}"))?;
        let total = parse_total(&series).unwrap_or(0) as usize;
        let ajax = admin_ajax_url(url)?;

        // POST each ToC page (newest-first). Stop at the total (so we never POST
        // an out-of-range page, which returns 403) or a short/empty page.
        let mut collected: Vec<(String, String)> = Vec::new();
        let mut page_size = 15usize;
        let mut page = 1u32;
        loop {
            let links = parse_toc_links(&self.fetcher.post(&ajax, &toc_body(page, &post_id)).await?);
            if links.is_empty() {
                break;
            }
            if page == 1 {
                page_size = links.len().max(1);
            }
            let got = links.len();
            collected.extend(links);
            if (total > 0 && collected.len() >= total) || got < page_size {
                break;
            }
            page += 1;
            if page > 1000 {
                break;
            }
        }
        if collected.is_empty() {
            return Err(anyhow!("no chapters found for {url}"));
        }
        Ok(to_refs(collected, 1))
    }

    /// Cheap delta check: just the newest ToC page, numbered to end at the total.
    async fn discover_latest(&self, url: &str) -> Result<Vec<ChapterRef>> {
        let series = self.fetcher.get(url).await?;
        let post_id = parse_post_id(&series).ok_or_else(|| anyhow!("no mypostid on {url}"))?;
        let total = parse_total(&series).unwrap_or(0);
        let ajax = admin_ajax_url(url)?;
        let links = parse_toc_links(&self.fetcher.post(&ajax, &toc_body(1, &post_id)).await?);
        let start = (total as usize)
            .checked_sub(links.len())
            .map(|n| n as u32 + 1)
            .unwrap_or(1);
        Ok(to_refs(links, start))
    }

    async fn fetch_chapter(&self, chapter: &ChapterRef) -> Result<Chapter> {
        let html = self.fetcher.get(&chapter.url).await?;
        let paragraphs = parse_chapter_body(&html, CONTENT_SELECTOR, "p")?;
        Ok(Chapter {
            number: chapter.number,
            title: chapter.title.clone(),
            paragraphs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_chapter_prefix() {
        assert_eq!(clean_title("Chapter 135. Sting like a bee"), "Sting like a bee");
        assert_eq!(clean_title("Chapter 1. Arrival."), "Arrival.");
        assert_eq!(clean_title("Prologue"), "Prologue");
    }

    #[test]
    fn parses_toc_links() {
        let html = r#"<div>
          <a class="toc_a" href="https://www.scribblehub.com/read/1-x/chapter/22/">Chapter 2. Two</a>
          <a class="toc_a" href="https://www.scribblehub.com/read/1-x/chapter/11/">Chapter 1. One</a>
        </div>"#;
        let links = parse_toc_links(html);
        assert_eq!(links.len(), 2);
        // to_refs reverses newest-first -> oldest-first and numbers from `start`.
        let refs = to_refs(links, 1);
        assert_eq!(refs[0].number, 1);
        assert_eq!(refs[0].title, "One");
        assert_eq!(refs[1].title, "Two");
    }

    #[test]
    fn parses_metadata_and_ids() {
        let html = r#"<html><head>
            <meta property="og:title" content="Farming Monster Girls">
            <meta property="og:image" content="https://www.scribblehub.com/img/noimagefound.jpg">
            </head><body>
            <a href="/profile/190510/firlinedboots/">FirLinedBoots</a>
            <input type="hidden" id="mypostid" value="1947818">
            <span class="cnt_toc">145</span>
            </body></html>"#;
        let meta = parse_novel(html, "https://www.scribblehub.com/series/1947818/x/").unwrap();
        assert_eq!(meta.title, "Farming Monster Girls");
        assert_eq!(meta.author.as_deref(), Some("FirLinedBoots"));
        assert_eq!(meta.cover_url, None, "noimagefound placeholder is not a cover");
        assert_eq!(parse_post_id(html).as_deref(), Some("1947818"));
        assert_eq!(parse_total(html), Some(145));
    }
}
