//! Re-downloading chapters that are already stored.
//!
//! Sync can never do this: `insert_chapter_if_absent` is `OR IGNORE`, so once a
//! chapter is stored it is never looked at again, however wrong it turned out
//! to be. `repair` covers the narrow case of a gating placeholder; this covers
//! the general one — the site itself changed.
//!
//! Two things prompt it, and they need different handling:
//!
//! - The site **corrected chapters in place** — a run that was accidentally
//!   duplicated (152, 153 and 154 all serving chapter 152's text) now has the
//!   right text at each number, or a chapter was updated with content it was
//!   missing. Re-fetching and overwriting fixes this, and nothing is deleted.
//! - The site **removed chapters and renumbered** around them. Now stored rows
//!   sit at numbers the source no longer has, and no amount of overwriting
//!   reaches them. Only deletion does, which is what `drop_missing` is for.
//!
//! Deletion is opt-in because downloaded text is the thing Vesper exists to
//! keep. Two rules make it safe: a chapter is only dropped when discovery
//! *succeeded* and the sources genuinely no longer list it (never because a
//! fetch failed), and nothing is deleted before its replacement is in hand — a
//! transient failure leaves the stored chapter exactly where it was.

use std::collections::BTreeSet;

use anyhow::{anyhow, Result};

use crate::model::{Chapter, ChapterRef, DerivedState};
use crate::repair::looks_like_gate_stub;
use crate::source::Source;
use crate::store::{Store, StoredSource};

/// What a refetch pass did. Every target lands in exactly one of these.
#[derive(Debug, Default)]
pub struct RefetchReport {
    /// Stored text differed from the source's and was rewritten.
    pub replaced: Vec<u32>,
    /// Re-downloaded and identical to what was already stored.
    pub unchanged: Vec<u32>,
    /// Not previously stored; now downloaded.
    pub added: Vec<u32>,
    /// Deleted because no source lists them any more (`drop_missing` only).
    pub removed: Vec<u32>,
    /// Left as they were, with why.
    pub skipped: Vec<(u32, String)>,
}

impl RefetchReport {
    /// Whether anything about the library actually changed.
    pub fn changed(&self) -> bool {
        !self.replaced.is_empty() || !self.added.is_empty() || !self.removed.is_empty()
    }
}

/// Re-download `targets` (or the whole novel when `None`) and replace what is
/// stored.
///
/// `drop_missing` additionally deletes targeted chapters that no source lists
/// any more — the renumbering case above. `dry_run` reports without writing.
pub async fn refetch_novel(
    store: &Store,
    novel_id: i64,
    sources: &[(StoredSource, Box<dyn Source>)],
    targets: Option<&BTreeSet<u32>>,
    drop_missing: bool,
    dry_run: bool,
    mut on_progress: impl FnMut(u32, usize, usize),
) -> Result<RefetchReport> {
    let mut report = RefetchReport::default();
    if sources.is_empty() {
        return Err(anyhow!("no usable source to re-download from"));
    }

    // Discover once, up front. This is also what makes deletion safe: if not a
    // single source could be read, we have no basis for saying a chapter is
    // gone, and must not act as though we do.
    let mut discovered: Vec<(&StoredSource, &dyn Source, Vec<ChapterRef>)> = Vec::new();
    let mut failures = Vec::new();
    for (meta, src) in sources {
        match src.discover_chapters(&meta.url, None).await {
            Ok(refs) => discovered.push((meta, src.as_ref(), refs)),
            Err(e) => failures.push(format!("{}: {e}", meta.name)),
        }
    }
    if discovered.is_empty() {
        return Err(anyhow!(
            "could not read the chapter list from any source ({})",
            failures.join("; ")
        ));
    }
    for f in &failures {
        report.skipped.push((0, format!("discovery failed for {f}")));
    }

    let stored = store.stored_chapter_numbers(novel_id)?;
    let listed: BTreeSet<u32> = discovered
        .iter()
        .flat_map(|(_, _, refs)| refs.iter().map(|r| r.number))
        .collect();

    // Default target is everything the novel has or the sources offer, so a
    // bare `refetch` both rewrites what is stored and picks up anything missing.
    let targets: BTreeSet<u32> = match targets {
        Some(t) => t.clone(),
        None => stored.union(&listed).copied().collect(),
    };

    let total = targets.len();
    let mut still_absent = Vec::new();
    for (i, number) in targets.iter().copied().enumerate() {
        on_progress(number, i + 1, total);
        let existing = store.load_chapter(novel_id, number)?;

        let Some((fresh, source_id)) = fetch_one(&discovered, number, &mut report).await? else {
            // Nothing served it. Whether that is "gone" or "the site is having a
            // bad day" is decided below, from the chapter list rather than here.
            if existing.is_none() {
                still_absent.push(number);
            }
            continue;
        };

        match existing {
            None => {
                if !dry_run {
                    store.insert_chapter_if_absent(novel_id, source_id, &fresh)?;
                }
                report.added.push(number);
            }
            Some(old) if same_text(&old, &fresh) => report.unchanged.push(number),
            Some(_) => {
                if !dry_run {
                    store.update_chapter_content(novel_id, source_id, &fresh)?;
                }
                report.replaced.push(number);
            }
        }
    }

    // Only now, with a chapter list actually in hand, is it safe to say a
    // stored chapter is gone rather than merely unfetchable this minute.
    if drop_missing {
        let gone: BTreeSet<u32> = targets
            .iter()
            .copied()
            .filter(|n| stored.contains(n) && !listed.contains(n))
            .collect();
        if !gone.is_empty() {
            if !dry_run {
                store.delete_chapters(novel_id, &gone)?;
            }
            report.removed.extend(gone);
        }
    }

    // A target the sources list but nothing could serve leaves a real hole. Put
    // the novel back to Backfilling so an ordinary sync walks the full list and
    // fills it, rather than a delta check skipping straight past it.
    let recoverable = still_absent.iter().any(|n| listed.contains(n));
    if recoverable && !dry_run {
        store.set_derived_state(novel_id, DerivedState::Backfilling)?;
    }

    Ok(report)
}

/// Fetch one chapter from the highest-priority source that can serve it,
/// returning the chapter and the source id that supplied it.
async fn fetch_one(
    discovered: &[(&StoredSource, &dyn Source, Vec<ChapterRef>)],
    number: u32,
    report: &mut RefetchReport,
) -> Result<Option<(Chapter, i64)>> {
    let mut last_error: Option<String> = None;
    for (meta, src, refs) in discovered {
        let Some(cref) = refs.iter().find(|r| r.number == number) else {
            continue;
        };
        match src.fetch_chapter(cref).await {
            Ok(chapter) => {
                // The same rule sync's upgrade pass follows: a gating
                // placeholder is not content, and must never replace prose.
                if looks_like_gate_stub(&chapter.paragraphs) {
                    last_error = Some(format!("{} served a login placeholder", meta.name));
                    continue;
                }
                return Ok(Some((chapter, meta.id)));
            }
            Err(e) => last_error = Some(format!("{}: {e}", meta.name)),
        }
    }
    if let Some(why) = last_error {
        report.skipped.push((number, why));
    }
    Ok(None)
}

fn same_text(a: &Chapter, b: &Chapter) -> bool {
    a.title == b.title && a.paragraphs == b.paragraphs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NovelMeta, NovelStatus};
    use std::collections::BTreeMap;

    /// A source serving canned bodies, with optional per-chapter failures.
    struct MockSource {
        name: String,
        bodies: BTreeMap<u32, String>,
        broken: BTreeSet<u32>,
        dead: bool,
    }

    impl MockSource {
        fn new(name: &str, bodies: &[(u32, &str)]) -> Self {
            Self {
                name: name.into(),
                bodies: bodies.iter().map(|(n, b)| (*n, b.to_string())).collect(),
                broken: BTreeSet::new(),
                dead: false,
            }
        }

        /// Chapters this source lists but errors on when fetched.
        fn failing(mut self, numbers: &[u32]) -> Self {
            self.broken = numbers.iter().copied().collect();
            self
        }

        /// Discovery itself fails (site down).
        fn unreachable(name: &str) -> Self {
            Self {
                name: name.into(),
                bodies: BTreeMap::new(),
                broken: BTreeSet::new(),
                dead: true,
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
        async fn fetch_novel(&self, url: &str) -> Result<NovelMeta> {
            Ok(NovelMeta {
                title: "Mock".into(),
                author: None,
                cover_url: None,
                genre: None,
                status_hint: NovelStatus::Unknown,
                source_url: url.into(),
            })
        }
        async fn discover_chapters(
            &self,
            _url: &str,
            _needed: Option<usize>,
        ) -> Result<Vec<ChapterRef>> {
            if self.dead {
                return Err(anyhow!("{} is unreachable", self.name));
            }
            Ok(self
                .bodies
                .keys()
                .map(|n| ChapterRef {
                    number: *n,
                    title: format!("Ch {n}"),
                    url: format!("mock://{}/{n}", self.name),
                })
                .collect())
        }
        async fn fetch_chapter(&self, c: &ChapterRef) -> Result<Chapter> {
            if self.broken.contains(&c.number) {
                return Err(anyhow!("ch.{} timed out", c.number));
            }
            let body = self
                .bodies
                .get(&c.number)
                .ok_or_else(|| anyhow!("mock lacks ch.{}", c.number))?;
            Ok(Chapter {
                number: c.number,
                title: format!("Ch {}", c.number),
                paragraphs: vec![body.clone()],
            })
        }
    }

    fn setup(stored: &[(u32, &str)]) -> (Store, i64, i64) {
        let store = Store::open_in_memory().unwrap();
        let meta = NovelMeta {
            title: "Mock Novel".into(),
            author: Some("A".into()),
            cover_url: None,
            genre: None,
            status_hint: NovelStatus::Ongoing,
            source_url: "https://primary.example/n".into(),
        };
        let id = store.subscribe(&meta, "primary").unwrap();
        let src = store
            .find_novel(&id.to_string())
            .unwrap()
            .unwrap()
            .primary_source()
            .unwrap()
            .id;
        for (n, body) in stored {
            store
                .insert_chapter_if_absent(
                    id,
                    src,
                    &Chapter {
                        number: *n,
                        title: format!("Ch {n}"),
                        paragraphs: vec![(*body).to_string()],
                    },
                )
                .unwrap();
        }
        store.set_derived_state(id, DerivedState::Live).unwrap();
        (store, id, src)
    }

    fn pair(store: &Store, id: i64, mocks: Vec<Box<dyn Source>>) -> Vec<(StoredSource, Box<dyn Source>)> {
        let novel = store.find_novel(&id.to_string()).unwrap().unwrap();
        novel.sources.into_iter().zip(mocks).collect()
    }

    fn body_of(store: &Store, id: i64, number: u32) -> String {
        store
            .load_chapter(id, number)
            .unwrap()
            .unwrap()
            .paragraphs
            .join(" ")
    }

    /// The case that prompted this: the site served the same text at 152, 153
    /// and 154, then fixed it. Refetch rewrites the wrong ones and reports the
    /// already-correct one as unchanged rather than churning it.
    #[tokio::test]
    async fn rewrites_chapters_the_site_has_corrected() {
        let (store, id, _) = setup(&[(152, "dup"), (153, "dup"), (154, "dup")]);
        let sources = pair(
            &store,
            id,
            vec![Box::new(MockSource::new(
                "primary",
                &[(152, "dup"), (153, "real 153"), (154, "real 154")],
            ))],
        );
        let targets: BTreeSet<u32> = [152, 153, 154].into_iter().collect();

        let r = refetch_novel(&store, id, &sources, Some(&targets), false, false, |_, _, _| {})
            .await
            .unwrap();

        assert_eq!(r.replaced, vec![153, 154]);
        assert_eq!(r.unchanged, vec![152], "identical text is not rewritten");
        assert!(r.removed.is_empty());
        assert_eq!(body_of(&store, id, 153), "real 153");
        assert_eq!(body_of(&store, id, 154), "real 154");
    }

    /// A replaced chapter must be flagged for re-export, or the EPUB keeps the
    /// stale text.
    #[tokio::test]
    async fn a_rewrite_marks_the_novel_for_re_export() {
        let (store, id, _) = setup(&[(1, "old")]);
        store.mark_all_exported(id).unwrap();
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &[(1, "new")]))]);
        let targets: BTreeSet<u32> = [1].into_iter().collect();

        refetch_novel(&store, id, &sources, Some(&targets), false, false, |_, _, _| {})
            .await
            .unwrap();

        let exported: i64 = store
            .load_chapters(id)
            .map(|c| c.len() as i64)
            .unwrap();
        assert_eq!(exported, 1);
        // update_chapter_content clears the exported flag; retention keys on it.
        assert_eq!(store.apply_retention(0).unwrap(), 0, "un-exported, so not purgeable");
    }

    #[tokio::test]
    async fn dry_run_reports_without_writing() {
        let (store, id, _) = setup(&[(1, "old")]);
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &[(1, "new")]))]);

        let r = refetch_novel(&store, id, &sources, None, false, true, |_, _, _| {})
            .await
            .unwrap();
        assert_eq!(r.replaced, vec![1]);
        assert_eq!(body_of(&store, id, 1), "old", "dry run must not write");
    }

    /// The renumbering case: the site dropped the duplicates entirely, so 153
    /// and 154 no longer exist. Only deletion reaches those.
    #[tokio::test]
    async fn drop_missing_removes_chapters_the_source_no_longer_lists() {
        let (store, id, _) = setup(&[(152, "dup"), (153, "dup"), (154, "dup")]);
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &[(152, "real 152")]))]);

        // Without the flag, the stale rows are kept.
        let kept = refetch_novel(&store, id, &sources, None, false, false, |_, _, _| {})
            .await
            .unwrap();
        assert!(kept.removed.is_empty());
        assert_eq!(store.stored_chapter_numbers(id).unwrap().len(), 3);

        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &[(152, "real 152")]))]);
        let r = refetch_novel(&store, id, &sources, None, true, false, |_, _, _| {})
            .await
            .unwrap();
        assert_eq!(r.removed, vec![153, 154]);
        assert_eq!(store.stored_chapter_numbers(id).unwrap(), [152].into_iter().collect());
        assert_eq!(body_of(&store, id, 152), "real 152");
    }

    /// The rule that keeps deletion honest: a chapter that merely failed to
    /// fetch is still listed by the source, so it must survive `drop_missing`.
    #[tokio::test]
    async fn a_failed_fetch_is_never_treated_as_a_deletion() {
        let (store, id, _) = setup(&[(1, "one"), (2, "two")]);
        let sources = pair(
            &store,
            id,
            vec![Box::new(MockSource::new("primary", &[(1, "one"), (2, "two v2")]).failing(&[2]))],
        );

        let r = refetch_novel(&store, id, &sources, None, true, false, |_, _, _| {})
            .await
            .unwrap();

        assert!(r.removed.is_empty(), "a timeout is not evidence a chapter is gone");
        assert_eq!(body_of(&store, id, 2), "two", "stored text left intact");
        assert!(r.skipped.iter().any(|(n, _)| *n == 2));
    }

    /// With no source readable there is no basis for any of this, least of all
    /// deletion — it errors rather than acting on an empty chapter list.
    #[tokio::test]
    async fn refuses_to_run_when_no_source_can_be_read() {
        let (store, id, _) = setup(&[(1, "one")]);
        let sources = pair(&store, id, vec![Box::new(MockSource::unreachable("primary"))]);

        let err = refetch_novel(&store, id, &sources, None, true, false, |_, _, _| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not read the chapter list"), "{err}");
        assert_eq!(body_of(&store, id, 1), "one", "nothing touched");
    }

    /// Refetch honours fallbacks, and will not let a gated source overwrite
    /// prose with a login placeholder.
    #[tokio::test]
    async fn falls_back_and_refuses_a_placeholder() {
        const GATE: &str = "This chapter requires a free account to read.";
        let (store, id, _) = setup(&[(1, "real text")]);
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();
        let sources = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[(1, GATE)])) as Box<dyn Source>,
                Box::new(MockSource::new("fallback", &[(1, "fuller text")])),
            ],
        );

        let r = refetch_novel(&store, id, &sources, None, false, false, |_, _, _| {})
            .await
            .unwrap();

        assert_eq!(r.replaced, vec![1]);
        assert_eq!(body_of(&store, id, 1), "fuller text", "came from the fallback");
    }

    /// A chapter the sources list but nobody could serve leaves a hole, so the
    /// novel goes back to Backfilling and an ordinary sync fills it — a delta
    /// check would skip straight past a mid-range hole.
    #[tokio::test]
    async fn an_unfillable_hole_sends_the_novel_back_to_backfilling() {
        let (store, id, _) = setup(&[(1, "one")]);
        let sources = pair(
            &store,
            id,
            vec![Box::new(MockSource::new("primary", &[(1, "one"), (2, "two")]).failing(&[2]))],
        );

        refetch_novel(&store, id, &sources, None, false, false, |_, _, _| {})
            .await
            .unwrap();

        let novel = store.find_novel(&id.to_string()).unwrap().unwrap();
        assert_eq!(novel.derived_state, DerivedState::Backfilling);
    }
}
