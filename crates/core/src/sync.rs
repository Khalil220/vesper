//! Syncing a novel from its ranked sources, with active fallback and a
//! Backfilling -> Live state machine driving discovery cost.
//!
//! **Backfilling** (initial bulk download): full ToC discovery; once every
//! discovered chapter is stored, the novel transitions to **Live**.
//!
//! **Live** (caught up): a cheap *delta* discovery — just the landing page,
//! which lists the latest chapters — instead of re-walking the whole ToC every
//! run. If the delta check reveals a gap (more new chapters than the landing
//! page surfaces), it falls back to a full walk for that pass.
//!
//! For every not-yet-stored chapter number, the chapter is fetched from the
//! highest-priority source that has it (primary authoritative; fallbacks fill
//! gaps). See DESIGN.md.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use anyhow::Result;

use crate::fetch::is_not_found;
use crate::model::{ChapterRef, DerivedState};
use crate::source::Source;
use crate::store::{Store, StoredSource};

/// A progress event emitted during a sync pass. Structured rather than a
/// pre-formatted string so each caller renders it to suit its output — an
/// interactive terminal draws an in-place `n/m` counter, a background run can
/// log periodically or ignore it. `done` counts from 1 up to `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncProgress {
    /// Downloading chapter bodies.
    Fetching { done: usize, total: usize },
    /// Re-fetching fallback-held chapters from the (now caught-up) primary.
    Upgrading { done: usize, total: usize },
}

/// Outcome of a sync pass.
#[derive(Debug)]
pub struct SyncReport {
    /// Chapters newly stored this pass.
    pub newly_fetched: u32,
    /// Of those, how many came from a fallback (non-primary) source.
    pub from_fallback: u32,
    /// Fallback-sourced chapters re-fetched from the primary this pass.
    pub upgraded: u32,
    /// The novel's derived state after this pass.
    pub new_state: DerivedState,
    /// Whether this pass used cheap delta discovery (vs a full ToC walk).
    pub delta_mode: bool,
    /// Non-fatal notices (source divergence, per-chapter failures, gap fallback).
    pub warnings: Vec<String>,
    /// Chapter numbers that failed *transiently* this pass (timeout/5xx) and will
    /// be retried next time — not permanent gaps.
    pub failures: Vec<u32>,
    /// Chapter numbers confirmed absent at the source (permanent 404 holes). These
    /// no longer block completion; they are surfaced so the gap stays visible.
    pub gaps: Vec<u32>,
    /// The caller asked to stop early (e.g. Ctrl+C); fetched chapters are saved
    /// and the run can be resumed. The state machine stays put (no transition).
    pub interrupted: bool,
}

type Discovered<'a> = Vec<(&'a StoredSource, &'a dyn Source, BTreeMap<u32, ChapterRef>)>;

/// Discover each source's chapter map, best-effort (a failed source is warned
/// and skipped, not fatal). `full` selects a complete ToC walk vs a delta check.
async fn discover<'a>(
    sources: &'a [(StoredSource, Box<dyn Source>)],
    full: bool,
    warnings: &mut Vec<String>,
) -> Discovered<'a> {
    let mut out = Discovered::new();
    for (meta, src) in sources {
        let res = if full {
            src.discover_chapters(&meta.url, None).await
        } else {
            src.discover_latest(&meta.url).await
        };
        match res {
            Ok(refs) => {
                let map = refs.into_iter().map(|r| (r.number, r)).collect();
                out.push((meta, src.as_ref(), map));
            }
            Err(e) => warnings.push(format!(
                "discovery failed for {}: {e}; skipping this source",
                meta.name
            )),
        }
    }
    out
}

/// Sync `novel_id` (currently in `state`) from `sources` (priority order,
/// primary first). `limit` caps chapters fetched this pass (0 = all missing).
///
/// `on_progress` is called once per chapter with a [`SyncProgress`] and returns
/// a [`ControlFlow`]: returning `Break` stops the pass cleanly after the current
/// chapter (which is already saved), leaving the run resumable — this is how the
/// CLI turns Ctrl+C into a graceful pause.
pub async fn sync_novel(
    store: &Store,
    novel_id: i64,
    state: DerivedState,
    sources: &[(StoredSource, Box<dyn Source>)],
    limit: usize,
    mut on_progress: impl FnMut(SyncProgress) -> ControlFlow<()>,
) -> Result<SyncReport> {
    let is_backfilling = matches!(state, DerivedState::Backfilling);
    let have = store.stored_chapter_numbers(novel_id)?;
    let max_have = have.iter().copied().max().unwrap_or(0);
    // Previously-recorded permanent gaps (404 holes). Re-probed automatically
    // when they reappear in a full walk's target below.
    let mut gaps = store.gaps(novel_id)?;

    let mut report = SyncReport {
        newly_fetched: 0,
        from_fallback: 0,
        upgraded: 0,
        new_state: state,
        delta_mode: !is_backfilling,
        warnings: Vec::new(),
        failures: Vec::new(),
        gaps: Vec::new(),
        interrupted: false,
    };

    // Backfilling walks the full ToC; a caught-up novel does a cheap delta check.
    let mut discovered = discover(sources, is_backfilling, &mut report.warnings).await;

    // If a delta check reveals new chapters that don't pick up right after the
    // last one we stored, the landing page didn't surface the whole tail — fall
    // back to a full walk so we don't leave a gap.
    if !is_backfilling {
        let min_new = discovered
            .iter()
            .flat_map(|(_, _, m)| m.keys().copied())
            .filter(|n| *n > max_have)
            .min();
        if let Some(min_new) = min_new {
            if min_new > max_have + 1 {
                report.warnings.push(format!(
                    "delta check found new chapters from ch.{min_new} but last stored is ch.{max_have}; \
                     falling back to a full walk to fill the gap"
                ));
                discovered = discover(sources, true, &mut report.warnings).await;
                report.delta_mode = false;
            }
        }
    }

    // Warn if sources disagree on how far the novel goes.
    if discovered.len() > 1 {
        let latest: Vec<(String, u32)> = discovered
            .iter()
            .map(|(m, _, map)| (m.name.clone(), map.keys().next_back().copied().unwrap_or(0)))
            .collect();
        let hi = latest.iter().map(|(_, c)| *c).max().unwrap_or(0);
        let lo = latest.iter().map(|(_, c)| *c).min().unwrap_or(0);
        if hi != lo {
            let detail = latest
                .iter()
                .map(|(n, c)| format!("{n}: up to ch.{c}"))
                .collect::<Vec<_>>()
                .join(", ");
            report.warnings.push(format!(
                "sources report different latest chapters ({detail}); \
                 gap-fill matches by number and may not align 1:1"
            ));
        }
    }

    // Target = union of discovered numbers; fetch those not yet stored.
    let mut target: BTreeSet<u32> = BTreeSet::new();
    for (_, _, map) in &discovered {
        target.extend(map.keys().copied());
    }
    let missing: Vec<u32> = target.difference(&have).copied().collect();
    let to_fetch: Vec<u32> = if limit == 0 {
        missing
    } else {
        missing.into_iter().take(limit).collect()
    };

    // Report progress as a structured `done/total` event per chapter; the caller
    // renders it (an interactive fetch draws a single in-place counter rather
    // than a line per chapter, which would be unusable for a big backfill).
    let total = to_fetch.len();
    for (i, num) in to_fetch.into_iter().enumerate() {
        let known_gap = gaps.contains(&num);
        let mut done = false;
        let mut attempted = false;
        let mut primary_404 = false;
        let mut primary_ok = false;
        for (idx, (meta, src, map)) in discovered.iter().enumerate() {
            let Some(cref) = map.get(&num) else { continue };
            attempted = true;
            let is_primary = meta.priority == 1;
            match src.fetch_chapter(cref).await {
                Ok(chapter) => {
                    if store.insert_chapter_if_absent(novel_id, meta.id, &chapter)? {
                        report.newly_fetched += 1;
                        if idx > 0 {
                            report.from_fallback += 1;
                        }
                    }
                    if is_primary {
                        primary_ok = true;
                    }
                    done = true;
                    break;
                }
                Err(e) => {
                    if is_primary && is_not_found(&e) {
                        primary_404 = true;
                    }
                    // Re-probing an already-known gap on every full walk shouldn't
                    // re-spam; only warn about a newly-seen failure.
                    if !known_gap {
                        report.warnings.push(format!(
                            "ch.{num} from {} failed: {e}; trying next source",
                            meta.name
                        ));
                    }
                }
            }
        }
        // A gap means the *primary* can't provide this chapter (permanent 404).
        // Recorded even when a fallback fills it, so the upgrade pass below stops
        // re-trying the primary for it every sync.
        if primary_ok {
            if gaps.remove(&num) {
                store.clear_gap(novel_id, num)?;
            }
        } else if primary_404 {
            if gaps.insert(num) {
                store.record_gap(novel_id, num)?;
            }
        }
        if !done && !primary_404 && attempted {
            // Couldn't fetch and it isn't a primary hole => transient; retry next.
            report.failures.push(num);
        }
        // The chapter just fetched is already committed, so stopping here is safe
        // and resumable.
        if on_progress(SyncProgress::Fetching { done: i + 1, total }).is_break() {
            report.interrupted = true;
            break;
        }
    }

    // Content upgrade: the primary is authoritative, so any chapter we're
    // currently holding from a fallback that the primary now offers gets
    // re-fetched from the primary and replaced. Normally there are none (steady
    // state is all-primary); this fires once after a lagging primary catches up.
    // Skipped when the fetch was interrupted — the user asked to stop.
    if !report.interrupted {
        if let Some((pmeta, psrc, pmap)) = discovered.iter().find(|(m, _, _)| m.priority == 1) {
            // Skip chapters the primary is a known 404 hole for — upgrading them is
            // permanently futile (that was the "upgrade of ch.N failed" spam).
            let upgradable: Vec<u32> = store
                .chapters_from_other_sources(novel_id, pmeta.id)?
                .into_iter()
                .filter(|n| !gaps.contains(n))
                .collect();
            let up_total = upgradable.len();
            for (i, num) in upgradable.into_iter().enumerate() {
                let Some(cref) = pmap.get(&num) else { continue };
                if on_progress(SyncProgress::Upgrading { done: i + 1, total: up_total }).is_break() {
                    report.interrupted = true;
                    break;
                }
                match psrc.fetch_chapter(cref).await {
                    Ok(chapter) => {
                        store.update_chapter_content(novel_id, pmeta.id, &chapter)?;
                        report.upgraded += 1;
                    }
                    Err(e) if is_not_found(&e) => {
                        // The primary permanently lacks this (it's fallback-filled);
                        // record the gap so future syncs skip it, and stay quiet.
                        if gaps.insert(num) {
                            store.record_gap(novel_id, num)?;
                        }
                    }
                    Err(e) => report.warnings.push(format!(
                        "upgrade of ch.{num} from {} failed: {e}",
                        pmeta.name
                    )),
                }
            }
        }
    }

    // Record each source's latest discovered chapter.
    for (meta, _, map) in &discovered {
        if let Some(max) = map.keys().next_back() {
            store.update_source_progress(meta.id, *max)?;
        }
    }

    // State transitions:
    // - Backfilling -> Live once every discovered chapter is stored.
    // - A non-backfilling novel that fetched something is (still) Live — activity
    //   overrides a prior LikelyComplete. With nothing new, its state is left
    //   unchanged; Live -> LikelyComplete is decided by `reevaluate_completion`.
    let now_have = store.stored_chapter_numbers(novel_id)?;
    let new_state = if is_backfilling {
        // Complete once every target chapter is either stored or a known gap. A
        // permanent 404 hole would otherwise wedge the novel in Backfilling
        // forever (and so never auto-export).
        let outstanding = target.difference(&now_have).any(|n| !gaps.contains(n));
        if !target.is_empty() && !outstanding {
            DerivedState::Live
        } else {
            DerivedState::Backfilling
        }
    } else if report.newly_fetched > 0 {
        DerivedState::Live
    } else {
        state
    };
    if new_state != state {
        store.set_derived_state(novel_id, new_state)?;
    }
    report.new_state = new_state;
    // Report only *unfilled* gaps as missing — a gap filled from a fallback is a
    // primary hole but not a missing chapter, so it shouldn't surface to the user.
    report.gaps = gaps.iter().copied().filter(|n| !now_have.contains(n)).collect();

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Chapter, NovelMeta, NovelStatus};
    use async_trait::async_trait;

    /// A source backed by an in-memory number->body map.
    struct MockSource {
        name: String,
        bodies: BTreeMap<u32, String>,
        /// discovery error (simulates a dead source).
        broken: bool,
        /// If set, `discover_latest` returns only the highest N numbers
        /// (simulates a landing page's short "latest" list).
        latest_window: Option<usize>,
        /// Numbers that discovery lists but that 404 on fetch (site holes).
        holes: BTreeSet<u32>,
    }

    impl MockSource {
        fn new(name: &str, numbers: &[u32]) -> Self {
            let bodies = numbers
                .iter()
                .map(|n| (*n, format!("{name} body {n}")))
                .collect();
            Self {
                name: name.into(),
                bodies,
                broken: false,
                latest_window: None,
                holes: BTreeSet::new(),
            }
        }

        /// Discovered chapters that 404 on fetch (permanent site holes).
        fn with_holes(mut self, holes: &[u32]) -> Self {
            self.holes = holes.iter().copied().collect();
            self
        }

        fn with_latest_window(mut self, n: usize) -> Self {
            self.latest_window = Some(n);
            self
        }

        fn broken(name: &str) -> Self {
            Self {
                name: name.into(),
                bodies: BTreeMap::new(),
                broken: true,
                latest_window: None,
                holes: BTreeSet::new(),
            }
        }

        /// Numbers this source lists in discovery: real bodies plus holes.
        fn discovered_numbers(&self) -> BTreeSet<u32> {
            self.bodies.keys().copied().chain(self.holes.iter().copied()).collect()
        }

        fn refs_for<'a>(&self, numbers: impl Iterator<Item = &'a u32>) -> Vec<ChapterRef> {
            numbers
                .map(|n| ChapterRef {
                    number: *n,
                    title: format!("Ch {n}"),
                    url: format!("mock://{}/{n}", self.name),
                })
                .collect()
        }
    }

    #[async_trait]
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
            if self.broken {
                return Err(anyhow::anyhow!("{} is unreachable", self.name));
            }
            let nums = self.discovered_numbers();
            Ok(self.refs_for(nums.iter()))
        }
        async fn discover_latest(&self, url: &str) -> Result<Vec<ChapterRef>> {
            match self.latest_window {
                Some(k) => {
                    let nums = self.discovered_numbers();
                    let tail: Vec<&u32> = nums.iter().rev().take(k).collect();
                    Ok(self.refs_for(tail.into_iter().rev()))
                }
                None => self.discover_chapters(url, None).await,
            }
        }
        async fn fetch_chapter(&self, chapter: &ChapterRef) -> Result<Chapter> {
            if self.holes.contains(&chapter.number) {
                return Err(anyhow::anyhow!(crate::fetch::NotFound {
                    url: chapter.url.clone(),
                    status: 404,
                }));
            }
            let body = self
                .bodies
                .get(&chapter.number)
                .ok_or_else(|| anyhow::anyhow!("mock lacks ch.{}", chapter.number))?;
            Ok(Chapter {
                number: chapter.number,
                title: chapter.title.clone(),
                paragraphs: vec![body.clone()],
            })
        }
    }

    fn subscribe(store: &Store) -> i64 {
        let meta = NovelMeta {
            title: "Mock Novel".into(),
            author: Some("A".into()),
            cover_url: None,
            genre: None,
            status_hint: NovelStatus::Ongoing,
            source_url: "https://primary.example/n".into(),
        };
        store.subscribe(&meta, "primary").unwrap()
    }

    /// Build (StoredSource, adapter) pairs for a novel from provided mocks,
    /// aligned to stored priority order.
    fn pair(store: &Store, id: i64, mocks: Vec<Box<dyn Source>>) -> Vec<(StoredSource, Box<dyn Source>)> {
        let novel = store.find_novel(&id.to_string()).unwrap().unwrap();
        novel.sources.into_iter().zip(mocks).collect()
    }

    #[tokio::test]
    async fn fallback_fills_chapters_the_primary_lacks() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();
        let sources = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 2, 3])),
                Box::new(MockSource::new("fallback", &[1, 2, 3, 4, 5])),
            ],
        );

        let report = sync_novel(&store, id, DerivedState::Backfilling, &sources, 0, |_| ControlFlow::Continue(()))
            .await
            .unwrap();

        assert_eq!(report.newly_fetched, 5);
        assert_eq!(report.from_fallback, 2, "ch.4 and ch.5 come from the fallback");
        assert!(report.failures.is_empty());
        assert!(report.warnings.iter().any(|w| w.contains("different latest")));

        let ch4 = store.load_chapters(id).unwrap().into_iter().find(|c| c.number == 4).unwrap();
        assert_eq!(ch4.paragraphs, vec!["fallback body 4"]);
        let ch1 = store.load_chapters(id).unwrap().into_iter().find(|c| c.number == 1).unwrap();
        assert_eq!(ch1.paragraphs, vec!["primary body 1"]);
    }

    #[tokio::test]
    async fn fetch_progress_counts_up_to_total() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        let numbers: Vec<u32> = (1..=60).collect();
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &numbers))]);

        let mut events: Vec<SyncProgress> = Vec::new();
        let report = sync_novel(&store, id, DerivedState::Backfilling, &sources, 0, |p| {
            events.push(p);
            ControlFlow::Continue(())
        })
        .await
        .unwrap();
        assert_eq!(report.newly_fetched, 60);

        // One structured Fetching event per chapter, counting 1..=60 against a
        // constant total. The caller collapses these into a single in-place line.
        let fetching: Vec<(usize, usize)> = events
            .iter()
            .filter_map(|p| match p {
                SyncProgress::Fetching { done, total } => Some((*done, *total)),
                _ => None,
            })
            .collect();
        assert_eq!(fetching.len(), 60);
        assert!(fetching.iter().all(|(_, total)| *total == 60), "total is constant");
        assert_eq!(fetching.first(), Some(&(1, 60)));
        assert_eq!(fetching.last(), Some(&(60, 60)), "reaches total");
        let dones: Vec<usize> = fetching.iter().map(|(d, _)| *d).collect();
        assert!(dones.windows(2).all(|w| w[1] == w[0] + 1), "monotonic 1..=60");
    }

    #[tokio::test]
    async fn break_from_callback_stops_early_and_preserves_chapters() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        let numbers: Vec<u32> = (1..=60).collect();
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &numbers))]);

        // Ask to stop once 10 chapters are in (as the CLI does on Ctrl+C).
        let report = sync_novel(&store, id, DerivedState::Backfilling, &sources, 0, |p| match p {
            SyncProgress::Fetching { done, .. } if done >= 10 => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        })
        .await
        .unwrap();

        assert!(report.interrupted, "the pass reports it was interrupted");
        assert_eq!(report.newly_fetched, 10, "stopped after the 10th chapter");
        assert_eq!(store.stored_chapter_numbers(id).unwrap().len(), 10, "fetched chapters persisted");
        assert_eq!(
            report.new_state,
            DerivedState::Backfilling,
            "an interrupted backfill does not transition to Live"
        );
    }

    #[tokio::test]
    async fn permanent_gap_does_not_wedge_backfill() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        // Primary lists 1..=5 but ch.3 is a 404 hole (like lightnovelworld).
        let sources = pair(
            &store,
            id,
            vec![Box::new(MockSource::new("primary", &[1, 2, 4, 5]).with_holes(&[3]))],
        );
        let report = sync_novel(&store, id, DerivedState::Backfilling, &sources, 0, |_| {
            ControlFlow::Continue(())
        })
        .await
        .unwrap();

        assert_eq!(report.newly_fetched, 4, "the 4 real chapters are fetched");
        assert_eq!(report.gaps, vec![3], "ch.3 recorded as a permanent gap");
        assert!(report.failures.is_empty(), "a 404 is a gap, not a transient failure");
        assert_eq!(
            report.new_state,
            DerivedState::Live,
            "completes despite the hole instead of wedging in Backfilling"
        );
        assert_eq!(store.gaps(id).unwrap().into_iter().collect::<Vec<_>>(), vec![3]);
    }

    #[tokio::test]
    async fn gap_filled_from_fallback_is_recorded_but_not_surfaced() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();
        // Primary's ch.3 is a hole, but the fallback has it.
        let sources = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 2, 4, 5]).with_holes(&[3])),
                Box::new(MockSource::new("fallback", &[3])),
            ],
        );
        let report = sync_novel(&store, id, DerivedState::Backfilling, &sources, 0, |_| {
            ControlFlow::Continue(())
        })
        .await
        .unwrap();

        assert_eq!(report.newly_fetched, 5);
        assert_eq!(report.from_fallback, 1, "ch.3 came from the fallback");
        // ch.3 is a primary 404 hole, so it's recorded (so upgrades skip it)...
        assert_eq!(store.gaps(id).unwrap().into_iter().collect::<Vec<_>>(), vec![3]);
        // ...but it's filled from the fallback, so it isn't a *missing* chapter.
        assert!(report.gaps.is_empty(), "filled, so nothing is surfaced as missing");
        assert!(store.unfilled_gaps(id).unwrap().is_empty());
        assert_eq!(report.new_state, DerivedState::Live);
    }

    #[tokio::test]
    async fn primary_hole_filled_from_fallback_is_not_re_upgraded() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();
        // Primary is a 404 hole at ch.2; the fallback has it.
        let backfill = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 3]).with_holes(&[2])),
                Box::new(MockSource::new("fallback", &[1, 2, 3])),
            ],
        );
        let r1 = sync_novel(&store, id, DerivedState::Backfilling, &backfill, 0, |_| {
            ControlFlow::Continue(())
        })
        .await
        .unwrap();
        assert_eq!(r1.from_fallback, 1, "ch.2 filled from fallback");
        assert_eq!(store.gaps(id).unwrap().into_iter().collect::<Vec<_>>(), vec![2]);
        assert_eq!(r1.upgraded, 0);

        // The next sync must NOT keep trying to upgrade ch.2 to the primary — that
        // permanent 404 was the "upgrade of ch.N failed" spam.
        let again = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 3]).with_holes(&[2])),
                Box::new(MockSource::new("fallback", &[1, 2, 3])),
            ],
        );
        let r2 = sync_novel(&store, id, DerivedState::Live, &again, 0, |_| {
            ControlFlow::Continue(())
        })
        .await
        .unwrap();
        assert_eq!(r2.upgraded, 0);
        assert!(
            !r2.warnings.iter().any(|w| w.contains("upgrade")),
            "no futile upgrade attempt: {:?}",
            r2.warnings
        );
    }

    #[tokio::test]
    async fn gap_clears_when_chapter_reappears() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        // First pass: ch.3 is a hole -> recorded, novel still goes Live.
        let s1 = pair(
            &store,
            id,
            vec![Box::new(MockSource::new("primary", &[1, 2, 4, 5]).with_holes(&[3]))],
        );
        let r1 = sync_novel(&store, id, DerivedState::Backfilling, &s1, 0, |_| {
            ControlFlow::Continue(())
        })
        .await
        .unwrap();
        assert_eq!(r1.gaps, vec![3]);
        assert_eq!(store.gaps(id).unwrap().len(), 1);

        // The site restores ch.3; a full re-walk re-probes and fills it, clearing
        // the gap (this is also the path a newly-added fallback source takes).
        let s2 = pair(&store, id, vec![Box::new(MockSource::new("primary", &[1, 2, 3, 4, 5]))]);
        let r2 = sync_novel(&store, id, DerivedState::Backfilling, &s2, 0, |_| {
            ControlFlow::Continue(())
        })
        .await
        .unwrap();
        assert_eq!(r2.newly_fetched, 1, "ch.3 now fetched");
        assert!(r2.gaps.is_empty(), "gap cleared once the chapter reappears");
        assert!(store.gaps(id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn backfill_completes_then_transitions_to_live() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &[1, 2, 3]))]);

        let report = sync_novel(&store, id, DerivedState::Backfilling, &sources, 0, |_| ControlFlow::Continue(()))
            .await
            .unwrap();
        assert_eq!(report.newly_fetched, 3);
        assert_eq!(report.new_state, DerivedState::Live, "caught up => Live");
        assert!(!report.delta_mode, "backfill uses a full walk");
    }

    #[tokio::test]
    async fn partial_backfill_stays_backfilling() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &[1, 2, 3, 4, 5]))]);

        let report = sync_novel(&store, id, DerivedState::Backfilling, &sources, 2, |_| ControlFlow::Continue(()))
            .await
            .unwrap();
        assert_eq!(report.newly_fetched, 2);
        assert_eq!(report.new_state, DerivedState::Backfilling, "still missing chapters");
    }

    #[tokio::test]
    async fn live_delta_fetches_only_the_new_tail() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        // Backfill 1-3.
        let backfill = pair(&store, id, vec![Box::new(MockSource::new("primary", &[1, 2, 3]))]);
        sync_novel(&store, id, DerivedState::Backfilling, &backfill, 0, |_| ControlFlow::Continue(())).await.unwrap();

        // Now Live; the source has 1-5 but its landing page only lists the last 3.
        let live = pair(
            &store,
            id,
            vec![Box::new(MockSource::new("primary", &[1, 2, 3, 4, 5]).with_latest_window(3))],
        );
        let report = sync_novel(&store, id, DerivedState::Live, &live, 0, |_| ControlFlow::Continue(())).await.unwrap();

        assert!(report.delta_mode, "Live novel uses delta discovery");
        assert_eq!(report.newly_fetched, 2, "only ch.4 and ch.5 are new");
        assert_eq!(report.new_state, DerivedState::Live);
    }

    #[tokio::test]
    async fn live_delta_falls_back_on_a_gap() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        let backfill = pair(&store, id, vec![Box::new(MockSource::new("primary", &[1, 2, 3]))]);
        sync_novel(&store, id, DerivedState::Backfilling, &backfill, 0, |_| ControlFlow::Continue(())).await.unwrap();

        // Source jumped to 1-6, but the landing page only shows the single latest
        // chapter (6) — a gap over 4 and 5 the delta check can't see directly.
        let live = pair(
            &store,
            id,
            vec![Box::new(MockSource::new("primary", &[1, 2, 3, 4, 5, 6]).with_latest_window(1))],
        );
        let report = sync_novel(&store, id, DerivedState::Live, &live, 0, |_| ControlFlow::Continue(())).await.unwrap();

        assert!(!report.delta_mode, "gap forced a full-walk fallback");
        assert!(report.warnings.iter().any(|w| w.contains("falling back to a full walk")));
        assert_eq!(report.newly_fetched, 3, "ch.4, 5, 6 all filled");
    }

    #[tokio::test]
    async fn primary_upgrades_fallback_sourced_chapters_when_it_catches_up() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();

        // Backfill: primary has 1-3, fallback is ahead with 1-5. ch.4/5 come from
        // the fallback.
        let backfill = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 2, 3])),
                Box::new(MockSource::new("fallback", &[1, 2, 3, 4, 5])),
            ],
        );
        let r1 = sync_novel(&store, id, DerivedState::Backfilling, &backfill, 0, |_| ControlFlow::Continue(()))
            .await
            .unwrap();
        assert_eq!(r1.from_fallback, 2);
        let ch4 = store.load_chapters(id).unwrap().into_iter().find(|c| c.number == 4).unwrap();
        assert_eq!(ch4.paragraphs, vec!["fallback body 4"]);

        // Primary catches up to 1-5. Next sync should upgrade ch.4/5 to the
        // primary's content.
        let caught_up = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 2, 3, 4, 5])),
                Box::new(MockSource::new("fallback", &[1, 2, 3, 4, 5])),
            ],
        );
        let r2 = sync_novel(&store, id, DerivedState::Live, &caught_up, 0, |_| ControlFlow::Continue(()))
            .await
            .unwrap();
        assert_eq!(r2.newly_fetched, 0, "nothing new");
        assert_eq!(r2.upgraded, 2, "ch.4 and ch.5 upgraded to primary");

        let ch4 = store.load_chapters(id).unwrap().into_iter().find(|c| c.number == 4).unwrap();
        assert_eq!(ch4.paragraphs, vec!["primary body 4"]);

        // A third sync has nothing left to upgrade.
        let again = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 2, 3, 4, 5])),
                Box::new(MockSource::new("fallback", &[1, 2, 3, 4, 5])),
            ],
        );
        let r3 = sync_novel(&store, id, DerivedState::Live, &again, 0, |_| ControlFlow::Continue(()))
            .await
            .unwrap();
        assert_eq!(r3.upgraded, 0);
    }

    #[tokio::test]
    async fn second_sync_is_a_noop_resume() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &[1, 2, 3]))]);
        sync_novel(&store, id, DerivedState::Backfilling, &sources, 0, |_| ControlFlow::Continue(())).await.unwrap();

        let again = pair(&store, id, vec![Box::new(MockSource::new("primary", &[1, 2, 3]))]);
        let report = sync_novel(&store, id, DerivedState::Live, &again, 0, |_| ControlFlow::Continue(())).await.unwrap();
        assert_eq!(report.newly_fetched, 0);
    }

    #[tokio::test]
    async fn broken_fallback_does_not_sink_the_primary() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();
        let sources = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 2, 3])),
                Box::new(MockSource::broken("fallback")),
            ],
        );

        let report = sync_novel(&store, id, DerivedState::Backfilling, &sources, 0, |_| ControlFlow::Continue(()))
            .await
            .unwrap();
        assert_eq!(report.newly_fetched, 3, "primary still fully synced");
        assert!(report.warnings.iter().any(|w| w.contains("discovery failed")));
    }
}
