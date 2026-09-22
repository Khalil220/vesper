
use std::time::Duration;

use anyhow::Result;

use crate::chikari::{self, ChikariSource};
use crate::fetch::{is_not_found, Fetcher, ReqwestFetcher};
use crate::lightnovelworld;
use crate::store::Store;

pub const MIGRATION_KEY: &str = "migration.lightnovelworld_to_chikari";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    Moved {
        novel_id: i64,
        title: String,
        from: String,
        to: String,
        via_title_search: bool,
    },
    NotOnChikari {
        novel_id: i64,
        title: String,
        promoted: Option<String>,
    },
    Undetermined {
        novel_id: i64,
        title: String,
        reason: String,
    },
}

#[derive(Debug, Default)]
pub struct MigrationReport {
    pub outcomes: Vec<MigrationOutcome>,
    pub complete: bool,
}

impl MigrationReport {
    pub fn moved(&self) -> impl Iterator<Item = &MigrationOutcome> {
        self.outcomes
            .iter()
            .filter(|o| matches!(o, MigrationOutcome::Moved { .. }))
    }

    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }
}

pub async fn migrate_lightnovelworld(store: &Store, delay: Duration) -> Result<Option<MigrationReport>> {
    if store.meta_get(MIGRATION_KEY)?.is_some() {
        return Ok(None);
    }
    let chikari = ChikariSource::new(ReqwestFetcher::new(delay)?);
    let report = migrate_with(store, &chikari).await?;
    if report.complete {
        store.meta_set(MIGRATION_KEY, "done")?;
    }
    Ok(Some(report))
}

pub async fn migrate_with<F: Fetcher>(
    store: &Store,
    chikari: &ChikariSource<F>,
) -> Result<MigrationReport> {
    let stale: Vec<(i64, String, crate::store::StoredSource)> = store
        .all_sources()?
        .into_iter()
        .filter(|(_, _, s)| lightnovelworld::is_lightnovelworld_url(&s.url))
        .collect();

    let mut report = MigrationReport {
        outcomes: Vec::new(),
        complete: true,
    };

    for (novel_id, title, source) in stale {
        let outcome = migrate_one(store, chikari, novel_id, &title, &source).await;
        if matches!(outcome, MigrationOutcome::Undetermined { .. }) {
            report.complete = false;
        }
        report.outcomes.push(outcome);
    }
    Ok(report)
}

async fn migrate_one<F: Fetcher>(
    store: &Store,
    chikari: &ChikariSource<F>,
    novel_id: i64,
    title: &str,
    source: &crate::store::StoredSource,
) -> MigrationOutcome {
    let Some(slug) = lightnovelworld::slug_from_url(&source.url) else {
        return MigrationOutcome::Undetermined {
            novel_id,
            title: title.to_string(),
            reason: format!("no novel slug in {}", source.url),
        };
    };

    match resolve_on_chikari(chikari, &slug, title).await {
        Ok(Some((new_slug, via_title_search))) => {
            let to = chikari::novel_url(&new_slug);
            match store.repoint_source(source.id, "chikari", &to) {
                Ok(()) => MigrationOutcome::Moved {
                    novel_id,
                    title: title.to_string(),
                    from: source.url.clone(),
                    to,
                    via_title_search,
                },
                Err(e) => MigrationOutcome::Undetermined {
                    novel_id,
                    title: title.to_string(),
                    reason: e.to_string(),
                },
            }
        }
        Ok(None) => MigrationOutcome::NotOnChikari {
            novel_id,
            title: title.to_string(),
            promoted: promote_surviving_source(store, novel_id, source.id).unwrap_or(None),
        },
        Err(e) => MigrationOutcome::Undetermined {
            novel_id,
            title: title.to_string(),
            reason: e.to_string(),
        },
    }
}

fn promote_surviving_source(
    store: &Store,
    novel_id: i64,
    dead_source_id: i64,
) -> Result<Option<String>> {
    let Some(novel) = store.find_novel(&novel_id.to_string())? else {
        return Ok(None);
    };
    if novel.primary_source().map(|s| s.id) != Some(dead_source_id) {
        return Ok(None);
    }
    let candidate = novel
        .sources
        .iter()
        .filter(|s| s.id != dead_source_id && !lightnovelworld::is_lightnovelworld_url(&s.url))
        .min_by_key(|s| s.priority);
    let Some(alt) = candidate else {
        return Ok(None);
    };
    store.promote_source(novel_id, alt.id)?;
    Ok(Some(alt.name.clone()))
}

pub async fn resolve_on_chikari<F: Fetcher>(
    chikari: &ChikariSource<F>,
    slug: &str,
    title: &str,
) -> Result<Option<(String, bool)>> {
    use crate::source::Source;

    match chikari.fetch_novel(&chikari::novel_url(slug)).await {
        Ok(meta) => {
            if crate::util::normalize_title(&meta.title) == crate::util::normalize_title(title) {
                return Ok(Some((slug.to_string(), false)));
            }
        }
        Err(e) if !is_not_found(&e) => return Err(e),
        Err(_) => {}
    }

    Ok(chikari
        .find_slug_by_title(title)
        .await?
        .map(|found| (found, true)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NovelMeta, NovelStatus};
    use anyhow::anyhow;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct CannedFetcher {
        responses: HashMap<String, String>,
        unreachable: Vec<String>,
        requested: Mutex<Vec<String>>,
    }

    impl CannedFetcher {
        fn new() -> Self {
            Self {
                responses: HashMap::new(),
                unreachable: Vec::new(),
                requested: Mutex::new(Vec::new()),
            }
        }

        fn with(mut self, url: &str, body: &str) -> Self {
            self.responses.insert(url.to_string(), body.to_string());
            self
        }

        fn with_novel(self, slug: &str, title: &str) -> Self {
            let body = format!(
                r#"{{"slug":"{slug}","title":"{title}","status":"releasing","authors":[]}}"#
            );
            self.with(&format!("https://chikari.moe/api/novels/{slug}"), &body)
        }

        fn down(mut self, url: &str) -> Self {
            self.unreachable.push(url.to_string());
            self
        }
    }

    #[async_trait]
    impl Fetcher for CannedFetcher {
        async fn get(&self, url: &str) -> Result<String> {
            self.requested.lock().unwrap().push(url.to_string());
            if self.unreachable.iter().any(|u| url.starts_with(u.as_str())) {
                return Err(anyhow!("connection timed out"));
            }
            match self.responses.get(url) {
                Some(body) => Ok(body.clone()),
                None => Err(anyhow!(crate::fetch::NotFound {
                    url: url.to_string(),
                    status: 404,
                })),
            }
        }
    }

    fn source_name_for(url: &str) -> String {
        ::url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .and_then(|h| {
                h.trim_start_matches("www.")
                    .split('.')
                    .next()
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "other".into())
    }

    fn store_with(subs: &[(&str, &str)]) -> Store {
        let store = Store::open_in_memory().unwrap();
        for (title, url) in subs {
            let meta = NovelMeta {
                title: (*title).to_string(),
                author: Some("A".into()),
                cover_url: None,
                genre: None,
                status_hint: NovelStatus::Ongoing,
                source_url: (*url).to_string(),
            };
            store.subscribe(&meta, &source_name_for(url)).unwrap();
        }
        store
    }

    fn primary_id(store: &Store, novel_id: i64) -> i64 {
        store
            .find_novel(&novel_id.to_string())
            .unwrap()
            .unwrap()
            .primary_source()
            .unwrap()
            .id
    }

    fn source_url(store: &Store, novel_id: i64) -> String {
        store
            .find_novel(&novel_id.to_string())
            .unwrap()
            .unwrap()
            .primary_source()
            .unwrap()
            .url
            .clone()
    }

    #[tokio::test]
    async fn repoints_a_subscription_whose_slug_carried_over() {
        let store = store_with(&[("Shadow Slave", "https://lightnovelworld.org/novel/shadow-slave/")]);
        let fetcher = CannedFetcher::new().with_novel("shadow-slave", "Shadow Slave");
        let chikari = ChikariSource::new(fetcher);

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(report.complete);
        assert_eq!(report.moved().count(), 1);
        assert_eq!(source_url(&store, 1), "https://chikari.moe/novels/shadow-slave");
    }

    #[tokio::test]
    async fn stored_chapters_keep_their_source_and_are_not_orphaned() {
        let store = store_with(&[("Shadow Slave", "https://lightnovelworld.org/novel/shadow-slave/")]);
        let source_id = store
            .find_novel("1")
            .unwrap()
            .unwrap()
            .primary_source()
            .unwrap()
            .id;
        for n in 1..=3 {
            store
                .insert_chapter_if_absent(
                    1,
                    source_id,
                    &crate::model::Chapter {
                        number: n,
                        title: format!("Ch {n}"),
                        paragraphs: vec!["prose".into()],
                    },
                )
                .unwrap();
        }

        let chikari = ChikariSource::new(CannedFetcher::new().with_novel("shadow-slave", "Shadow Slave"));
        migrate_with(&store, &chikari).await.unwrap();

        let novel = store.find_novel("1").unwrap().unwrap();
        let primary = novel.primary_source().unwrap();
        assert_eq!(primary.id, source_id, "same row, so chapters stay attributed");
        assert_eq!(primary.name, "chikari");
        assert_eq!(novel.chapter_count, 3, "chapters survived untouched");
        assert!(store.chapters_from_other_sources(1, source_id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn recovers_a_changed_slug_by_title_search() {
        let store = store_with(&[("Reverend Insanity", "https://lightnovelworld.org/novel/reverend-insanity-old/")]);
        let fetcher = CannedFetcher::new()
            .with_novel("reverend-insanity", "Reverend Insanity")
            .with(
                "https://chikari.moe/api/novels/search?q=Reverend+Insanity&limit=20",
                r#"[{"slug":"reverend-insanity","title":"Reverend Insanity"}]"#,
            );
        let chikari = ChikariSource::new(fetcher);

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(report.complete);
        assert!(matches!(
            report.outcomes[0],
            MigrationOutcome::Moved { via_title_search: true, .. }
        ));
        assert_eq!(source_url(&store, 1), "https://chikari.moe/novels/reverend-insanity");
    }

    #[tokio::test]
    async fn leaves_a_novel_chikari_lacks_alone() {
        let store = store_with(&[("Obscure Web Serial", "https://lightnovelworld.org/novel/obscure-web-serial/")]);
        let fetcher = CannedFetcher::new().with(
            "https://chikari.moe/api/novels/search?q=Obscure+Web+Serial&limit=20",
            "[]",
        );
        let chikari = ChikariSource::new(fetcher);

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(matches!(report.outcomes[0], MigrationOutcome::NotOnChikari { .. }));
        assert!(report.complete, "a definite 'not there' still settles the question");
        assert_eq!(
            source_url(&store, 1),
            "https://lightnovelworld.org/novel/obscure-web-serial/",
            "left working on the old site"
        );
    }

    #[tokio::test]
    async fn a_novel_left_behind_promotes_its_surviving_source() {
        let store = store_with(&[("Obscure Serial", "https://lightnovelworld.org/novel/obscure-serial/")]);
        let dead = primary_id(&store, 1);
        store
            .add_source(1, "freewebnovel", "https://freewebnovel.com/novel/obscure-serial")
            .unwrap();
        for n in 1..=3 {
            store
                .insert_chapter_if_absent(1, dead, &crate::model::Chapter {
                    number: n,
                    title: format!("Ch {n}"),
                    paragraphs: vec!["prose".into()],
                })
                .unwrap();
        }
        let chikari = ChikariSource::new(CannedFetcher::new().with(
            "https://chikari.moe/api/novels/search?q=Obscure+Serial&limit=20",
            "[]",
        ));

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(matches!(
            report.outcomes[0],
            MigrationOutcome::NotOnChikari { promoted: Some(ref s), .. } if s == "freewebnovel"
        ));

        let novel = store.find_novel("1").unwrap().unwrap();
        let primary = novel.primary_source().unwrap();
        assert_eq!(primary.name, "freewebnovel");
        assert_eq!(novel.chapter_count, 3, "chapters kept");
        assert!(
            store.chapters_from_other_sources(1, primary.id).unwrap().is_empty(),
            "and not queued for a pointless re-download"
        );
        assert_eq!(novel.sources.len(), 2);
        assert!(novel.sources.iter().any(|s| s.priority == 2 && s.url.contains("lightnovelworld")));
    }

    #[tokio::test]
    async fn a_novel_left_behind_with_no_other_source_is_untouched() {
        let store = store_with(&[("Only Here", "https://lightnovelworld.org/novel/only-here/")]);
        let chikari = ChikariSource::new(
            CannedFetcher::new()
                .with("https://chikari.moe/api/novels/search?q=Only+Here&limit=20", "[]"),
        );

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(matches!(
            report.outcomes[0],
            MigrationOutcome::NotOnChikari { promoted: None, .. }
        ));
        assert_eq!(source_url(&store, 1), "https://lightnovelworld.org/novel/only-here/");
    }

    #[tokio::test]
    async fn a_dead_fallback_does_not_disturb_a_working_primary() {
        let store = store_with(&[("Kept", "https://www.royalroad.com/fiction/1/kept")]);
        store
            .add_source(1, "lightnovelworld", "https://lightnovelworld.org/novel/kept/")
            .unwrap();
        let chikari = ChikariSource::new(
            CannedFetcher::new().with("https://chikari.moe/api/novels/search?q=Kept&limit=20", "[]"),
        );

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(matches!(
            report.outcomes[0],
            MigrationOutcome::NotOnChikari { promoted: None, .. }
        ));
        let novel = store.find_novel("1").unwrap().unwrap();
        assert_eq!(novel.primary_source().unwrap().name, "royalroad");
    }

    #[tokio::test]
    async fn a_slug_collision_does_not_retarget_the_subscription() {
        let store = store_with(&[("The Innkeeper", "https://lightnovelworld.org/novel/the-innkeeper/")]);
        let fetcher = CannedFetcher::new()
            .with_novel("the-innkeeper", "The Innkeeper's Daughter")
            .with(
                "https://chikari.moe/api/novels/search?q=The+Innkeeper&limit=20",
                "[]",
            );
        let chikari = ChikariSource::new(fetcher);

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(matches!(report.outcomes[0], MigrationOutcome::NotOnChikari { .. }));
        assert_eq!(
            source_url(&store, 1),
            "https://lightnovelworld.org/novel/the-innkeeper/",
            "not silently bound to a different novel"
        );
    }

    #[tokio::test]
    async fn an_unreachable_site_defers_instead_of_deciding() {
        let store = store_with(&[("Shadow Slave", "https://lightnovelworld.org/novel/shadow-slave/")]);
        let chikari = ChikariSource::new(CannedFetcher::new().down("https://chikari.moe/"));

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(matches!(report.outcomes[0], MigrationOutcome::Undetermined { .. }));
        assert!(!report.complete, "so the marker isn't set and it runs again");
        assert_eq!(source_url(&store, 1), "https://lightnovelworld.org/novel/shadow-slave/");
    }

    #[tokio::test]
    async fn other_sites_are_untouched_and_cost_no_requests() {
        let store = store_with(&[
            ("A ScribbleHub Novel", "https://www.scribblehub.com/series/1/a-scribblehub-novel/"),
            ("A Royal Road Novel", "https://royalroad.com/fiction/1/x"),
        ]);
        let fetcher = CannedFetcher::new();
        let chikari = ChikariSource::new(fetcher);

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(report.is_empty());
        assert!(report.complete);
        assert_eq!(
            source_url(&store, 1),
            "https://www.scribblehub.com/series/1/a-scribblehub-novel/"
        );
    }

    #[tokio::test]
    async fn a_lightnovelworld_fallback_moves_and_keeps_its_priority() {
        let store = store_with(&[("Shadow Slave", "https://freewebnovel.com/novel/shadow-slave")]);
        store
            .add_source(1, "lightnovelworld", "https://lightnovelworld.org/novel/shadow-slave/")
            .unwrap();
        let chikari = ChikariSource::new(CannedFetcher::new().with_novel("shadow-slave", "Shadow Slave"));

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert_eq!(report.moved().count(), 1);

        let novel = store.find_novel("1").unwrap().unwrap();
        assert_eq!(novel.primary_source().unwrap().url, "https://freewebnovel.com/novel/shadow-slave");
        let fallback = novel.sources.iter().find(|s| s.priority == 2).unwrap();
        assert_eq!(fallback.url, "https://chikari.moe/novels/shadow-slave");
        assert_eq!(fallback.name, "chikari");
    }

    #[tokio::test]
    async fn an_existing_chikari_source_is_reported_not_clobbered() {
        let store = store_with(&[("Shadow Slave", "https://lightnovelworld.org/novel/shadow-slave/")]);
        store
            .add_source(1, "chikari", "https://chikari.moe/novels/shadow-slave")
            .unwrap();
        let chikari = ChikariSource::new(CannedFetcher::new().with_novel("shadow-slave", "Shadow Slave"));

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(matches!(report.outcomes[0], MigrationOutcome::Undetermined { .. }));
        let novel = store.find_novel("1").unwrap().unwrap();
        assert_eq!(novel.sources.len(), 2, "both rows survive");
    }

    #[tokio::test]
    async fn the_marker_stops_it_running_twice() {
        let store = store_with(&[("Shadow Slave", "https://lightnovelworld.org/novel/shadow-slave/")]);
        assert!(store.meta_get(MIGRATION_KEY).unwrap().is_none());

        let chikari = ChikariSource::new(CannedFetcher::new().with_novel("shadow-slave", "Shadow Slave"));
        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(report.complete);
        store.meta_set(MIGRATION_KEY, "done").unwrap();

        assert!(migrate_lightnovelworld(&store, Duration::ZERO).await.unwrap().is_none());
    }
}
