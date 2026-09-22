
use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use scraper::{Html, Selector};
use url::Url;

use crate::fetch::Fetcher;
use crate::model::{Chapter, ChapterRef, NovelMeta, NovelStatus};
use crate::util::{clean_chapter_title, parse_chapter_number};

#[async_trait]
pub trait Source: Send + Sync {
    fn name(&self) -> &str;

    fn matches(&self, url: &str) -> bool;

    async fn fetch_novel(&self, url: &str) -> Result<NovelMeta>;

    async fn discover_chapters(&self, url: &str, needed: Option<usize>) -> Result<Vec<ChapterRef>>;

    async fn fetch_chapter(&self, chapter: &ChapterRef) -> Result<Chapter>;

    async fn discover_latest(&self, url: &str) -> Result<Vec<ChapterRef>> {
        self.discover_chapters(url, None).await
    }
}

#[derive(Debug, Clone)]
pub struct SiteProfile {
    pub name: String,
    pub host: String,
    pub content_selector: String,
    pub paragraph_selector: String,
    pub chapter_marker: String,
    pub page_param: String,
    pub max_pages: u32,
}

pub struct GenericSource<F: Fetcher> {
    profile: SiteProfile,
    fetcher: F,
}

impl<F: Fetcher> GenericSource<F> {
    pub fn new(profile: SiteProfile, fetcher: F) -> Self {
        Self { profile, fetcher }
    }
}

#[async_trait]
impl<F: Fetcher> Source for GenericSource<F> {
    fn name(&self) -> &str {
        &self.profile.name
    }

    fn matches(&self, url: &str) -> bool {
        Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.eq_ignore_ascii_case(&self.profile.host)))
            .unwrap_or(false)
    }

    async fn fetch_novel(&self, url: &str) -> Result<NovelMeta> {
        let html = self.fetcher.get(url).await?;
        parse_novel_meta(&html, url)
    }

    async fn discover_chapters(&self, url: &str, needed: Option<usize>) -> Result<Vec<ChapterRef>> {
        let mut found: BTreeMap<u32, ChapterRef> = BTreeMap::new();
        let mut page: u32 = 1;

        loop {
            let page_url = if page == 1 {
                url.to_string()
            } else {
                format!("{url}?{}={page}", self.profile.page_param)
            };
            let html = self.fetcher.get(&page_url).await?;
            let links = parse_chapter_links(&html, url, &self.profile.chapter_marker)?;

            if links.is_empty() {
                break;
            }

            let before = found.len();
            for c in links {
                found.entry(c.number).or_insert(c);
            }

            if let Some(n) = needed {
                if found.len() >= n {
                    break;
                }
            }
            if found.len() == before {
                break;
            }
            page += 1;
            if page > self.profile.max_pages {
                break;
            }
        }

        Ok(found.into_values().collect())
    }

    async fn fetch_chapter(&self, chapter: &ChapterRef) -> Result<Chapter> {
        let html = self.fetcher.get(&chapter.url).await?;
        let paragraphs = parse_chapter_body(
            &html,
            &self.profile.content_selector,
            &self.profile.paragraph_selector,
        )?;
        Ok(Chapter {
            number: chapter.number,
            title: chapter.title.clone(),
            paragraphs,
        })
    }

    async fn discover_latest(&self, url: &str) -> Result<Vec<ChapterRef>> {
        let html = self.fetcher.get(url).await?;
        parse_chapter_links(&html, url, &self.profile.chapter_marker)
    }
}

fn sel(selector: &str) -> Result<Selector> {
    Selector::parse(selector).map_err(|e| anyhow!("invalid selector {selector:?}: {e:?}"))
}

pub(crate) fn parse_status_hint(raw: &str) -> NovelStatus {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "ongoing" | "on going" | "serializing" | "active" | "releasing" => {
            NovelStatus::Ongoing
        }
        "2" | "completed" | "complete" | "finished" => NovelStatus::Completed,
        _ => NovelStatus::Unknown,
    }
}

fn meta_content(doc: &Html, property: &str) -> Option<String> {
    let selector = Selector::parse(&format!("meta[property=\"{property}\"]")).ok()?;
    doc.select(&selector)
        .next()?
        .value()
        .attr("content")
        .map(str::to_string)
}

pub(crate) fn parse_novel_meta(html: &str, source_url: &str) -> Result<NovelMeta> {
    let doc = Html::parse_document(html);

    let title = meta_content(&doc, "og:novel:novel_name")
        .or_else(|| meta_content(&doc, "og:title"))
        .or_else(|| {
            let t = sel("title").ok()?;
            doc.select(&t).next().map(|e| e.text().collect::<String>().trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("could not find a novel title on {source_url}"))?;

    let author = meta_content(&doc, "og:novel:author").filter(|s| !s.is_empty());
    let cover_url = meta_content(&doc, "og:image").filter(|s| !s.is_empty());
    let genre = meta_content(&doc, "og:novel:genre").filter(|s| !s.is_empty());

    let status_hint = meta_content(&doc, "og:novel:status")
        .map(|s| parse_status_hint(&s))
        .unwrap_or(NovelStatus::Unknown);

    Ok(NovelMeta {
        title,
        author,
        cover_url,
        genre,
        status_hint,
        source_url: source_url.to_string(),
    })
}

fn parse_chapter_links(html: &str, base_url: &str, marker: &str) -> Result<Vec<ChapterRef>> {
    let doc = Html::parse_document(html);
    let base = Url::parse(base_url).with_context(|| format!("parsing base URL {base_url}"))?;
    let anchors = sel("a[href]")?;

    let mut out = Vec::new();
    for a in doc.select(&anchors) {
        let Some(href) = a.value().attr("href") else {
            continue;
        };
        if !href.contains(marker) {
            continue;
        }
        let Ok(abs) = base.join(href) else {
            continue;
        };
        let abs = abs.to_string();
        let Some(number) = parse_chapter_number(&abs) else {
            continue;
        };
        let title = clean_chapter_title(&a.text().collect::<String>());
        out.push(ChapterRef { number, title, url: abs });
    }
    Ok(out)
}

pub(crate) fn parse_chapter_body(
    html: &str,
    content_selector: &str,
    paragraph_selector: &str,
) -> Result<Vec<String>> {
    let doc = Html::parse_document(html);
    let content_sel = sel(content_selector)?;
    let para_sel = sel(paragraph_selector)?;

    let content = doc
        .select(&content_sel)
        .next()
        .ok_or_else(|| anyhow!("no content container matching {content_selector}"))?;

    let mut paragraphs = Vec::new();
    for p in content.select(&para_sel) {
        let text = p.text().collect::<String>().trim().to_string();
        if !text.is_empty() {
            paragraphs.push(text);
        }
    }

    if paragraphs.is_empty() {
        return Err(anyhow!(
            "content container had no non-empty paragraphs (selector {paragraph_selector})"
        ));
    }
    Ok(paragraphs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHAPTER_HTML: &str = r#"
        <html><head><title>Ch 1</title></head><body>
          <div id="chapter-content" class="chapter-c">
            <p>First paragraph.</p>
            <div class="ads ads-holder"><ins>an advert</ins></div>
            <p>  Second paragraph.  </p>
            <p></p>
          </div>
        </body></html>"#;

    const NOVEL_HTML: &str = r#"
        <html><head>
          <meta property="og:novel:novel_name" content="Cultivation Online">
          <meta property="og:novel:author" content="MyLittleBrother">
          <meta property="og:image" content="https://example.com/cover.jpg">
          <meta property="og:novel:status" content="Ongoing">
        </head><body>
          <a href="/cultivation-online-novel/chapter-1-a.html">Chapter 1 - A</a>
          <a href="/cultivation-online-novel/chapter-2-b.html">Chapter 2 - B</a>
          <a href="/cultivation-online-novel/chapter-2-b.html">Chapter 2 - B (dup)</a>
          <a href="/some/other/page.html">Not a chapter</a>
        </body></html>"#;

    #[test]
    fn extracts_paragraphs_and_skips_ads_and_blanks() {
        let paras = parse_chapter_body(CHAPTER_HTML, "#chapter-content", "p").unwrap();
        assert_eq!(paras, vec!["First paragraph.", "Second paragraph."]);
    }

    #[test]
    fn extracts_novel_metadata() {
        let meta = parse_novel_meta(NOVEL_HTML, "https://example.com/cultivation-online-novel.html")
            .unwrap();
        assert_eq!(meta.title, "Cultivation Online");
        assert_eq!(meta.author.as_deref(), Some("MyLittleBrother"));
        assert_eq!(meta.cover_url.as_deref(), Some("https://example.com/cover.jpg"));
        assert_eq!(meta.status_hint, NovelStatus::Ongoing);
    }

    #[test]
    fn only_finished_labels_count_as_completed() {
        assert_eq!(parse_status_hint("releasing"), NovelStatus::Ongoing);
        assert_eq!(parse_status_hint("Ongoing"), NovelStatus::Ongoing);
        assert_eq!(parse_status_hint("completed"), NovelStatus::Completed);
        assert_eq!(parse_status_hint("1"), NovelStatus::Ongoing);
        assert_eq!(parse_status_hint("2"), NovelStatus::Completed);
        for paused in ["hiatus", "cancelled", "dropped", ""] {
            assert_eq!(
                parse_status_hint(paused),
                NovelStatus::Unknown,
                "{paused:?} must not be treated as completed"
            );
        }
    }

    #[test]
    fn parses_and_resolves_chapter_links() {
        let links =
            parse_chapter_links(NOVEL_HTML, "https://example.com/cultivation-online-novel.html", "/chapter-")
                .unwrap();
        assert_eq!(links.len(), 3);
        assert!(links
            .iter()
            .all(|c| c.url.starts_with("https://example.com/")));
        assert_eq!(links[0].number, 1);
    }
}
