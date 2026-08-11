//! Hand-written adapter for chikari.moe — the site lightnovelworld's novel
//! library moved to.
//!
//! Why hand-written: chikari is a SvelteKit app whose chapter pages are
//! client-rendered (the HTML served for `/novels/<slug>/<n>` is an empty app
//! shell), so there is nothing to scrape. It does, however, publish a plain
//! JSON API — the same one its own front end calls — documented at
//! `/api/openapi.json`. Reading that is both more robust and far politer than
//! scraping would be:
//!
//! - `GET /api/novels/<slug>` — metadata (title, authors, cover, status,
//!   genres, chapter counts).
//! - `GET /api/novels/<slug>/chapters?order=asc&limit=500&offset=N` — the real
//!   table of contents, paginated. The server clamps `limit` to 500.
//! - `GET /api/novels/<slug>/chapters/<n>/read` — one chapter; `body` is plain
//!   text with paragraphs separated by newlines.
//!
//! **Discovery reads the list; it never generates `1..=N`.** chikari's chapter
//! numbering has holes (deleted/merged chapters — `latest_number` runs ahead of
//! `stored_chapter_count` for roughly half the catalogue), and those numbers
//! 404 on the read endpoint. Because the ToC endpoint returns only the numbers
//! that actually exist, Vesper never requests a dead one, so this source
//! produces no 404 gaps at all — unlike lightnovelworld, where the count was
//! all we had and the missing numbers had to be discovered by hitting them.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use crate::fetch::Fetcher;
use crate::model::{Chapter, ChapterRef, NovelMeta, NovelStatus};
use crate::source::{parse_status_hint, Source};
use crate::util::clean_chapter_title;

pub const HOST: &str = "chikari.moe";

/// Page size for ToC requests. The API silently clamps anything larger, so
/// asking for more just wastes the round trip.
const PAGE_LIMIT: u32 = 500;

/// How many of the newest chapters a delta check pulls. One request, and wide
/// enough to cover a burst of releases between syncs; if it still isn't enough,
/// `sync` notices the hole and falls back to a full walk.
const LATEST_WINDOW: u32 = 60;

pub struct ChikariSource<F: Fetcher> {
    fetcher: F,
}

impl<F: Fetcher> ChikariSource<F> {
    pub fn new(fetcher: F) -> Self {
        Self { fetcher }
    }

    async fn get_json(&self, url: &str) -> Result<Value> {
        let text = self.fetcher.get(url).await?;
        serde_json::from_str(&text).with_context(|| format!("parsing JSON from {url}"))
    }

    /// Find a novel's slug by title, for when a known slug no longer resolves.
    ///
    /// Matching is on the *normalized* title (the same comparison `subscribe`
    /// uses for duplicates), so punctuation and spacing differences between
    /// sites don't defeat it. Returns `None` rather than a near-miss: silently
    /// binding a subscription to the wrong novel is far worse than reporting
    /// that it couldn't be found.
    pub async fn find_slug_by_title(&self, title: &str) -> Result<Option<String>> {
        let query: String = url::form_urlencoded::byte_serialize(title.as_bytes()).collect();
        let url = format!("https://{HOST}/api/novels/search?q={query}&limit=20");
        let json = self.get_json(&url).await?;
        Ok(match_slug_by_title(&json, title))
    }

    /// One page of the table of contents. Returns the refs plus the reported
    /// total, so the caller knows when to stop paging.
    async fn chapter_page(
        &self,
        slug: &str,
        order: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<ChapterRef>, u32)> {
        let url = format!(
            "https://{HOST}/api/novels/{slug}/chapters?order={order}&limit={limit}&offset={offset}"
        );
        let json = self.get_json(&url).await?;
        parse_chapter_page(&json, slug)
    }
}

/// The novel slug from a chikari URL. Accepts the novel page
/// (`/novels/<slug>`), a chapter page (`/novels/<slug>/<n>`), and the bare
/// `/novel/<slug>` singular form a hand-typed or migrated URL might carry.
fn slug_from_url(url: &str) -> Result<String> {
    let parsed = Url::parse(url).with_context(|| format!("parsing {url}"))?;
    let mut segments = parsed
        .path_segments()
        .ok_or_else(|| anyhow!("{url} has no path"))?
        .filter(|s| !s.is_empty());

    match segments.next() {
        Some("novels") | Some("novel") => {}
        _ => return Err(anyhow!("{url} is not a chikari novel URL (expected /novels/<slug>)")),
    }
    let slug = segments
        .next()
        .ok_or_else(|| anyhow!("{url} is missing a novel slug"))?;
    Ok(slug.to_string())
}

/// The canonical novel URL Vesper stores for a chikari novel.
pub fn novel_url(slug: &str) -> String {
    format!("https://{HOST}/novels/{slug}")
}

fn read_url(slug: &str, number: u32) -> String {
    format!("https://{HOST}/api/novels/{slug}/chapters/{number}/read")
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    let s = v.get(key)?.as_str()?.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// A chapter number as Vesper's schema needs it: a positive integer.
///
/// The API types `number` as a float. Every chapter observed across the
/// catalogue is integral, but a fractional number (a "12.5" side chapter) can't
/// be stored — `chapters` is keyed by an integer number — and rounding one would
/// silently collide with, and overwrite, a real neighbouring chapter. So a
/// non-integral number is skipped rather than mangled.
fn chapter_number(v: &Value) -> Option<u32> {
    let n = v.get("number")?.as_f64()?;
    if !n.is_finite() || n < 1.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
        return None;
    }
    Some(n as u32)
}

/// Author from the `authors` array, preferring the one credited as the author
/// over a translator or artist.
fn parse_author(v: &Value) -> Option<String> {
    let authors = v.get("authors")?.as_array()?;
    let pick = authors
        .iter()
        .find(|a| a.get("role").and_then(Value::as_str) == Some("author"))
        .or_else(|| authors.first())?;
    str_field(pick, "name")
}

/// Genres as a comma-separated list, matching what the other adapters put in
/// `NovelMeta::genre` (it becomes the EPUB's `dc:subject`).
fn parse_genres(v: &Value) -> Option<String> {
    let names: Vec<String> = v
        .get("genres")?
        .as_array()?
        .iter()
        .filter_map(|g| str_field(g, "name"))
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// Pick the search hit whose title matches `title` once normalized. The API
/// returns either a bare array or an `{ "items": [...] }` envelope depending on
/// the endpoint, so accept both.
fn match_slug_by_title(json: &Value, title: &str) -> Option<String> {
    let items = json
        .as_array()
        .or_else(|| json.get("items")?.as_array())?;
    let wanted = crate::util::normalize_title(title);
    if wanted.is_empty() {
        return None;
    }
    items
        .iter()
        .find(|item| {
            str_field(item, "title")
                .map(|t| crate::util::normalize_title(&t) == wanted)
                .unwrap_or(false)
        })
        .and_then(|item| str_field(item, "slug"))
}

fn parse_novel(json: &Value, source_url: &str) -> Result<NovelMeta> {
    let title = str_field(json, "title")
        .ok_or_else(|| anyhow!("no novel title in the API response for {source_url}"))?;
    Ok(NovelMeta {
        title,
        author: parse_author(json),
        cover_url: str_field(json, "cover_url"),
        genre: parse_genres(json),
        status_hint: str_field(json, "status")
            .map(|s| parse_status_hint(&s))
            .unwrap_or(NovelStatus::Unknown),
        source_url: source_url.to_string(),
    })
}

/// One page of `/chapters`: the refs it lists (skipping unusable numbers) and
/// the total chapter count the server reports.
fn parse_chapter_page(json: &Value, slug: &str) -> Result<(Vec<ChapterRef>, u32)> {
    let items = json
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("chapter listing had no `items` array"))?;
    let total = json
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(items.len() as u64) as u32;

    let refs = items
        .iter()
        .filter_map(|item| {
            let number = chapter_number(item)?;
            Some(ChapterRef {
                number,
                title: chapter_title(item, number),
                url: read_url(slug, number),
            })
        })
        .collect();
    Ok((refs, total))
}

/// A chapter's display title. The site stores it with its own "Chapter N"
/// prefix — and often a duplicated "N:" after that ("Chapter 1 - 1: Nightmare
/// Begins") — while Vesper renders its own prefix at EPUB build time, so both
/// are stripped. Note the site's prefix number is its *display* number, which
/// can differ from the canonical `number` the URL uses.
fn chapter_title(item: &Value, number: u32) -> String {
    let raw = str_field(item, "title").unwrap_or_default();
    let cleaned = strip_leading_number_colon(&clean_chapter_title(&raw));
    if cleaned.is_empty() {
        format!("Chapter {number}")
    } else {
        cleaned
    }
}

fn strip_leading_number_colon(s: &str) -> String {
    let t = s.trim_start();
    let digits: usize = t.chars().take_while(|c| c.is_ascii_digit()).count();
    let rest = t[digits..].trim_start();
    if digits > 0 && rest.starts_with(':') {
        rest[1..].trim().to_string()
    } else {
        t.trim().to_string()
    }
}

/// Inline tags chikari permits inside a chapter body. The body is *plain text*,
/// not HTML — the reader escapes `&`, `<` and `>` wholesale and then re-enables
/// exactly this set — so these arrive as literal characters and would otherwise
/// end up visible in the EPUB, whose paragraphs are XML-escaped.
const INLINE_TAGS: &[&str] = &["em", "strong", "i", "b", "u", "s", "sup", "sub", "br"];

/// Drop chikari's inline markup, leaving the prose. Anything that isn't one of
/// the recognised tags is kept verbatim — a stray `<` in dialogue is text, not
/// markup, and must survive.
fn strip_inline_markup(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if bytes[i] == b'<' {
            if let Some(end) = inline_tag_end(s, i) {
                i = end;
                continue;
            }
        }
        // Step by whole characters so multi-byte prose isn't split.
        let ch = s[i..].chars().next().expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// If `s[start..]` opens a recognised inline tag, the index just past its `>`.
fn inline_tag_end(s: &str, start: usize) -> Option<usize> {
    let rest = &s[start + 1..];
    let close = rest.find('>')?;
    let inner = rest[..close].trim();
    let name = inner.strip_prefix('/').unwrap_or(inner);
    // `<br/>` and `<br />` also end with a slash.
    let name = name.strip_suffix('/').unwrap_or(name).trim();
    INLINE_TAGS
        .iter()
        .any(|t| name.eq_ignore_ascii_case(t))
        .then_some(start + 1 + close + 1)
}

/// Split a chapter body into paragraphs. chikari separates them with newlines
/// (single or blank-line-doubled); its own reader treats every newline as a
/// break, so we do too.
fn body_paragraphs(body: &str) -> Vec<String> {
    body.split('\n')
        .map(|line| strip_inline_markup(line).trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn parse_read(json: &Value, fallback: &ChapterRef) -> Result<Chapter> {
    // An early-access chapter comes back flagged rather than as an error. It has
    // no prose to store, so treat it as "not available yet" — a transient
    // failure that sync retries, not a permanent hole.
    if json.get("locked").and_then(Value::as_bool).unwrap_or(false) {
        let reason = str_field(json, "lock_reason").unwrap_or_else(|| "locked".into());
        return Err(anyhow!(
            "ch.{} is locked on chikari ({reason}); it will be retried",
            fallback.number
        ));
    }
    let body = json
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("ch.{} came back without a body", fallback.number))?;
    let paragraphs = body_paragraphs(body);
    if paragraphs.is_empty() {
        return Err(anyhow!("ch.{} came back with an empty body", fallback.number));
    }
    let number = chapter_number(json).unwrap_or(fallback.number);
    let title = {
        let t = chapter_title(json, number);
        if t.is_empty() { fallback.title.clone() } else { t }
    };
    Ok(Chapter {
        number,
        title,
        paragraphs,
    })
}

#[async_trait]
impl<F: Fetcher> Source for ChikariSource<F> {
    fn name(&self) -> &str {
        "chikari"
    }

    fn matches(&self, url: &str) -> bool {
        Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| is_chikari_host(h)))
            .unwrap_or(false)
    }

    async fn fetch_novel(&self, url: &str) -> Result<NovelMeta> {
        let slug = slug_from_url(url)?;
        let json = self
            .get_json(&format!("https://{HOST}/api/novels/{slug}"))
            .await?;
        // Store the canonical novel URL rather than whatever form was passed in.
        parse_novel(&json, &novel_url(&slug))
    }

    async fn discover_chapters(&self, url: &str, needed: Option<usize>) -> Result<Vec<ChapterRef>> {
        let slug = slug_from_url(url)?;

        // With a `needed` hint, page from the *newest* end and stop once that
        // many are in hand — the caller wants the recent tail, not the oldest N.
        let descending = needed.is_some();
        let order = if descending { "desc" } else { "asc" };

        let mut out: Vec<ChapterRef> = Vec::new();
        let mut offset: u32 = 0;
        loop {
            let (page, total) = self.chapter_page(&slug, order, PAGE_LIMIT, offset).await?;
            if page.is_empty() {
                break;
            }
            // Advance by the page's raw length, not by how many refs survived
            // filtering, or an unusable number would shift every later offset.
            offset = offset.saturating_add(PAGE_LIMIT.min(page.len() as u32));
            out.extend(page);
            if let Some(n) = needed {
                if out.len() >= n {
                    break;
                }
            }
            if offset >= total {
                break;
            }
        }

        out.sort_by_key(|c| c.number);
        out.dedup_by_key(|c| c.number);
        Ok(out)
    }

    async fn fetch_chapter(&self, chapter: &ChapterRef) -> Result<Chapter> {
        let json = self.get_json(&chapter.url).await?;
        parse_read(&json, chapter)
    }

    async fn discover_latest(&self, url: &str) -> Result<Vec<ChapterRef>> {
        let slug = slug_from_url(url)?;
        let (mut page, _) = self.chapter_page(&slug, "desc", LATEST_WINDOW, 0).await?;
        page.sort_by_key(|c| c.number);
        Ok(page)
    }
}

/// chikari serves the same app on the bare domain and `www.`.
pub fn is_chikari_host(host: &str) -> bool {
    let host = host.trim_start_matches("www.");
    host.eq_ignore_ascii_case(HOST)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn novel_json() -> Value {
        json!({
            "id": 50,
            "slug": "shadow-slave",
            "title": "Shadow Slave",
            "status": "releasing",
            "chapter_count": 3150,
            "stored_chapter_count": 3150,
            "latest_number": 3150.0,
            "cover_url": "https://cdn.chikari.moe/novels/53/cover.webp",
            "genres": [{"slug": "action", "name": "Action"}, {"slug": "fantasy", "name": "Fantasy"}],
            "authors": [
                {"name": "Some Translator", "slug": "t", "role": "translator"},
                {"name": "GuiltyThree", "slug": "guiltythree", "role": "author"}
            ]
        })
    }

    #[test]
    fn parses_novel_metadata() {
        let meta = parse_novel(&novel_json(), &novel_url("shadow-slave")).unwrap();
        assert_eq!(meta.title, "Shadow Slave");
        assert_eq!(meta.author.as_deref(), Some("GuiltyThree"), "the author, not the translator");
        assert_eq!(
            meta.cover_url.as_deref(),
            Some("https://cdn.chikari.moe/novels/53/cover.webp")
        );
        assert_eq!(meta.genre.as_deref(), Some("Action, Fantasy"));
        assert_eq!(meta.status_hint, NovelStatus::Ongoing, "\"releasing\" is ongoing");
        assert_eq!(meta.source_url, "https://chikari.moe/novels/shadow-slave");
    }

    #[test]
    fn falls_back_to_the_first_author_when_no_role_is_credited() {
        let v = json!({"title": "X", "authors": [{"name": "Only One", "slug": "o"}]});
        assert_eq!(parse_novel(&v, "u").unwrap().author.as_deref(), Some("Only One"));
    }

    #[test]
    fn slugs_come_from_novel_and_chapter_urls() {
        assert_eq!(slug_from_url("https://chikari.moe/novels/shadow-slave").unwrap(), "shadow-slave");
        assert_eq!(slug_from_url("https://chikari.moe/novels/shadow-slave/").unwrap(), "shadow-slave");
        assert_eq!(slug_from_url("https://chikari.moe/novels/shadow-slave/42").unwrap(), "shadow-slave");
        // Singular form, as a hand-typed or migrated URL might carry.
        assert_eq!(slug_from_url("https://chikari.moe/novel/shadow-slave").unwrap(), "shadow-slave");
        // Not a novel URL.
        assert!(slug_from_url("https://chikari.moe/series/one-piece").is_err());
        assert!(slug_from_url("https://chikari.moe/novels").is_err());
    }

    /// The ToC is the authority: numbers absent from it (site holes) are never
    /// generated, so they're never requested and never become 404 gaps.
    #[test]
    fn discovery_lists_only_the_numbers_the_site_reports() {
        let page = json!({
            "items": [
                {"number": 1.0, "title": "Chapter 1 - 1: Nightmare Begins"},
                {"number": 2.0, "title": "Chapter 2 Slave Caravan"},
                // 3 is missing on the site — a deleted chapter.
                {"number": 4.0, "title": "Chapter 4: The Fourth"}
            ],
            "total": 3
        });
        let (refs, total) = parse_chapter_page(&page, "shadow-slave").unwrap();
        assert_eq!(total, 3);
        assert_eq!(refs.iter().map(|c| c.number).collect::<Vec<_>>(), vec![1, 2, 4]);
        assert_eq!(refs[0].title, "Nightmare Begins", "doubled \"N:\" prefix stripped");
        assert_eq!(refs[1].title, "Slave Caravan");
        assert_eq!(refs[2].title, "The Fourth");
        assert_eq!(
            refs[0].url,
            "https://chikari.moe/api/novels/shadow-slave/chapters/1/read"
        );
    }

    /// A fractional number can't be stored and must not be rounded into a
    /// neighbour's slot — it is dropped instead.
    #[test]
    fn fractional_chapter_numbers_are_skipped_not_rounded() {
        let page = json!({
            "items": [
                {"number": 12.0, "title": "Chapter 12"},
                {"number": 12.5, "title": "Chapter 12.5 Interlude"},
                {"number": 13.0, "title": "Chapter 13"}
            ],
            "total": 3
        });
        let (refs, _) = parse_chapter_page(&page, "x").unwrap();
        assert_eq!(refs.iter().map(|c| c.number).collect::<Vec<_>>(), vec![12, 13]);
    }

    #[test]
    fn keeps_a_bare_chapter_label_when_there_is_no_name() {
        let page = json!({"items": [{"number": 7.0, "title": "Chapter 7"}], "total": 1});
        let (refs, _) = parse_chapter_page(&page, "x").unwrap();
        assert_eq!(refs[0].title, "Chapter 7");
        // Missing title entirely still yields something usable.
        let page = json!({"items": [{"number": 8.0}], "total": 1});
        let (refs, _) = parse_chapter_page(&page, "x").unwrap();
        assert_eq!(refs[0].title, "Chapter 8");
    }

    fn a_ref(number: u32) -> ChapterRef {
        ChapterRef {
            number,
            title: format!("Chapter {number}"),
            url: read_url("x", number),
        }
    }

    #[test]
    fn reads_a_chapter_body_into_paragraphs() {
        let v = json!({
            "number": 1.0,
            "title": "Chapter 1 - 1: Nightmare Begins",
            "body": "First paragraph.\n\nSecond paragraph.\n\n  \n Third paragraph. "
        });
        let ch = parse_read(&v, &a_ref(1)).unwrap();
        assert_eq!(ch.number, 1);
        assert_eq!(ch.title, "Nightmare Begins");
        assert_eq!(
            ch.paragraphs,
            vec!["First paragraph.", "Second paragraph.", "Third paragraph."]
        );
    }

    #[test]
    fn strips_inline_markup_but_keeps_stray_angle_brackets() {
        let v = json!({
            "number": 2.0,
            "title": "Chapter 2",
            "body": "He <em>ran</em> fast.<br/>\nShe said <i>no</i>.\nDamage: 5 < 10 and a <notatag> stays."
        });
        let ch = parse_read(&v, &a_ref(2)).unwrap();
        assert_eq!(
            ch.paragraphs,
            vec![
                "He ran fast.",
                "She said no.",
                "Damage: 5 < 10 and a <notatag> stays."
            ]
        );
    }

    /// Multi-byte prose must survive the markup scan intact.
    #[test]
    fn non_ascii_prose_is_not_mangled() {
        let v = json!({"number": 3.0, "title": "", "body": "「こんにちは」と<b>言った</b>。\nDash — and é."});
        let ch = parse_read(&v, &a_ref(3)).unwrap();
        assert_eq!(ch.paragraphs, vec!["「こんにちは」と言った。", "Dash — and é."]);
        // Empty title falls back to the ref's.
        assert_eq!(ch.title, "Chapter 3");
    }

    /// A locked (early-access) chapter is a "not yet", not a permanent hole:
    /// it must surface as an ordinary error so sync retries it rather than
    /// recording a gap or storing an empty chapter.
    #[test]
    fn locked_chapter_is_a_retryable_error() {
        let v = json!({
            "number": 9.0,
            "title": "Chapter 9",
            "body": "",
            "locked": true,
            "lock_reason": "early access"
        });
        let err = parse_read(&v, &a_ref(9)).unwrap_err();
        assert!(err.to_string().contains("locked"), "{err}");
        assert!(!crate::fetch::is_not_found(&err), "a lock is not a 404 gap");
    }

    #[test]
    fn empty_body_is_rejected_rather_than_stored() {
        let v = json!({"number": 4.0, "title": "Chapter 4", "body": "   \n  \n"});
        assert!(parse_read(&v, &a_ref(4)).is_err());
    }

    #[test]
    fn search_matches_on_the_normalized_title_only() {
        let results = json!([
            {"slug": "shadow-slave-2", "title": "Shadow Slave: Side Stories"},
            {"slug": "shadow-slave", "title": "Shadow  Slave!"}
        ]);
        // Punctuation and spacing differences still match...
        assert_eq!(
            match_slug_by_title(&results, "Shadow Slave").as_deref(),
            Some("shadow-slave")
        );
        // ...but a merely similar title does not, so a subscription is never
        // silently bound to the wrong novel.
        assert_eq!(match_slug_by_title(&results, "Shadow Slaves"), None);
        // The `{items: [...]}` envelope form is accepted too.
        let enveloped = json!({"items": [{"slug": "x", "title": "A Novel"}]});
        assert_eq!(match_slug_by_title(&enveloped, "A Novel").as_deref(), Some("x"));
    }

    #[test]
    fn matches_only_chikari_hosts() {
        let src = ChikariSource::new(crate::fetch::CurlFetcher::new(std::time::Duration::ZERO));
        assert!(src.matches("https://chikari.moe/novels/x"));
        assert!(src.matches("https://www.chikari.moe/novels/x"));
        assert!(!src.matches("https://lightnovelworld.org/novel/x/"));
        assert!(!src.matches("https://notchikari.moe/novels/x"));
    }
}
