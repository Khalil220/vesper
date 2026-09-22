
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use crate::fetch::Fetcher;
use crate::model::{Chapter, ChapterRef, NovelMeta, NovelStatus};
use crate::source::{parse_status_hint, Source};
use crate::util::clean_chapter_title;

pub const HOST: &str = "chikari.moe";

const PAGE_LIMIT: u32 = 500;

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

    pub async fn find_slug_by_title(&self, title: &str) -> Result<Option<String>> {
        let query: String = url::form_urlencoded::byte_serialize(title.as_bytes()).collect();
        let url = format!("https://{HOST}/api/novels/search?q={query}&limit=20");
        let json = self.get_json(&url).await?;
        Ok(match_slug_by_title(&json, title))
    }

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

fn chapter_number(v: &Value) -> Option<u32> {
    let n = v.get("number")?.as_f64()?;
    if !n.is_finite() || n < 1.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
        return None;
    }
    Some(n as u32)
}

fn parse_author(v: &Value) -> Option<String> {
    let authors = v.get("authors")?.as_array()?;
    let pick = authors
        .iter()
        .find(|a| a.get("role").and_then(Value::as_str) == Some("author"))
        .or_else(|| authors.first())?;
    str_field(pick, "name")
}

fn parse_genres(v: &Value) -> Option<String> {
    let names: Vec<String> = v
        .get("genres")?
        .as_array()?
        .iter()
        .filter_map(|g| str_field(g, "name"))
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

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

const INLINE_TAGS: &[&str] = &["em", "strong", "i", "b", "u", "s", "sup", "sub", "br"];

fn strip_inline_markup(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if bytes[i] == b'<' {
            if let Some(end) = inline_tag_end(s, i) {
                if welds_words(out.chars().next_back(), s[end..].chars().next()) {
                    out.push(' ');
                }
                i = end;
                continue;
            }
        }
        let ch = s[i..].chars().next().expect("index is on a char boundary");
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn welds_words(before: Option<char>, after: Option<char>) -> bool {
    let (Some(b), Some(a)) = (before, after) else {
        return false;
    };
    if b.is_whitespace() || a.is_whitespace() {
        return false;
    }
    if !is_space_separated(b) || !is_space_separated(a) {
        return false;
    }
    !hugs_what_precedes(a) && !hugs_what_follows(b)
}

fn hugs_what_precedes(c: char) -> bool {
    matches!(
        c,
        '.' | ',' | '!' | '?' | ';' | ':' | ')' | ']' | '}' | '%'
            | '\u{2019}' | '\u{201d}' | '\u{00bb}' | '\u{2026}' | '\u{2014}' | '\u{2013}'
    )
}

fn hugs_what_follows(c: char) -> bool {
    matches!(
        c,
        '(' | '[' | '{' | '\u{201c}' | '\u{2018}' | '\u{00ab}' | '\u{00bf}' | '\u{00a1}'
    )
}

fn is_space_separated(c: char) -> bool {
    (c as u32) < 0x2E80
}

fn inline_tag_end(s: &str, start: usize) -> Option<usize> {
    let rest = &s[start + 1..];
    let close = rest.find('>')?;
    let inner = rest[..close].trim();
    let name = inner.strip_prefix('/').unwrap_or(inner);
    let name = name.strip_suffix('/').unwrap_or(name).trim();
    INLINE_TAGS
        .iter()
        .any(|t| name.eq_ignore_ascii_case(t))
        .then_some(start + 1 + close + 1)
}

fn body_paragraphs(body: &str) -> Vec<String> {
    body.split('\n')
        .map(|line| strip_inline_markup(line).trim().to_string())
        .filter(|line| !line.is_empty())
        .collect()
}

fn parse_read(json: &Value, fallback: &ChapterRef) -> Result<Chapter> {
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
        parse_novel(&json, &novel_url(&slug))
    }

    async fn discover_chapters(&self, url: &str, needed: Option<usize>) -> Result<Vec<ChapterRef>> {
        let slug = slug_from_url(url)?;

        let descending = needed.is_some();
        let order = if descending { "desc" } else { "asc" };

        let mut out: Vec<ChapterRef> = Vec::new();
        let mut offset: u32 = 0;
        loop {
            let (page, total) = self.chapter_page(&slug, order, PAGE_LIMIT, offset).await?;
            if page.is_empty() {
                break;
            }
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
        assert_eq!(slug_from_url("https://chikari.moe/novel/shadow-slave").unwrap(), "shadow-slave");
        assert!(slug_from_url("https://chikari.moe/series/one-piece").is_err());
        assert!(slug_from_url("https://chikari.moe/novels").is_err());
    }

    #[test]
    fn discovery_lists_only_the_numbers_the_site_reports() {
        let page = json!({
            "items": [
                {"number": 1.0, "title": "Chapter 1 - 1: Nightmare Begins"},
                {"number": 2.0, "title": "Chapter 2 Slave Caravan"},
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

    #[test]
    fn non_ascii_prose_is_not_mangled() {
        let v = json!({"number": 3.0, "title": "", "body": "「こんにちは」と<b>言った</b>。\nDash — and é."});
        let ch = parse_read(&v, &a_ref(3)).unwrap();
        assert_eq!(ch.paragraphs, vec!["「こんにちは」と言った。", "Dash — and é."]);
        assert_eq!(ch.title, "Chapter 3");
    }

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
        assert_eq!(
            match_slug_by_title(&results, "Shadow Slave").as_deref(),
            Some("shadow-slave")
        );
        assert_eq!(match_slug_by_title(&results, "Shadow Slaves"), None);
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

    #[test]
    fn restores_the_space_the_site_dropped_beside_markup() {
        let v = json!({
            "number": 5.0,
            "title": "Chapter 5",
            "body": "He said <em>\u{201c}no\u{201d}</em>Devon thought.\nAlready <em>spaced</em> here."
        });
        let ch = parse_read(&v, &a_ref(5)).unwrap();
        assert_eq!(
            ch.paragraphs,
            vec![
                "He said \u{201c}no\u{201d} Devon thought.",
                "Already spaced here.",
            ]
        );
    }

    #[test]
    fn does_not_inject_spaces_into_cjk_prose() {
        let v = json!({
            "number": 6.0,
            "title": "Chapter 6",
            "body": "\u{300c}\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\u{300d}\u{3068}<b>\u{8a00}\u{3063}\u{305f}</b>\u{3002}"
        });
        let ch = parse_read(&v, &a_ref(6)).unwrap();
        assert_eq!(
            ch.paragraphs,
            vec!["\u{300c}\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}\u{300d}\u{3068}\u{8a00}\u{3063}\u{305f}\u{3002}"]
        );
    }

    #[test]
    fn spacing_matches_what_the_prose_actually_needs() {
        let cases = [
            ("to finally have a<strong>[+1]</strong>next to it.", "to finally have a [+1] next to it."),
            ("<strong>[Name:</strong>Lisa", "[Name: Lisa"),
            ("<strong>Level:</strong>03", "Level: 03"),
            ("hologram,<strong>Isabella Vance</strong>\u{2019}s heart", "hologram, Isabella Vance\u{2019}s heart"),
            ("with<strong>[Aptitude Lv 2]</strong>, he heard", "with [Aptitude Lv 2], he heard"),
            ("<strong>Status:</strong>Online<strong>]</strong>", "Status: Online]"),
            ("She said <i>no</i>.", "She said no."),
        ];
        for (raw, want) in cases {
            assert_eq!(strip_inline_markup(raw), want, "input: {raw:?}");
        }
    }
}
