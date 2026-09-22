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

/// Words the promo lines put between the pitch and the site's name.
const PROMO_PREPOSITIONS: &[&str] = &["on", "at", "from", "with", "by", "through", "via", "to"];

/// Longest a promo line runs. The observed ones are three to seven words.
const PROMO_MAX_WORDS: usize = 10;

pub struct FreewebnovelSource<F: Fetcher> {
    fetcher: F,
}

impl<F: Fetcher> FreewebnovelSource<F> {
    pub fn new(fetcher: F) -> Self {
        Self { fetcher }
    }
}

/// Whether a word is the site naming itself, spelled as the marks spell it.
///
/// `empire` (a sister site) only counts in lower case: novels are full of prose
/// about an in-story "Empire", and the capital is what tells the two apart.
/// `freewebnovel` needs no such care, since no story says it.
fn is_site_name(word: &str) -> bool {
    let trimmed = word.trim_end_matches(['.', '!', ',']);
    let base = trimmed.strip_suffix(".com").unwrap_or(trimmed);
    base.eq_ignore_ascii_case("freewebnovel") || base == "empire"
}

/// Whether `text` is exactly one of the site's promo lines, e.g. "Enjoy
/// exclusive adventures from freewebnovel" or "Updates by Freewebnovel. com".
///
/// The shape is fixed: a short pitch, a preposition, then the site's name at
/// the very end. Requiring the name to come last is what keeps real sentences
/// safe, because prose that mentions a site name carries on past it.
fn is_promo(text: &str) -> bool {
    let mut words: Vec<&str> = text.split_whitespace().collect();
    // The injection sometimes arrives with a separator glued to its front.
    while words
        .first()
        .is_some_and(|w| !w.chars().any(char::is_alphanumeric))
    {
        words.remove(0);
    }
    if words.len() < 2 || words.len() > PROMO_MAX_WORDS {
        return false;
    }

    let (last, head) = words.split_last().expect("checked above");
    // "Freewebnovel. com" splits the domain across two words.
    let (site, rest) = match head.split_last() {
        Some((prev, before)) if last.eq_ignore_ascii_case("com") && prev.ends_with('.') => {
            (format!("{prev}com"), before)
        }
        _ => ((*last).to_string(), head),
    };
    if !is_site_name(&site) {
        return false;
    }

    let Some((preposition, pitch)) = rest.split_last() else {
        return false;
    };
    PROMO_PREPOSITIONS
        .iter()
        .any(|p| preposition.eq_ignore_ascii_case(p))
        && pitch
            .iter()
            .all(|w| w.chars().all(|c| c.is_alphabetic() || "'\u{2019}-".contains(c)))
}

/// Where an appended promo starts, if the paragraph ends with one.
///
/// The mark is tacked on after a sentence break, so every break is a candidate
/// and the last one that parses as a promo wins.
fn promo_tail_start(text: &str) -> Option<usize> {
    const SENTENCE_END: [char; 7] = ['.', '!', '?', '"', '\'', '\u{2019}', '\u{201d}'];
    let mut starts = Vec::new();
    let mut after_end = false;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if after_end {
                starts.push(i + c.len_utf8());
            }
        } else {
            after_end = SENTENCE_END.contains(&c);
        }
    }
    starts
        .into_iter()
        .rev()
        .find(|&s| s < text.len() && is_promo(text[s..].trim()))
}

/// Remove freewebnovel's injected advert from a paragraph.
///
/// Returns `None` when the paragraph was nothing but the advert. The site
/// either drops one in as a paragraph of its own or tacks it onto the end of
/// real prose; anything else comes back unchanged.
pub fn strip_promo(paragraph: &str) -> Option<String> {
    let text = paragraph.trim();
    if text.is_empty() || is_promo(text) {
        return None;
    }
    match promo_tail_start(text) {
        Some(cut) => {
            let kept = text[..cut].trim_end();
            (!kept.is_empty()).then(|| kept.to_string())
        }
        None => Some(text.to_string()),
    }
}

/// Apply [`strip_promo`] across a chapter, keeping the original if the marks
/// somehow account for all of it: a chapter with the advert still in it beats
/// no chapter at all.
pub fn strip_promo_paragraphs(paragraphs: &[String]) -> Vec<String> {
    let cleaned: Vec<String> = paragraphs.iter().filter_map(|p| strip_promo(p)).collect();
    if cleaned.is_empty() {
        return paragraphs.to_vec();
    }
    cleaned
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
        let paragraphs = strip_promo_paragraphs(&paragraphs);
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

    /// Marks taken verbatim from a real library, mangled spellings included.
    const MARKS: &[&str] = &[
        "Enjoy exclusive adventures from freewebnovel",
        "Updates by Freewebnovel. com",
        "Experience exclusive tales on freewebnovel.com",
        "Continue -reading on Freewebnovel.com",
        "Your next journey awaits at freewebnovel",
        "--- Discover stories with empire",
        "Continue your saga on empire",
        "Stay tuned for updates on empire",
        "Read the latest on freewebnovel",
    ];

    /// Real prose from the same library. Every one of these mentions an
    /// in-story empire, which is why detection can't just look for the word.
    const PROSE: &[&str] = &[
        "Eldorath Empire.",
        "Iron Empire.",
        "\"The Empire is protected by the mighty War God. Of course they're not afraid to burn a temple.\"",
        "Followers of War burned the temples of Shadow, and their empire spread, consuming many weaker realms.\"",
        "'So the Nine were determined to destroy the Empire...'",
        "\"WELCOME, LADIES AND GENTLEMEN, TO THE EMPIRE'S GRAND YOUTH TOURNAMENT!\"",
        "He was a soldier of a militant empire which worshiped War God and had conquered many lands.",
    ];

    #[test]
    fn drops_a_paragraph_that_is_only_a_mark() {
        for m in MARKS {
            assert_eq!(strip_promo(m), None, "{m:?} should be dropped");
        }
    }

    #[test]
    fn keeps_prose_about_an_in_story_empire() {
        for p in PROSE {
            assert_eq!(strip_promo(p).as_deref(), Some(*p), "{p:?} is prose");
        }
    }

    #[test]
    fn strips_a_mark_appended_to_a_real_paragraph() {
        assert_eq!(
            strip_promo("\"What do you want, Derek?\" Alice asked coldly. Find more chapters on empire")
                .as_deref(),
            Some("\"What do you want, Derek?\" Alice asked coldly.")
        );
        assert_eq!(
            strip_promo("Bang! Your adventure continues at empire").as_deref(),
            Some("Bang!")
        );
        assert_eq!(
            strip_promo(
                "Max sighed. \"Thorne Family sure is rich,\" he muttered, smiling. \
                 Find your next read on freewebnovel.com"
            )
            .as_deref(),
            Some("Max sighed. \"Thorne Family sure is rich,\" he muttered, smiling.")
        );
    }

    #[test]
    fn filters_a_whole_chapter() {
        let paragraphs: Vec<String> = [
            "He drew his sword.",
            "Enjoy more content from freewebnovel",
            "The blade sang. Stay updated with freewebnovel",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            strip_promo_paragraphs(&paragraphs),
            vec!["He drew his sword.".to_string(), "The blade sang.".to_string()]
        );
    }

    /// A chapter that is nothing but marks keeps its text: losing the advert is
    /// not worth losing the chapter.
    #[test]
    fn a_chapter_of_nothing_but_marks_is_left_alone() {
        let only = vec!["Read the latest on freewebnovel".to_string()];
        assert_eq!(strip_promo_paragraphs(&only), only);
    }
}
