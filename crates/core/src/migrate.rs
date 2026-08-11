//! One-shot library migrations.
//!
//! Currently one: lightnovelworld's novel library moved to chikari.moe, so
//! subscriptions pointing at the old host are repointed at the new one on the
//! first launch after the upgrade.
//!
//! ## Why this is safe to do in place
//!
//! chikari inherited lightnovelworld's slugs *and its chapter numbering* — the
//! two sites are the same catalogue. Spot-checking chapters across several
//! novels, `chikari/<slug>` chapter N and `lightnovelworld/<slug>` chapter N
//! are the same text, including the cases where a novel's *displayed* chapter
//! label runs offset from its canonical number (chikari ch.1200 and
//! lightnovelworld ch.1200 of `the-primal-hunter` are both titled "Chapter
//! 1176"). So an already-downloaded ch.1200 stays correct after the move, and
//! the migration is a URL rewrite rather than a re-download.
//!
//! That is why this repoints the existing `sources` row via
//! [`Store::repoint_source`] instead of adding chikari as a new source: the row
//! keeps its id, so every stored chapter stays attributed to it. Adding a new
//! primary would instead make sync's content-upgrade pass re-fetch the novel's
//! entire back catalogue from chikari — thousands of requests per novel, for
//! byte-identical prose.
//!
//! ## What it refuses to do
//!
//! A subscription is only moved once chikari has confirmed the novel exists
//! there, by slug or — if the slug didn't carry over — by an exact normalized
//! title match in its search. Anything else is left pointing at
//! lightnovelworld, which still works, and reported so the user can decide.
//! A novel is never bound to a merely similar title.

use std::time::Duration;

use anyhow::Result;

use crate::chikari::{self, ChikariSource};
use crate::fetch::{is_not_found, Fetcher, ReqwestFetcher};
use crate::lightnovelworld;
use crate::store::Store;

/// `meta` key recording that the lightnovelworld -> chikari move has been done.
pub const MIGRATION_KEY: &str = "migration.lightnovelworld_to_chikari";

/// What happened to one subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationOutcome {
    /// Repointed at chikari.
    Moved {
        novel_id: i64,
        title: String,
        from: String,
        to: String,
        /// The slug changed and was recovered by title search.
        via_title_search: bool,
    },
    /// chikari doesn't have this novel. Left on lightnovelworld.
    NotOnChikari {
        novel_id: i64,
        title: String,
    },
    /// Couldn't tell (network error, or the URL didn't yield a slug). Left
    /// alone, and the migration will be retried on the next launch.
    Undetermined {
        novel_id: i64,
        title: String,
        reason: String,
    },
}

/// Result of a migration pass.
#[derive(Debug, Default)]
pub struct MigrationReport {
    pub outcomes: Vec<MigrationOutcome>,
    /// Every subscription reached a definite verdict, so the pass need not run
    /// again. False when something was [`MigrationOutcome::Undetermined`].
    pub complete: bool,
}

impl MigrationReport {
    pub fn moved(&self) -> impl Iterator<Item = &MigrationOutcome> {
        self.outcomes
            .iter()
            .filter(|o| matches!(o, MigrationOutcome::Moved { .. }))
    }

    /// Nothing to report to the user.
    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }
}

/// Run the lightnovelworld -> chikari migration if it hasn't already been done.
///
/// Cheap when there's nothing to do: with the marker set it is a single
/// `SELECT`, and with no lightnovelworld subscriptions it makes no network
/// request at all. Returns `None` when the migration had already run.
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

/// The migration proper, against a caller-supplied chikari adapter (the seam
/// the tests substitute a canned fetcher through).
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
                // Almost always "you already added that chikari URL yourself".
                // Leave both rows alone rather than guessing which to drop.
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
        },
        Err(e) => MigrationOutcome::Undetermined {
            novel_id,
            title: title.to_string(),
            reason: e.to_string(),
        },
    }
}

/// Find the novel's slug on chikari: the inherited one if it still resolves,
/// otherwise whatever the title search turns up. `Ok(None)` means chikari
/// definitively doesn't have it; `Err` means we couldn't reach chikari to find
/// out, which must not be mistaken for the former.
///
/// The `bool` reports whether the answer came from the title search (i.e. the
/// slug changed). Public so the `live_migration` example can preview a real
/// library's migration without writing to it.
pub async fn resolve_on_chikari<F: Fetcher>(
    chikari: &ChikariSource<F>,
    slug: &str,
    title: &str,
) -> Result<Option<(String, bool)>> {
    use crate::source::Source;

    match chikari.fetch_novel(&chikari::novel_url(slug)).await {
        // The slug carried over. Confirm it's really the same novel: slugs are
        // shared across a merged catalogue, but a collision would otherwise
        // silently retarget the subscription.
        Ok(meta) => {
            if crate::util::normalize_title(&meta.title) == crate::util::normalize_title(title) {
                return Ok(Some((slug.to_string(), false)));
            }
        }
        // A 404 is a definite "not at that slug" — fall through to the search.
        // Anything else (timeout, 5xx, a Cloudflare hiccup) is not an answer.
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

    /// A fetcher serving canned JSON per URL, so the migration's decisions can
    /// be exercised without the network. A URL with no canned response 404s;
    /// URLs listed in `unreachable` fail transiently instead.
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

        /// Register a novel at `slug` with the given title.
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
            store.subscribe(&meta, "lightnovelworld").unwrap();
        }
        store
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

    /// The whole point of repointing in place: the source row keeps its id, so
    /// stored chapters stay attributed to it and none are re-downloaded.
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
        // Nothing is pending a re-fetch from the "new" primary.
        assert!(store.chapters_from_other_sources(1, source_id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn recovers_a_changed_slug_by_title_search() {
        let store = store_with(&[("Reverend Insanity", "https://lightnovelworld.org/novel/reverend-insanity-old/")]);
        let fetcher = CannedFetcher::new()
            // The old slug 404s...
            .with_novel("reverend-insanity", "Reverend Insanity")
            // ...but search finds it under the new one.
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

    /// A novel chikari genuinely doesn't carry stays on lightnovelworld, which
    /// still works — losing the subscription would be worse than not moving it.
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

    /// A slug that resolves to a *different* novel must not be taken at face
    /// value — the title decides, and search gets the final say.
    #[tokio::test]
    async fn a_slug_collision_does_not_retarget_the_subscription() {
        let store = store_with(&[("The Innkeeper", "https://lightnovelworld.org/novel/the-innkeeper/")]);
        let fetcher = CannedFetcher::new()
            // Same slug on chikari, but it's someone else's novel.
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

    /// Offline (or chikari down) must not be mistaken for "not on chikari":
    /// nothing is changed and the marker stays unset so it retries next launch.
    #[tokio::test]
    async fn an_unreachable_site_defers_instead_of_deciding() {
        let store = store_with(&[("Shadow Slave", "https://lightnovelworld.org/novel/shadow-slave/")]);
        let chikari = ChikariSource::new(CannedFetcher::new().down("https://chikari.moe/"));

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(matches!(report.outcomes[0], MigrationOutcome::Undetermined { .. }));
        assert!(!report.complete, "so the marker isn't set and it runs again");
        assert_eq!(source_url(&store, 1), "https://lightnovelworld.org/novel/shadow-slave/");
    }

    /// Sources on other sites are none of this migration's business.
    #[tokio::test]
    async fn other_sites_are_untouched_and_cost_no_requests() {
        let store = store_with(&[
            ("A Novgo Novel", "https://novgo.net/a-novgo-novel.html"),
            ("A Royal Road Novel", "https://royalroad.com/fiction/1/x"),
        ]);
        let fetcher = CannedFetcher::new();
        let chikari = ChikariSource::new(fetcher);

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert!(report.is_empty());
        assert!(report.complete);
        assert_eq!(source_url(&store, 1), "https://novgo.net/a-novgo-novel.html");
    }

    /// A fallback source on lightnovelworld moves too, keeping its priority.
    #[tokio::test]
    async fn a_lightnovelworld_fallback_moves_and_keeps_its_priority() {
        let store = store_with(&[("Shadow Slave", "https://novgo.net/shadow-slave.html")]);
        store
            .add_source(1, "lightnovelworld", "https://lightnovelworld.org/novel/shadow-slave/")
            .unwrap();
        let chikari = ChikariSource::new(CannedFetcher::new().with_novel("shadow-slave", "Shadow Slave"));

        let report = migrate_with(&store, &chikari).await.unwrap();
        assert_eq!(report.moved().count(), 1);

        let novel = store.find_novel("1").unwrap().unwrap();
        assert_eq!(novel.primary_source().unwrap().url, "https://novgo.net/shadow-slave.html");
        let fallback = novel.sources.iter().find(|s| s.priority == 2).unwrap();
        assert_eq!(fallback.url, "https://chikari.moe/novels/shadow-slave");
        assert_eq!(fallback.name, "chikari");
    }

    /// If the user already added the chikari URL by hand, repointing would
    /// collide with the UNIQUE(url) constraint — report it rather than
    /// destroying either row.
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

        // The public entry point now short-circuits without touching the network.
        assert!(migrate_lightnovelworld(&store, Duration::ZERO).await.unwrap().is_none());
    }
}
