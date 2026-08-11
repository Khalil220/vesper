//! Re-fetching chapters that were stored as something other than the chapter.
//!
//! A site that gates part of its catalogue can serve a short "log in to keep
//! reading" placeholder with an ordinary 200, so the fetch succeeds and the
//! placeholder is stored as the chapter's text. Sync can never fix that on its
//! own: `insert_chapter_if_absent` is `OR IGNORE`, so a chapter already present
//! is never revisited, however wrong it is. This is the body counterpart to
//! `store::update_chapter_title`, which exists for the same reason.
//!
//! Detection keys on the placeholder's wording, **not** on length alone. Real
//! chapters can be legitimately short — an author's note between arcs is a
//! couple of hundred characters of genuine content — and deleting one of those
//! to "repair" it would be a straight loss. Length only narrows the scan.
//!
//! Every replacement is also checked before it lands: the incoming text must
//! not itself be a placeholder, and must be longer than what is already
//! stored. Repairing from a source that is *also* gated would otherwise
//! overwrite one stub with another, and re-running it against a site having a
//! bad day would overwrite a real chapter with a stub.

use anyhow::{anyhow, Result};

use crate::model::Chapter;
use crate::source::Source;
use crate::store::{Store, StoredSource};

/// Wordings that mark a gating placeholder rather than prose.
const GATE_PHRASES: &[&str] = &[
    "requires a free account",
    "log in to continue",
    "log in to keep reading",
    "sign in to continue",
    "sign up to continue",
    "create a free account",
    "subscribe to continue reading",
];

/// Only bodies shorter than this are even considered. A placeholder is a
/// sentence or two; this is generous enough to cover a wordier one while
/// keeping the scan off the whole table.
pub const STUB_MAX_CHARS: usize = 2000;

/// Whether a stored body is a gating placeholder rather than the chapter.
pub fn looks_like_gate_stub(paragraphs: &[String]) -> bool {
    let text = paragraphs.join(" ");
    if text.chars().count() > STUB_MAX_CHARS {
        return false;
    }
    let lowered = text.to_lowercase();
    GATE_PHRASES.iter().any(|p| lowered.contains(p))
}

/// What a repair pass did.
#[derive(Debug, Default)]
pub struct RepairReport {
    /// Chapter numbers whose text was replaced (or would be, on a dry run).
    pub repaired: Vec<u32>,
    /// Chapters left alone, with why.
    pub skipped: Vec<(u32, String)>,
}

impl RepairReport {
    pub fn is_empty(&self) -> bool {
        self.repaired.is_empty() && self.skipped.is_empty()
    }
}

/// Re-fetch and replace a novel's placeholder chapters.
///
/// `only` repairs one specific chapter regardless of how its stored text
/// looks — for a bad chapter whose wording this doesn't recognise. The
/// safety checks on the *incoming* text still apply. `dry_run` reports without
/// writing.
pub async fn repair_novel(
    store: &Store,
    novel_id: i64,
    sources: &[(StoredSource, Box<dyn Source>)],
    only: Option<u32>,
    dry_run: bool,
    mut on_progress: impl FnMut(u32, usize, usize),
) -> Result<RepairReport> {
    let mut report = RepairReport::default();

    let targets: Vec<Chapter> = match only {
        Some(number) => match store.load_chapter(novel_id, number)? {
            Some(c) => vec![c],
            None => {
                report
                    .skipped
                    .push((number, "not stored for this novel".into()));
                return Ok(report);
            }
        },
        None => store
            .chapters_shorter_than(novel_id, STUB_MAX_CHARS)?
            .into_iter()
            .filter(|c| looks_like_gate_stub(&c.paragraphs))
            .collect(),
    };
    if targets.is_empty() {
        return Ok(report);
    }
    if sources.is_empty() {
        return Err(anyhow!("no usable source to re-fetch from"));
    }

    // One discovery pass per source, reused for every chapter below.
    let mut discovered = Vec::new();
    for (meta, src) in sources {
        match src.discover_chapters(&meta.url, None).await {
            Ok(refs) => discovered.push((meta, src, refs)),
            Err(e) => report
                .skipped
                .push((0, format!("discovery failed for {}: {e}", meta.name))),
        }
    }

    let total = targets.len();
    for (i, stored) in targets.into_iter().enumerate() {
        on_progress(stored.number, i + 1, total);
        let mut outcome = Err("no source lists this chapter".to_string());

        for (meta, src, refs) in &discovered {
            let Some(cref) = refs.iter().find(|r| r.number == stored.number) else {
                continue;
            };
            match src.fetch_chapter(cref).await {
                Ok(fresh) => {
                    if let Err(why) = acceptable_replacement(&stored, &fresh) {
                        outcome = Err(why);
                        continue;
                    }
                    if !dry_run {
                        store.update_chapter_content(novel_id, meta.id, &fresh)?;
                    }
                    outcome = Ok(());
                    break;
                }
                Err(e) => outcome = Err(format!("{}: {e}", meta.name)),
            }
        }

        match outcome {
            Ok(()) => report.repaired.push(stored.number),
            Err(why) => report.skipped.push((stored.number, why)),
        }
    }
    Ok(report)
}

/// Refuse a replacement that isn't an improvement.
fn acceptable_replacement(stored: &Chapter, fresh: &Chapter) -> Result<(), String> {
    if looks_like_gate_stub(&fresh.paragraphs) {
        return Err("the source served the same kind of placeholder".into());
    }
    let old = stored.paragraphs.join(" ").chars().count();
    let new = fresh.paragraphs.join(" ").chars().count();
    if new <= old {
        return Err(format!("replacement is no longer than what is stored ({new} vs {old} chars)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paras(s: &str) -> Vec<String> {
        s.split("\n\n").map(str::to_string).collect()
    }

    #[test]
    fn recognises_a_gating_placeholder() {
        // The exact shape found in a real library.
        let stub = paras(
            "This chapter requires a free account to read. Sign up or log in to \
             continue reading \"Cultivation Online\".",
        );
        assert!(looks_like_gate_stub(&stub));
    }

    /// The case that makes length-based detection unsafe: a real, short
    /// author's note. Deleting one of these to "repair" it is a pure loss.
    #[test]
    fn leaves_a_genuinely_short_chapter_alone() {
        let note = paras(
            "Hello guys, how have you been?\n\nIt's me, the author.\n\nThanks for \
             waiting and supporting till now.\n\nSorry for the late update and the \
             bad news.",
        );
        assert!(!looks_like_gate_stub(&note));
    }

    /// A long chapter that merely mentions signing in is prose, not a gate.
    #[test]
    fn a_long_chapter_is_never_a_stub() {
        let mut body = "He had to log in to continue the simulation. ".repeat(80);
        body.push_str("The gate closed behind him.");
        assert!(body.chars().count() > STUB_MAX_CHARS);
        assert!(!looks_like_gate_stub(&paras(&body)));
    }

    fn chapter(number: u32, body: &str) -> Chapter {
        Chapter {
            number,
            title: format!("Ch {number}"),
            paragraphs: paras(body),
        }
    }

    /// A source serving canned bodies per chapter number.
    struct MockSource {
        name: String,
        bodies: std::collections::BTreeMap<u32, String>,
    }

    impl MockSource {
        fn new(name: &str, bodies: &[(u32, &str)]) -> Self {
            Self {
                name: name.into(),
                bodies: bodies.iter().map(|(n, b)| (*n, b.to_string())).collect(),
            }
        }
    }

    #[async_trait::async_trait]
    impl Source for MockSource {
        fn name(&self) -> &str {
            &self.name
        }
        fn matches(&self, _url: &str) -> bool {
            true
        }
        async fn fetch_novel(&self, url: &str) -> Result<crate::model::NovelMeta> {
            Ok(crate::model::NovelMeta {
                title: "Mock".into(),
                author: None,
                cover_url: None,
                genre: None,
                status_hint: crate::model::NovelStatus::Unknown,
                source_url: url.into(),
            })
        }
        async fn discover_chapters(
            &self,
            _url: &str,
            _needed: Option<usize>,
        ) -> Result<Vec<crate::model::ChapterRef>> {
            Ok(self
                .bodies
                .keys()
                .map(|n| crate::model::ChapterRef {
                    number: *n,
                    title: format!("Ch {n}"),
                    url: format!("mock://{}/{n}", self.name),
                })
                .collect())
        }
        async fn fetch_chapter(&self, c: &crate::model::ChapterRef) -> Result<Chapter> {
            let body = self
                .bodies
                .get(&c.number)
                .ok_or_else(|| anyhow!("mock lacks ch.{}", c.number))?;
            Ok(chapter(c.number, body))
        }
    }

    const GATED: &str = "This chapter requires a free account to read. \
                         Sign up or log in to continue reading \"Mock Novel\".";
    const REAL: &str = "The morning broke over the ruined city, and Sunny woke to \
                        the sound of the Nightmare Spell calling his name again.";

    fn store_with_gated_chapter() -> (Store, i64, i64) {
        let store = Store::open_in_memory().unwrap();
        let meta = crate::model::NovelMeta {
            title: "Mock Novel".into(),
            author: Some("A".into()),
            cover_url: None,
            genre: None,
            status_hint: crate::model::NovelStatus::Ongoing,
            source_url: "https://primary.example/n".into(),
        };
        let id = store.subscribe(&meta, "primary").unwrap();
        let primary = store.find_novel("1").unwrap().unwrap().primary_source().unwrap().id;
        store.insert_chapter_if_absent(id, primary, &chapter(1, GATED)).unwrap();
        (store, id, primary)
    }

    /// The question this answers: if the primary is still gated but a fallback
    /// carries the full text, does repair reach the fallback? It must — falling
    /// through on a rejected replacement is the whole point of trying each
    /// source in turn.
    #[tokio::test]
    async fn falls_through_to_a_fallback_when_the_primary_is_still_gated() {
        let (store, id, _) = store_with_gated_chapter();
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();
        let novel = store.find_novel(&id.to_string()).unwrap().unwrap();
        let sources: Vec<(StoredSource, Box<dyn Source>)> = novel
            .sources
            .into_iter()
            .zip(vec![
                Box::new(MockSource::new("primary", &[(1, GATED)])) as Box<dyn Source>,
                Box::new(MockSource::new("fallback", &[(1, REAL)])),
            ])
            .collect();

        let report = repair_novel(&store, id, &sources, None, false, |_, _, _| {})
            .await
            .unwrap();

        assert_eq!(report.repaired, vec![1], "repaired from the fallback");
        let stored = store.load_chapter(id, 1).unwrap().unwrap();
        assert_eq!(stored.paragraphs.join(" "), REAL);
    }

    /// With every source gated there is nothing to repair from, and the stored
    /// chapter must be left exactly as it was rather than churned.
    #[tokio::test]
    async fn leaves_the_chapter_alone_when_every_source_is_gated() {
        let (store, id, _) = store_with_gated_chapter();
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();
        let novel = store.find_novel(&id.to_string()).unwrap().unwrap();
        let sources: Vec<(StoredSource, Box<dyn Source>)> = novel
            .sources
            .into_iter()
            .zip(vec![
                Box::new(MockSource::new("primary", &[(1, GATED)])) as Box<dyn Source>,
                Box::new(MockSource::new("fallback", &[(1, GATED)])),
            ])
            .collect();

        let report = repair_novel(&store, id, &sources, None, false, |_, _, _| {})
            .await
            .unwrap();

        assert!(report.repaired.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].1.contains("placeholder"), "{:?}", report.skipped);
        assert_eq!(store.load_chapter(id, 1).unwrap().unwrap().paragraphs.join(" "), GATED);
    }

    /// A dry run reports what it would do and writes nothing.
    #[tokio::test]
    async fn dry_run_changes_nothing() {
        let (store, id, _) = store_with_gated_chapter();
        let novel = store.find_novel(&id.to_string()).unwrap().unwrap();
        let sources: Vec<(StoredSource, Box<dyn Source>)> = novel
            .sources
            .into_iter()
            .zip(vec![Box::new(MockSource::new("primary", &[(1, REAL)])) as Box<dyn Source>])
            .collect();

        let report = repair_novel(&store, id, &sources, None, true, |_, _, _| {})
            .await
            .unwrap();
        assert_eq!(report.repaired, vec![1]);
        assert_eq!(
            store.load_chapter(id, 1).unwrap().unwrap().paragraphs.join(" "),
            GATED,
            "dry run must not write"
        );
    }

    #[test]
    fn refuses_to_overwrite_prose_with_a_placeholder() {
        let stored = chapter(1, "A real chapter with actual prose in it.");
        let gated = chapter(1, "This chapter requires a free account to read.");
        let err = acceptable_replacement(&stored, &gated).unwrap_err();
        assert!(err.contains("placeholder"), "{err}");
    }

    #[test]
    fn refuses_a_replacement_that_is_not_an_improvement() {
        let stored = chapter(1, "This chapter requires a free account to read.");
        let shorter = chapter(1, "Nope.");
        assert!(acceptable_replacement(&stored, &shorter).is_err());
        // A real chapter is longer, so it goes through.
        let real = chapter(1, "The morning broke over the ruined city, and Sunny woke.");
        assert!(acceptable_replacement(&stored, &real).is_ok());
    }
}
