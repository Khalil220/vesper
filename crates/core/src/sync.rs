//! Syncing a novel from its ranked sources, with active fallback.
//!
//! Each source's table of contents is discovered; the union of chapter numbers
//! is the target. For every not-yet-stored number, chapters are fetched from the
//! **highest-priority source that has it** — the primary first, falling back to
//! secondaries only for numbers the primary lacks (or when the primary's fetch
//! fails). The primary stays authoritative for content. See DESIGN.md.
//!
//! This is the shared engine for both the manual `fetch` command and the future
//! scheduled `sync`.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::model::ChapterRef;
use crate::source::Source;
use crate::store::{Store, StoredSource};

/// Outcome of a sync pass.
#[derive(Debug, Default)]
pub struct SyncReport {
    /// Chapters newly stored this pass.
    pub newly_fetched: u32,
    /// Of those, how many came from a fallback (non-primary) source.
    pub from_fallback: u32,
    /// Non-fatal notices (source divergence, per-chapter fetch failures).
    pub warnings: Vec<String>,
    /// Chapter numbers no source could provide.
    pub failures: Vec<u32>,
}

/// Sync `novel_id` from `sources` (given in priority order, primary first).
///
/// `limit` caps the number of chapters fetched this pass (0 = all missing).
/// `on_progress` receives a line per chapter attempt.
pub async fn sync_novel(
    store: &Store,
    novel_id: i64,
    sources: &[(StoredSource, Box<dyn Source>)],
    limit: usize,
    mut on_progress: impl FnMut(String),
) -> Result<SyncReport> {
    let mut report = SyncReport::default();

    // Discover each source's chapter map (number -> ref). Discovery is
    // best-effort per source: if one fails (a dead fallback, say), warn and skip
    // it rather than aborting the whole sync and losing the other sources.
    let mut discovered: Vec<(&StoredSource, &dyn Source, BTreeMap<u32, ChapterRef>)> = Vec::new();
    for (meta, src) in sources {
        match src.discover_chapters(&meta.url, None).await {
            Ok(refs) => {
                let map = refs.into_iter().map(|r| (r.number, r)).collect();
                discovered.push((meta, src.as_ref(), map));
            }
            Err(e) => report
                .warnings
                .push(format!("discovery failed for {}: {e}; skipping this source", meta.name)),
        }
    }

    // Warn if sources disagree on how far the novel goes — a sign that their
    // chapter numbering may not align 1:1.
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

    // Target = union of all discovered numbers; fetch those we don't yet have.
    let mut target: BTreeSet<u32> = BTreeSet::new();
    for (_, _, map) in &discovered {
        target.extend(map.keys().copied());
    }
    let have = store.stored_chapter_numbers(novel_id)?;
    let missing: Vec<u32> = target.difference(&have).copied().collect();
    let to_fetch: Vec<u32> = if limit == 0 {
        missing
    } else {
        missing.into_iter().take(limit).collect()
    };

    for num in to_fetch {
        let mut done = false;
        for (idx, (meta, src, map)) in discovered.iter().enumerate() {
            let Some(cref) = map.get(&num) else { continue };
            on_progress(format!("ch.{num} <- {}", meta.name));
            match src.fetch_chapter(cref).await {
                Ok(chapter) => {
                    if store.insert_chapter_if_absent(novel_id, meta.id, &chapter)? {
                        report.newly_fetched += 1;
                        if idx > 0 {
                            report.from_fallback += 1;
                        }
                    }
                    done = true;
                    break;
                }
                Err(e) => {
                    report
                        .warnings
                        .push(format!("ch.{num} from {} failed: {e}; trying next source", meta.name));
                }
            }
        }
        if !done {
            report.failures.push(num);
        }
    }

    // Record each source's latest discovered chapter.
    for (meta, _, map) in &discovered {
        if let Some(max) = map.keys().next_back() {
            store.update_source_progress(meta.id, *max)?;
        }
    }

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
        /// When true, discovery returns an error (simulates a dead source).
        broken: bool,
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
            }
        }

        fn broken(name: &str) -> Self {
            Self {
                name: name.into(),
                bodies: BTreeMap::new(),
                broken: true,
            }
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
        async fn fetch_chapter(&self, chapter: &ChapterRef) -> Result<Chapter> {
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

    fn subscribe_with_two_sources(
        store: &Store,
        primary: &[u32],
        fallback: &[u32],
    ) -> Vec<(StoredSource, Box<dyn Source>)> {
        let meta = NovelMeta {
            title: "Mock Novel".into(),
            author: Some("A".into()),
            cover_url: None,
            status_hint: NovelStatus::Ongoing,
            source_url: "https://primary.example/n".into(),
        };
        let id = store.subscribe(&meta, "primary").unwrap();
        store
            .add_source(id, "fallback", "https://fallback.example/n")
            .unwrap();

        let novel = store.find_novel(&id.to_string()).unwrap().unwrap();
        let mut built: Vec<(StoredSource, Box<dyn Source>)> = Vec::new();
        for s in novel.sources {
            let src: Box<dyn Source> = if s.priority == 1 {
                Box::new(MockSource::new("primary", primary))
            } else {
                Box::new(MockSource::new("fallback", fallback))
            };
            built.push((s, src));
        }
        built
    }

    #[tokio::test]
    async fn fallback_fills_chapters_the_primary_lacks() {
        let store = Store::open_in_memory().unwrap();
        // Primary has 1-3; fallback is ahead with 1-5.
        let sources = subscribe_with_two_sources(&store, &[1, 2, 3], &[1, 2, 3, 4, 5]);
        let novel_id = store.find_novel("Mock Novel").unwrap().unwrap().id;

        let report = sync_novel(&store, novel_id, &sources, 0, |_| {}).await.unwrap();

        assert_eq!(report.newly_fetched, 5);
        assert_eq!(report.from_fallback, 2, "ch.4 and ch.5 come from the fallback");
        assert!(report.failures.is_empty());
        // Divergence warning fired (3 vs 5).
        assert!(report.warnings.iter().any(|w| w.contains("different latest")));

        // 1-3 came from the primary (authoritative); 4-5 from fallback.
        let ch4 = store
            .load_chapters(novel_id)
            .unwrap()
            .into_iter()
            .find(|c| c.number == 4)
            .unwrap();
        assert_eq!(ch4.paragraphs, vec!["fallback body 4"]);
        let ch1 = store
            .load_chapters(novel_id)
            .unwrap()
            .into_iter()
            .find(|c| c.number == 1)
            .unwrap();
        assert_eq!(ch1.paragraphs, vec!["primary body 1"]);
    }

    #[tokio::test]
    async fn second_sync_is_a_noop_resume() {
        let store = Store::open_in_memory().unwrap();
        let sources = subscribe_with_two_sources(&store, &[1, 2, 3], &[1, 2, 3, 4, 5]);
        let novel_id = store.find_novel("Mock Novel").unwrap().unwrap().id;

        sync_novel(&store, novel_id, &sources, 0, |_| {}).await.unwrap();
        let again = sync_novel(&store, novel_id, &sources, 0, |_| {}).await.unwrap();
        assert_eq!(again.newly_fetched, 0);
        assert_eq!(again.from_fallback, 0);
    }

    #[tokio::test]
    async fn broken_fallback_does_not_sink_the_primary() {
        let store = Store::open_in_memory().unwrap();
        let meta = NovelMeta {
            title: "Mock Novel".into(),
            author: Some("A".into()),
            cover_url: None,
            status_hint: NovelStatus::Ongoing,
            source_url: "https://primary.example/n".into(),
        };
        let id = store.subscribe(&meta, "primary").unwrap();
        store
            .add_source(id, "fallback", "https://fallback.example/n")
            .unwrap();
        let novel = store.find_novel(&id.to_string()).unwrap().unwrap();

        let mut sources: Vec<(StoredSource, Box<dyn Source>)> = Vec::new();
        for s in novel.sources {
            let src: Box<dyn Source> = if s.priority == 1 {
                Box::new(MockSource::new("primary", &[1, 2, 3]))
            } else {
                Box::new(MockSource::broken("fallback"))
            };
            sources.push((s, src));
        }

        let report = sync_novel(&store, id, &sources, 0, |_| {}).await.unwrap();
        assert_eq!(report.newly_fetched, 3, "primary still fully synced");
        assert!(report.warnings.iter().any(|w| w.contains("discovery failed")));
    }

    #[tokio::test]
    async fn limit_caps_chapters_per_pass() {
        let store = Store::open_in_memory().unwrap();
        let sources = subscribe_with_two_sources(&store, &[1, 2, 3, 4, 5], &[]);
        let novel_id = store.find_novel("Mock Novel").unwrap().unwrap().id;

        let report = sync_novel(&store, novel_id, &sources, 2, |_| {}).await.unwrap();
        assert_eq!(report.newly_fetched, 2);
        assert_eq!(store.stored_chapter_numbers(novel_id).unwrap().len(), 2);
    }
}
