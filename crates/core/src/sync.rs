
use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;

use anyhow::Result;

use crate::fetch::is_not_found;
use crate::model::{ChapterRef, DerivedState};
use crate::source::Source;
use crate::store::{Store, StoredSource};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncProgress {
    Fetching { done: usize, total: usize },
    Upgrading { done: usize, total: usize },
}

#[derive(Debug)]
pub struct SyncReport {
    pub newly_fetched: u32,
    pub from_fallback: u32,
    pub upgraded: u32,
    pub new_state: DerivedState,
    pub delta_mode: bool,
    pub warnings: Vec<String>,
    pub failures: Vec<u32>,
    pub gaps: Vec<u32>,
    pub interrupted: bool,
}

type Discovered<'a> = Vec<(&'a StoredSource, &'a dyn Source, BTreeMap<u32, ChapterRef>)>;

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

    let mut discovered = discover(sources, is_backfilling, &mut report.warnings).await;

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
                    if !known_gap {
                        report.warnings.push(format!(
                            "ch.{num} from {} failed: {e}; trying next source",
                            meta.name
                        ));
                    }
                }
            }
        }
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
            report.failures.push(num);
        }
        if on_progress(SyncProgress::Fetching { done: i + 1, total }).is_break() {
            report.interrupted = true;
            break;
        }
    }

    if !report.interrupted {
        if let Some((pmeta, psrc, pmap)) = discovered.iter().find(|(m, _, _)| m.priority == 1) {
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
                    Ok(chapter) if crate::repair::looks_like_gate_stub(&chapter.paragraphs) => {
                        if gaps.insert(num) {
                            store.record_gap(novel_id, num)?;
                        }
                    }
                    Ok(chapter) => {
                        store.update_chapter_content(novel_id, pmeta.id, &chapter)?;
                        report.upgraded += 1;
                    }
                    Err(e) if is_not_found(&e) => {
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

    for (meta, _, map) in &discovered {
        if let Some(max) = map.keys().next_back() {
            store.update_source_progress(meta.id, *max)?;
        }
    }

    let now_have = store.stored_chapter_numbers(novel_id)?;
    let new_state = if is_backfilling {
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
    report.gaps = gaps.iter().copied().filter(|n| !now_have.contains(n)).collect();

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Chapter, NovelMeta, NovelStatus};
    use async_trait::async_trait;

    struct MockSource {
        name: String,
        bodies: BTreeMap<u32, String>,
        broken: bool,
        latest_window: Option<usize>,
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

        fn with_holes(mut self, holes: &[u32]) -> Self {
            self.holes = holes.iter().copied().collect();
            self
        }

        fn with_body(mut self, number: u32, body: &str) -> Self {
            self.bodies.insert(number, body.to_string());
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
        assert_eq!(store.gaps(id).unwrap().into_iter().collect::<Vec<_>>(), vec![3]);
        assert!(report.gaps.is_empty(), "filled, so nothing is surfaced as missing");
        assert!(store.unfilled_gaps(id).unwrap().is_empty());
        assert_eq!(report.new_state, DerivedState::Live);
    }

    #[tokio::test]
    async fn primary_hole_filled_from_fallback_is_not_re_upgraded() {
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();
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
        let backfill = pair(&store, id, vec![Box::new(MockSource::new("primary", &[1, 2, 3]))]);
        sync_novel(&store, id, DerivedState::Backfilling, &backfill, 0, |_| ControlFlow::Continue(())).await.unwrap();

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
    async fn a_gated_primary_does_not_clobber_a_real_fallback_chapter() {
        const STUB: &str = "This chapter requires a free account to read. \
                            Sign up or log in to continue reading.";
        let store = Store::open_in_memory().unwrap();
        let id = subscribe(&store);
        store.add_source(id, "fallback", "https://fallback.example/n").unwrap();

        let backfill = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1])),
                Box::new(MockSource::new("fallback", &[1, 2])),
            ],
        );
        let r1 = sync_novel(&store, id, DerivedState::Backfilling, &backfill, 0, |_| {
            ControlFlow::Continue(())
        })
        .await
        .unwrap();
        assert_eq!(r1.from_fallback, 1, "ch.2 came from the fallback");

        let gated = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 2]).with_body(2, STUB)),
                Box::new(MockSource::new("fallback", &[1, 2])),
            ],
        );
        let r2 = sync_novel(&store, id, DerivedState::Live, &gated, 0, |_| {
            ControlFlow::Continue(())
        })
        .await
        .unwrap();

        assert_eq!(r2.upgraded, 0, "the placeholder is not an upgrade");
        let ch2 = store.load_chapters(id).unwrap().into_iter().find(|c| c.number == 2).unwrap();
        assert_eq!(ch2.paragraphs, vec!["fallback body 2"], "real chapter survived");

        assert!(store.gaps(id).unwrap().contains(&2));
        assert!(store.unfilled_gaps(id).unwrap().is_empty());
        assert!(r2.gaps.is_empty());

        let again = pair(
            &store,
            id,
            vec![
                Box::new(MockSource::new("primary", &[1, 2]).with_body(2, STUB)),
                Box::new(MockSource::new("fallback", &[1, 2])),
            ],
        );
        let r3 = sync_novel(&store, id, DerivedState::Live, &again, 0, |_| {
            ControlFlow::Continue(())
        })
        .await
        .unwrap();
        assert_eq!(r3.upgraded, 0);
        let ch2 = store.load_chapters(id).unwrap().into_iter().find(|c| c.number == 2).unwrap();
        assert_eq!(ch2.paragraphs, vec!["fallback body 2"]);
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
