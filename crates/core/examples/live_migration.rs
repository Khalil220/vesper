//! Live check of the lightnovelworld -> chikari migration against a real
//! library, including the assumption the whole migration rests on: that
//! chikari's chapter *numbering* matches lightnovelworld's, so repointing a
//! subscription in place leaves already-downloaded chapters correctly keyed.
//!
//! Not part of `cargo test` (it hits the network and needs a real DB).
//!
//!   # preview only — reads the DB, writes nothing
//!   cargo run -p vesper-core --example live_migration -- <path-to-library.db>
//!   # actually migrate
//!   cargo run -p vesper-core --example live_migration -- <path-to-library.db> --apply
//!
//! Point it at a *copy* of a library first. The preview samples stored
//! chapters and compares them with the same numbers on chikari, so a numbering
//! mismatch shows up as differing prose before anything is written.

use std::path::PathBuf;
use std::time::Duration;

use vesper_core::chikari::{self, ChikariSource};
use vesper_core::migrate::resolve_on_chikari;
use vesper_core::{lightnovelworld, migrate_lightnovelworld, ReqwestFetcher, Source, Store};

/// Chapters sampled per novel when checking that the numbering lines up.
const SAMPLES: usize = 3;

/// Cap on chapters fetched per novel by the post-migration sync check, so the
/// verification stays a smoke test rather than a full backfill.
const SYNC_LIMIT: usize = 2;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let db: PathBuf = args
        .next()
        .expect("usage: live_migration <path-to-library.db> [--apply]")
        .into();
    let apply = args.any(|a| a == "--apply");

    let store = Store::open(&db)?;
    let chikari = ChikariSource::new(ReqwestFetcher::new(Duration::from_millis(1200))?);

    let stale: Vec<_> = store
        .all_sources()?
        .into_iter()
        .filter(|(_, _, s)| lightnovelworld::is_lightnovelworld_url(&s.url))
        .collect();
    println!("{} lightnovelworld source(s) in {}", stale.len(), db.display());

    let mut mismatches = 0;
    for (novel_id, title, source) in &stale {
        println!("\n#{novel_id} {title}");
        let Some(slug) = lightnovelworld::slug_from_url(&source.url) else {
            println!("  no slug in {}", source.url);
            continue;
        };
        match resolve_on_chikari(&chikari, &slug, title).await {
            Ok(Some((new_slug, via_search))) => {
                println!(
                    "  resolves to {}{}",
                    chikari::novel_url(&new_slug),
                    if via_search { " (by title search)" } else { "" }
                );
                mismatches += verify_numbering(&store, &chikari, *novel_id, &new_slug).await?;
            }
            Ok(None) => println!("  NOT on chikari — would stay on lightnovelworld"),
            Err(e) => println!("  could not check: {e}"),
        }
    }

    println!("\n{} sampled chapter(s) disagreed between the two sites", mismatches);
    if !apply {
        println!("preview only; re-run with --apply to migrate this library");
        return Ok(());
    }
    if mismatches > 0 {
        println!("refusing to migrate: the numbering does not line up");
        return Ok(());
    }

    match migrate_lightnovelworld(&store, Duration::from_millis(1200)).await? {
        None => println!("\nalready migrated (marker set)"),
        Some(report) => {
            println!("\nmigration complete={}", report.complete);
            for outcome in &report.outcomes {
                println!("  {outcome:?}");
            }
            for (id, title, source) in store.all_sources()? {
                println!("  #{id} {title} -> [{}] {}", source.name, source.url);
            }
        }
    }

    // The last mile: actually sync a migrated novel through the normal engine,
    // proving new chapters now arrive from chikari into the existing library.
    println!("\n--- sync check (at most {SYNC_LIMIT} chapters per novel) ---");
    for (novel_id, _, _) in &stale {
        let Some(novel) = store.find_novel(&novel_id.to_string())? else {
            continue;
        };
        if !novel.sources.iter().any(|s| s.url.contains("chikari.moe")) {
            continue;
        }
        let before = store.stored_chapter_numbers(novel.id)?.len();
        let mut sources = Vec::new();
        for s in &novel.sources {
            if let Some(adapter) = vesper_core::build_source(&s.url, Duration::from_millis(1200)) {
                sources.push((s.clone(), adapter));
            }
        }
        let report = vesper_core::sync_novel(
            &store,
            novel.id,
            novel.derived_state,
            &sources,
            SYNC_LIMIT,
            |_| std::ops::ControlFlow::Continue(()),
        )
        .await?;
        let after = store.stored_chapter_numbers(novel.id)?.len();
        println!(
            "  #{} {}: {} -> {} chapters (+{} new, {} warning(s), state {})",
            novel.id,
            novel.title,
            before,
            after,
            report.newly_fetched,
            report.warnings.len(),
            report.new_state.as_str()
        );
        for w in report.warnings.iter().take(3) {
            println!("      warning: {w}");
        }
    }
    Ok(())
}

/// Fetch a few of the novel's already-stored chapter numbers from chikari and
/// compare the prose. Returns how many disagreed.
async fn verify_numbering(
    store: &Store,
    chikari: &ChikariSource<ReqwestFetcher>,
    novel_id: i64,
    slug: &str,
) -> anyhow::Result<usize> {
    let stored: Vec<u32> = store.stored_chapter_numbers(novel_id)?.into_iter().collect();
    if stored.is_empty() {
        println!("  no stored chapters to check");
        return Ok(0);
    }
    let refs = chikari
        .discover_chapters(&chikari::novel_url(slug), None)
        .await?;
    println!("  {} stored locally, {} listed on chikari", stored.len(), refs.len());

    if let (Some(local_max), Some(remote_max)) = (stored.last(), refs.last().map(|r| r.number)) {
        println!("  highest number: {local_max} locally, {remote_max} on chikari");
    }

    // First, middle and last chapter present on *both* sides: the ends catch a
    // whole-sequence offset, the middle catches a shift introduced partway
    // through. Numbers chikari no longer lists are skipped — they say nothing
    // about alignment.
    let common: Vec<u32> = stored
        .iter()
        .copied()
        .filter(|n| refs.iter().any(|r| r.number == *n))
        .collect();
    if common.is_empty() {
        println!("  no chapter numbers in common to compare");
        return Ok(0);
    }
    let only_local = stored.len() - common.len();
    if only_local > 0 {
        println!("  {only_local} stored chapter(s) chikari no longer lists (kept, never deleted)");
    }

    let picks = [0, common.len() / 2, common.len() - 1];
    let mut bad = 0;
    for idx in picks.iter().copied().take(SAMPLES) {
        let number = common[idx];
        let Some(local) = store.load_chapter(novel_id, number)? else {
            continue;
        };
        let cref = refs.iter().find(|r| r.number == number).expect("filtered above");
        let remote = match chikari.fetch_chapter(cref).await {
            Ok(c) => c,
            Err(e) => {
                println!("  ch.{number}: could not fetch from chikari: {e}");
                continue;
            }
        };
        // Compare as paragraph *sets*, not position by position: chapters saved
        // by the old adapter sometimes carry the heading as their first
        // paragraph, which would shift a positional comparison by one and read
        // as a mismatch when the prose is identical.
        let overlap = paragraph_overlap(&local.paragraphs, &remote.paragraphs);
        if overlap >= 0.6 {
            println!(
                "  ch.{number}: same prose ({:.0}% of paragraphs shared)  [{}]",
                overlap * 100.0,
                remote.title
            );
        } else if is_stub(&local.paragraphs) {
            // The stored copy is a lightnovelworld "log in to read" placeholder
            // that was saved instead of the chapter. It says nothing about
            // whether the numbering lines up, so it isn't a mismatch — but it
            // is worth flagging, since chikari has the real text.
            println!(
                "  ch.{number}: stored copy is a stale lightnovelworld login stub, \
                 not prose — chikari has the real chapter  [{}]",
                remote.title
            );
        } else {
            bad += 1;
            println!("  ch.{number}: DIFFERENT TEXT ({:.0}% shared)", overlap * 100.0);
            println!("     stored : {}", preview(&local.paragraphs));
            println!("     chikari: {}", preview(&remote.paragraphs));
        }
    }
    Ok(bad)
}

/// Whether a stored chapter is a lightnovelworld gating placeholder rather than
/// the chapter itself — a handful of these are sitting in real libraries.
fn is_stub(paragraphs: &[String]) -> bool {
    if paragraphs.len() > 4 {
        return false;
    }
    let text = paragraphs.join(" ").to_ascii_lowercase();
    text.contains("requires a free account") || text.contains("log in to continue")
}

/// Fraction of `remote`'s paragraphs that also appear in `local`.
fn paragraph_overlap(local: &[String], remote: &[String]) -> f64 {
    if remote.is_empty() {
        return 0.0;
    }
    let have: std::collections::HashSet<String> = local.iter().map(|p| normalize(p)).collect();
    let hits = remote.iter().filter(|p| have.contains(&normalize(p))).count();
    hits as f64 / remote.len() as f64
}

/// Compare on letters and digits only, so punctuation or whitespace differences
/// between the two renderings don't read as a numbering mismatch.
fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .take(160)
        .collect()
}

fn preview(paragraphs: &[String]) -> String {
    paragraphs
        .first()
        .map(|p| p.chars().take(90).collect::<String>())
        .unwrap_or_default()
}
