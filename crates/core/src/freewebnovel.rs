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

/// Longest pitch a mark uses. "Your next journey awaits at ..." is four words.
const PROMO_MAX_PITCH_WORDS: usize = 6;

/// Words the site opens a pitch with, required for the `empire` spelling only.
///
/// Without this, a paragraph ending "The road to empire." matches the shape of
/// a mark: capitalised word, a couple of lowercase ones, preposition, then a
/// lower-case `empire` that English writes without an article. That sentence is
/// prose, and eating it would be silent and permanent, while missing a mark
/// costs a cosmetic line that a later `vesper scrub` picks up. `freewebnovel`
/// needs no list, since naming the site is never prose.
const PROMO_OPENERS: &[&str] = &[
    "Continue",
    "Discover",
    "Enjoy",
    "Experience",
    "Explore",
    "Find",
    "Read",
    "Stay",
    "Your",
];

/// Punctuation the site wraps a mark in, on the opening side.
const MARK_OPENERS: &[char] = &['"', '\'', '\u{201c}', '\u{2018}', '(', '[', '<', '*'];

/// The same on the closing side, plus the stop a mark sometimes carries.
const MARK_CLOSERS: &[char] = &[
    '"', '\'', '\u{201d}', '\u{2019}', ')', ']', '>', '*', '.', '!', ',',
];

pub struct FreewebnovelSource<F: Fetcher> {
    fetcher: F,
}

impl<F: Fetcher> FreewebnovelSource<F> {
    pub fn new(fetcher: F) -> Self {
        Self { fetcher }
    }
}

/// Find `needle` (ASCII) from `from`, ignoring case.
fn ascii_ci_find(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() || from > h.len() - n.len() {
        return None;
    }
    (from..=h.len() - n.len())
        .find(|&i| haystack.is_char_boundary(i) && h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// The word ending before `pos`, with its start index.
///
/// A word is letters plus inner apostrophes and hyphens, so punctuation glued
/// to one ("so long.\u{201c}Please") ends the word instead of joining it, which is
/// what keeps a mark's pitch from reaching back into the sentence before it.
fn previous_word(text: &str, pos: usize) -> Option<(usize, &str)> {
    let mut end = pos;
    while let Some(c) = text[..end].chars().next_back() {
        if c == ' ' || c == '\t' {
            end -= c.len_utf8();
        } else {
            break;
        }
    }
    let word_end = end;
    while let Some(c) = text[..end].chars().next_back() {
        if c.is_alphabetic() || c == '\'' || c == '\u{2019}' || c == '-' {
            end -= c.len_utf8();
        } else {
            break;
        }
    }
    (end < word_end).then(|| (end, &text[end..word_end]))
}

/// Take in the quote or bracket the site opened the mark with, if it is glued
/// to the front of it.
fn absorb_openers(text: &str, start: usize) -> usize {
    let mut at = start;
    while let Some(c) = text[..at].chars().next_back() {
        if MARK_OPENERS.contains(&c) {
            at -= c.len_utf8();
        } else {
            break;
        }
    }
    at
}

/// Take in the punctuation trailing a mark, including a detached run like
/// " >\u{201d}".
fn absorb_closers(text: &str, end: usize) -> usize {
    let mut at = end;
    loop {
        let rest = &text[at..];
        let trimmed = rest.trim_start_matches([' ', '\t']);
        let gap = rest.len() - trimmed.len();
        match trimmed.chars().next() {
            Some(c) if MARK_CLOSERS.contains(&c) => at += gap + c.len_utf8(),
            _ => return at,
        }
    }
}

/// Where the injected sentence ending at `site_start` begins, if the words
/// before the site name are a promo pitch.
///
/// The site name is preceded by a preposition, and the pitch before that runs
/// back to the capitalised word the injection starts on. Only spaces may
/// separate those words, so a mark glued onto real prose can't swallow it.
fn phrase_start(text: &str, site_start: usize) -> Option<usize> {
    let (prep_start, preposition) = previous_word(text, site_start)?;
    if !PROMO_PREPOSITIONS
        .iter()
        .any(|p| preposition.eq_ignore_ascii_case(p))
    {
        return None;
    }

    let mut cursor = prep_start;
    for _ in 0..PROMO_MAX_PITCH_WORDS {
        let (start, word) = previous_word(text, cursor)?;
        if word.chars().next().is_some_and(char::is_uppercase) {
            return Some(absorb_openers(text, start));
        }
        if !matches!(text[..start].chars().next_back(), Some(' ')) {
            return None;
        }
        cursor = start;
    }
    None
}

/// Whether the phrase at `begin` opens with one of the site's pitch words.
fn opens_a_pitch(text: &str, begin: usize) -> bool {
    let word = text[begin..]
        .trim_start_matches(MARK_OPENERS)
        .split(|c: char| !(c.is_alphabetic() || c == '\'' || c == '\u{2019}' || c == '-'))
        .next()
        .unwrap_or_default();
    PROMO_OPENERS.iter().any(|o| word.eq_ignore_ascii_case(o))
}

/// Whether only the mark's own trailing punctuation follows.
fn only_decoration(rest: &str) -> bool {
    rest.chars()
        .all(|c| c.is_whitespace() || MARK_CLOSERS.contains(&c))
}

/// Byte ranges of the injected marks in `text`.
///
/// `freewebnovel` is removed wherever it appears, with its pitch when it has
/// one: no story says the word, so every occurrence is the site talking,
/// including a bare "\u{2018}Freewebnovel.com*\u{2019}" dropped between sentences. `empire`
/// (a sister site) is the ambiguous one and gets two extra conditions: it must
/// carry a pitch, and the mark must end the paragraph. Prose is full of an
/// in-story empire, but it says "the Empire" and carries on past the word.
fn promo_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();

    let mut from = 0;
    while let Some(start) = ascii_ci_find(text, "freewebnovel", from) {
        let mut end = start + "freewebnovel".len();
        for suffix in [".com", ". com"] {
            if text[end..].len() >= suffix.len()
                && text.as_bytes()[end..end + suffix.len()].eq_ignore_ascii_case(suffix.as_bytes())
            {
                end += suffix.len();
                break;
            }
        }
        end = absorb_closers(text, end);
        let begin = phrase_start(text, start).unwrap_or_else(|| absorb_openers(text, start));
        spans.push((begin, end));
        from = end;
    }

    let mut from = 0;
    while let Some(start) = ascii_find_word(text, "empire", from) {
        let end = absorb_closers(text, start + "empire".len());
        from = start + "empire".len();
        if let Some(begin) = phrase_start(text, start) {
            if only_decoration(&text[end..]) && opens_a_pitch(text, begin) {
                spans.push((begin, end));
            }
        }
    }

    spans.sort_unstable();
    spans
}

/// Find `needle` as a whole word, case-sensitively.
fn ascii_find_word(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    let mut at = from;
    while let Some(i) = haystack[at..].find(needle).map(|i| i + at) {
        let before = haystack[..i].chars().next_back();
        let after = haystack[i + needle.len()..].chars().next();
        let bounded = !before.is_some_and(char::is_alphanumeric)
            && !after.is_some_and(char::is_alphanumeric);
        if bounded {
            return Some(i);
        }
        at = i + needle.len();
    }
    None
}

/// Remove freewebnovel's injected adverts from a paragraph.
///
/// Returns `None` when nothing but the advert (and stray punctuation) is left.
pub fn strip_promo(paragraph: &str) -> Option<String> {
    let text = paragraph.trim();
    let spans = promo_spans(text);
    if spans.is_empty() {
        return (!text.is_empty()).then(|| text.to_string());
    }

    let mut kept = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end) in spans {
        if start > cursor {
            kept.push_str(&text[cursor..start]);
        }
        cursor = cursor.max(end);
    }
    kept.push_str(&text[cursor..]);

    // Taking a mark out can leave a doubled space, or a lone separator where
    // the mark arrived with one glued to its front.
    let tidy = kept.split_whitespace().collect::<Vec<_>>().join(" ");
    tidy.chars().any(char::is_alphanumeric).then_some(tidy)
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

    /// Marks that survived the first pass, because it looked for a mark after a
    /// full stop and a space. The site also appends after a bracket, an
    /// ellipsis or a dash, glues one on with no space at all, and drops a bare
    /// decorated domain between two sentences.
    #[test]
    fn strips_marks_the_punctuation_hid() {
        let cases = [
            (
                "Then\u{2014}without hesitation\u{2014} Enjoy exclusive content from freewebnovel",
                "Then\u{2014}without hesitation\u{2014}",
            ),
            (
                "[Energy decreased by 0.1] Discover more stories at freewebnovel",
                "[Energy decreased by 0.1]",
            ),
            (
                "Just when the countdown reach 10, 9, 8\u{2026} Find your next read on empire",
                "Just when the countdown reach 10, 9, 8\u{2026}",
            ),
            ("[Max] Explore stories at empire", "[Max]"),
            (
                "\u{2013} Battle Coins: [9240] Stay updated via empire",
                "\u{2013} Battle Coins: [9240]",
            ),
            (
                "See you in the Battle Realm at noon tomorrow.] Read exclusive adventures at empire",
                "See you in the Battle Realm at noon tomorrow.]",
            ),
            (
                "ordinary people would not last so long.\u{201c}Please reading on Freewebnovel.com >\u{201d}",
                "ordinary people would not last so long.",
            ),
            (
                "Those were the Mad Blood weapons. \u{2018}Freewebnovel.com*\u{2019}It turned out that they were gone.",
                "Those were the Mad Blood weapons. It turned out that they were gone.",
            ),
        ];
        for (raw, want) in cases {
            assert_eq!(strip_promo(raw).as_deref(), Some(want), "input: {raw:?}");
        }
    }

    /// The reason `empire` has to end the paragraph: mid-sentence it is prose,
    /// preposition in front of it or not.
    #[test]
    fn keeps_an_empire_that_the_sentence_carries_past() {
        for prose in [
            "In the war with empire forces, the column marched on.",
            "He rode to empire lands and never came back.",
        ] {
            assert_eq!(strip_promo(prose).as_deref(), Some(prose), "{prose:?}");
        }
    }

    /// Prose that has a mark's exact shape: capitalised word, lowercase words,
    /// preposition, then a bare lower-case "empire" at the end of the
    /// paragraph. Only the pitch vocabulary tells this from an advert.
    #[test]
    fn keeps_prose_shaped_like_a_mark() {
        for prose in [
            "The road to empire.",
            "Their long march to empire.",
            "A thousand years of war, and every step on the road to empire.",
        ] {
            assert_eq!(strip_promo(prose).as_deref(), Some(prose), "{prose:?}");
        }
    }

    /// Every distinct `empire` mark the library held, so tightening the pitch
    /// list can't quietly stop catching one.
    #[test]
    fn catches_every_observed_empire_mark() {
        for mark in [
            "Continue reading on empire",
            "Continue your saga on empire",
            "Discover hidden content at empire",
            "Discover more stories at empire",
            "Discover stories with empire",
            "Enjoy exclusive content from empire",
            "Enjoy more content from empire",
            "Enjoy new chapters from empire",
            "Experience more on empire",
            "Experience tales at empire",
            "Explore more at empire",
            "Explore stories at empire",
            "Find adventures on empire",
            "Find more chapters on empire",
            "Find your next adventure on empire",
            "Find your next read at empire",
            "Find your next read on empire",
            "Read exclusive adventures at empire",
            "Read new adventures at empire",
            "Read the latest on empire",
            "Stay tuned for updates on empire",
            "Stay updated through empire",
            "Stay updated via empire",
            "Your adventure continues at empire",
        ] {
            assert_eq!(strip_promo(mark), None, "{mark:?} should be dropped");
            let appended = format!("She turned away. {mark}");
            assert_eq!(
                strip_promo(&appended).as_deref(),
                Some("She turned away."),
                "{appended:?}"
            );
        }
    }

    /// A chapter that is nothing but marks keeps its text: losing the advert is
    /// not worth losing the chapter.
    #[test]
    fn a_chapter_of_nothing_but_marks_is_left_alone() {
        let only = vec!["Read the latest on freewebnovel".to_string()];
        assert_eq!(strip_promo_paragraphs(&only), only);
    }
}
