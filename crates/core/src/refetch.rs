
use std::collections::BTreeSet;

use anyhow::{anyhow, Result};

use crate::model::{Chapter, ChapterRef, DerivedState};
use crate::repair::looks_like_gate_stub;
use crate::source::Source;
use crate::store::{Store, StoredSource};

#[derive(Debug, Default)]
pub struct RefetchReport {
    pub replaced: Vec<u32>,
    pub unchanged: Vec<u32>,
    pub added: Vec<u32>,
    pub skipped: Vec<(u32, String)>,
}

impl RefetchReport {
    pub fn changed(&self) -> bool {
        !self.replaced.is_empty() || !self.added.is_empty()
    }
}

pub async fn refetch_novel(
    store: &Store,
    novel_id: i64,
    sources: &[(StoredSource, Box<dyn Source>)],
    targets: Option<&BTreeSet<u32>>,
    dry_run: bool,
    mut on_progress: impl FnMut(u32, usize, usize),
) -> Result<RefetchReport> {
    let mut report = RefetchReport::default();
    if sources.is_empty() {
        return Err(anyhow!("no usable source to re-download from"));
    }

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

    let recoverable = still_absent.iter().any(|n| listed.contains(n));
    if recoverable && !dry_run {
        store.set_derived_state(novel_id, DerivedState::Backfilling)?;
    }

    Ok(report)
}

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

    struct MockSource {
        name: String,
        bodies: BTreeMap<u32, String>,
        broken: BTreeSet<u32>,
        dead: bool,
        fetches: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl MockSource {
        fn new(name: &str, bodies: &[(u32, &str)]) -> Self {
            Self {
                name: name.into(),
                bodies: bodies.iter().map(|(n, b)| (*n, b.to_string())).collect(),
                broken: BTreeSet::new(),
                dead: false,
                fetches: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }

        fn failing(mut self, numbers: &[u32]) -> Self {
            self.broken = numbers.iter().copied().collect();
            self
        }

        fn unreachable(name: &str) -> Self {
            Self {
                name: name.into(),
                bodies: BTreeMap::new(),
                broken: BTreeSet::new(),
                dead: true,
                fetches: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
            self.fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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

        let r = refetch_novel(&store, id, &sources, Some(&targets), false, |_, _, _| {})
            .await
            .unwrap();

        assert_eq!(r.replaced, vec![153, 154]);
        assert_eq!(r.unchanged, vec![152], "identical text is not rewritten");
        assert_eq!(body_of(&store, id, 153), "real 153");
        assert_eq!(body_of(&store, id, 154), "real 154");
    }

    #[tokio::test]
    async fn a_rewrite_marks_the_novel_for_re_export() {
        let (store, id, _) = setup(&[(1, "old")]);
        store.mark_all_exported(id).unwrap();
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &[(1, "new")]))]);
        let targets: BTreeSet<u32> = [1].into_iter().collect();

        refetch_novel(&store, id, &sources, Some(&targets), false, |_, _, _| {})
            .await
            .unwrap();

        let exported: i64 = store
            .load_chapters(id)
            .map(|c| c.len() as i64)
            .unwrap();
        assert_eq!(exported, 1);
        assert_eq!(store.apply_retention(0).unwrap(), 0, "un-exported, so not purgeable");
    }

    #[tokio::test]
    async fn dry_run_reports_without_writing() {
        let (store, id, _) = setup(&[(1, "old")]);
        let sources = pair(&store, id, vec![Box::new(MockSource::new("primary", &[(1, "new")]))]);

        let r = refetch_novel(&store, id, &sources, None, true, |_, _, _| {})
            .await
            .unwrap();
        assert_eq!(r.replaced, vec![1]);
        assert_eq!(body_of(&store, id, 1), "old", "dry run must not write");
    }

    #[tokio::test]
    async fn a_failed_fetch_leaves_the_stored_chapter_intact() {
        let (store, id, _) = setup(&[(1, "one"), (2, "two")]);
        let sources = pair(
            &store,
            id,
            vec![Box::new(MockSource::new("primary", &[(1, "one"), (2, "two v2")]).failing(&[2]))],
        );

        let r = refetch_novel(&store, id, &sources, None, false, |_, _, _| {})
            .await
            .unwrap();

        assert_eq!(body_of(&store, id, 2), "two", "stored text left intact");
        assert!(r.skipped.iter().any(|(n, _)| *n == 2));
    }

    #[tokio::test]
    async fn refuses_to_run_when_no_source_can_be_read() {
        let (store, id, _) = setup(&[(1, "one")]);
        let sources = pair(&store, id, vec![Box::new(MockSource::unreachable("primary"))]);

        let err = refetch_novel(&store, id, &sources, None, false, |_, _, _| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("could not read the chapter list"), "{err}");
        assert_eq!(body_of(&store, id, 1), "one", "nothing touched");
    }

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

        let r = refetch_novel(&store, id, &sources, None, false, |_, _, _| {})
            .await
            .unwrap();

        assert_eq!(r.replaced, vec![1]);
        assert_eq!(body_of(&store, id, 1), "fuller text", "came from the fallback");
    }

    #[tokio::test]
    async fn an_unfillable_hole_sends_the_novel_back_to_backfilling() {
        let (store, id, _) = setup(&[(1, "one")]);
        let sources = pair(
            &store,
            id,
            vec![Box::new(MockSource::new("primary", &[(1, "one"), (2, "two")]).failing(&[2]))],
        );

        refetch_novel(&store, id, &sources, None, false, |_, _, _| {})
            .await
            .unwrap();

        let novel = store.find_novel(&id.to_string()).unwrap().unwrap();
        assert_eq!(novel.derived_state, DerivedState::Backfilling);
    }

    #[tokio::test]
    async fn a_plain_dry_run_still_compares_text() {
        let (store, id, _) = setup(&[(1, "one")]);
        let mock = MockSource::new("primary", &[(1, "one v2")]);
        let counter = mock.fetches.clone();
        let sources = pair(&store, id, vec![Box::new(mock)]);

        let r = refetch_novel(&store, id, &sources, None, true, |_, _, _| {})
            .await
            .unwrap();
        assert_eq!(r.replaced, vec![1]);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
