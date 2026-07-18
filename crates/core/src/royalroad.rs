//! Hand-written adapter for royalroad.com.
//!
//! RoyalRoad embeds the whole chapter list in the fiction page as a
//! `window.chapters = [...]` JSON array (id, title, url, order, visible), so
//! discovery is a single request — no ToC pagination — but chapter URLs use
//! non-sequential DB ids, so we must read the list rather than generate URLs.
//!
//! Content lives in `.chapter-inner`. RoyalRoad salts each chapter with **decoy
//! paragraphs**: a `<style>` block marks one (randomized-per-request) class as
//! `display: none`, and the decoy `<p>`s carry that class. We collect the
//! hidden classes and skip any `<p>` using them, keeping only visible prose.

use std::collections::HashSet;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use scraper::{Html, Selector};
use url::Url;

use crate::fetch::Fetcher;
use crate::model::{Chapter, ChapterRef, NovelMeta, NovelStatus};
use crate::source::{parse_status_hint, Source};

pub struct RoyalRoadSource<F: Fetcher> {
    fetcher: F,
}

impl<F: Fetcher> RoyalRoadSource<F> {
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

fn meta_name(doc: &Html, name: &str) -> Option<String> {
    doc.select(&sel(&format!("meta[name=\"{name}\"]")))
        .next()?
        .value()
        .attr("content")
        .map(|s| s.trim().to_string())
}

/// Strip a leading "N. " / "N: " numbering from a chapter title (we render our
/// own "Chapter N:" prefix).
fn clean_title(raw: &str) -> String {
    let t = raw.trim();
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &t[digits..];
        if let Some(after) = rest.strip_prefix('.').or_else(|| rest.strip_prefix(':')) {
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
        .or_else(|| {
            // "<title> | Royal Road" -> "<title>"
            text_of(&doc, "title").map(|t| {
                t.rsplit_once('|')
                    .map(|(head, _)| head.trim().to_string())
                    .unwrap_or(t)
            })
        })
        .or_else(|| text_of(&doc, "h1"))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("could not find a novel title on {source_url}"))?;

    let author = meta_name(&doc, "twitter:creator")
        .or_else(|| text_of(&doc, "a[href^=\"/profile/\"]"))
        .filter(|s| !s.is_empty());

    // Status is a `span.label` whose text is a status keyword (other labels are
    // genre tags).
    let status_hint = doc
        .select(&sel("span.label"))
        .map(|e| parse_status_hint(&e.text().collect::<String>()))
        .find(|s| *s != NovelStatus::Unknown)
        .unwrap_or(NovelStatus::Unknown);

    Ok(NovelMeta {
        title,
        author,
        cover_url: meta_prop(&doc, "og:image").filter(|s| !s.is_empty()),
        genre: None,
        status_hint,
        source_url: source_url.to_string(),
    })
}

/// Parse the `window.chapters = [...]` JSON array into ordered chapter refs.
fn parse_chapters(html: &str, base_url: &str) -> Result<Vec<ChapterRef>> {
    let idx = html
        .find("window.chapters")
        .ok_or_else(|| anyhow!("no window.chapters on the page"))?;
    let after = &html[idx..];
    let bracket = after
        .find('[')
        .ok_or_else(|| anyhow!("malformed chapter list"))?;
    // serde_json parses the first JSON value (the array) and ignores the
    // trailing `;` and rest of the script.
    let arr: serde_json::Value = serde_json::Deserializer::from_str(&after[bracket..])
        .into_iter::<serde_json::Value>()
        .next()
        .ok_or_else(|| anyhow!("empty chapter list"))?
        .map_err(|e| anyhow!("parsing window.chapters: {e}"))?;

    let base = Url::parse(base_url)?;
    let mut out = Vec::new();
    for ch in arr.as_array().ok_or_else(|| anyhow!("chapter list not an array"))? {
        if ch.get("visible").and_then(|v| v.as_i64()) == Some(0) {
            continue;
        }
        let Some(rel) = ch.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(abs) = base.join(rel) else { continue };
        let title = ch.get("title").and_then(|v| v.as_str()).unwrap_or("");
        out.push(ChapterRef {
            number: out.len() as u32 + 1,
            title: clean_title(title),
            url: abs.to_string(),
        });
    }
    if out.is_empty() {
        return Err(anyhow!("no visible chapters found"));
    }
    Ok(out)
}

/// CSS class names marked `display: none` in any `<style>` block — RoyalRoad's
/// decoy-paragraph classes.
fn decoy_classes(doc: &Html) -> HashSet<String> {
    let mut set = HashSet::new();
    for style in doc.select(&sel("style")) {
        let css = style.text().collect::<String>();
        for rule in css.split('}') {
            if rule.contains("display") && rule.contains("none") {
                if let Some(selector) = rule.split('{').next() {
                    for token in selector.split(|c: char| c == ',' || c.is_whitespace()) {
                        if let Some(cls) = token.trim().strip_prefix('.') {
                            if !cls.is_empty() {
                                set.insert(cls.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    set
}

fn parse_body(html: &str) -> Result<Vec<String>> {
    let doc = Html::parse_document(html);
    let decoy = decoy_classes(&doc);
    let content = doc
        .select(&sel(".chapter-inner"))
        .next()
        .ok_or_else(|| anyhow!("no .chapter-inner content container"))?;

    let p_sel = sel("p");
    let mut paragraphs = Vec::new();
    for p in content.select(&p_sel) {
        if p.value().classes().any(|c| decoy.contains(c)) {
            continue; // decoy paragraph
        }
        let text = p.text().collect::<String>().trim().to_string();
        if !text.is_empty() {
            paragraphs.push(text);
        }
    }
    if paragraphs.is_empty() {
        return Err(anyhow!("no paragraphs extracted from .chapter-inner"));
    }
    Ok(paragraphs)
}

#[async_trait]
impl<F: Fetcher> Source for RoyalRoadSource<F> {
    fn name(&self) -> &str {
        "royalroad"
    }

    fn matches(&self, url: &str) -> bool {
        Url::parse(url)
            .ok()
            .and_then(|u| {
                u.host_str().map(|h| {
                    let h = h.trim_start_matches("www.");
                    h.eq_ignore_ascii_case("royalroad.com")
                })
            })
            .unwrap_or(false)
    }

    async fn fetch_novel(&self, url: &str) -> Result<NovelMeta> {
        let html = self.fetcher.get(url).await?;
        parse_novel(&html, url)
    }

    async fn discover_chapters(&self, url: &str, _needed: Option<usize>) -> Result<Vec<ChapterRef>> {
        let html = self.fetcher.get(url).await?;
        parse_chapters(&html, url)
    }

    async fn fetch_chapter(&self, chapter: &ChapterRef) -> Result<Chapter> {
        let html = self.fetcher.get(&chapter.url).await?;
        let paragraphs = parse_body(&html)?;
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
    fn parses_window_chapters() {
        let html = r#"<html><body><script>
            window.chapters = [
              {"id":301778,"title":"1. Good Morning Brother","order":0,"visible":1,"url":"/fiction/21220/x/chapter/301778/1-good-morning-brother"},
              {"id":301779,"title":"2. Life's Problems","order":1,"visible":1,"url":"/fiction/21220/x/chapter/301779/2-life"},
              {"id":301780,"title":"Hidden","order":2,"visible":0,"url":"/fiction/21220/x/chapter/301780/h"}
            ];
            window.fictionInfo = {};
        </script></body></html>"#;
        let refs = parse_chapters(html, "https://www.royalroad.com/fiction/21220/x").unwrap();
        assert_eq!(refs.len(), 2, "visible:0 chapter skipped");
        assert_eq!(refs[0].number, 1);
        assert_eq!(refs[0].title, "Good Morning Brother"); // "1. " stripped
        assert_eq!(refs[1].title, "Life's Problems");
        assert!(refs[0].url.starts_with("https://www.royalroad.com/fiction/"));
    }

    #[test]
    fn skips_decoy_paragraphs() {
        let html = r#"<html><head><style>.deadbeef{ display: none; speak: never; }</style></head>
            <body><div class="chapter-inner chapter-content">
              <p class="aaa">Real paragraph one.</p>
              <p class="deadbeef">Stolen from RoyalRoad decoy text.</p>
              <p class="bbb">Real paragraph two.</p>
            </div></body></html>"#;
        let paras = parse_body(html).unwrap();
        assert_eq!(paras, vec!["Real paragraph one.", "Real paragraph two."]);
    }

    #[test]
    fn parses_metadata_and_status() {
        let html = r#"<html><head>
            <title>Mother of Learning | Royal Road</title>
            <meta name="twitter:creator" content="nobody103">
            <meta property="og:image" content="https://cdn/cover.jpg">
            </head><body>
            <span class="label">Fantasy</span>
            <span class="label label-default">COMPLETED</span>
            </body></html>"#;
        let meta = parse_novel(html, "https://www.royalroad.com/fiction/21220/x").unwrap();
        assert_eq!(meta.title, "Mother of Learning");
        assert_eq!(meta.author.as_deref(), Some("nobody103"));
        assert_eq!(meta.cover_url.as_deref(), Some("https://cdn/cover.jpg"));
        assert_eq!(meta.status_hint, NovelStatus::Completed);
    }
}
