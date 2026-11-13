use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Result};
use rusqlite::{Connection, OpenFlags};
use vesper_core::promo::{fold, strip_promo};
use vesper_core::{default_db_path, scrub_novel, Store};

const SITE_WORDS: &[&str] = &[
    "reewebnovel",
    "empire",
    "panda",
    "lightnovel",
    "novelfire",
    "novelphoenix",
    "eaglesnov",
    "myboxnovel",
    "allnovelfull",
    "newnovel",
    "buyersclub",
    "contentcopysite",
    "betterexperience",
    "morevisit",
    "pleasevisit",
    "chaptersvisit",
];

const LINK_SIGNS: &[&str] = &["t.me/", "discord.gg/"];

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let path: PathBuf = args
        .next()
        .ok_or_else(|| anyhow!("usage: promo_audit <path-to-library.db> [--apply]"))?
        .into();
    let apply = args.any(|a| a == "--apply");

    audit(&path)?;
    if apply {
        scrub_copy(&path)?;
    }
    Ok(())
}

fn audit(path: &PathBuf) -> Result<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT novel_id, number, body FROM chapters")?;
    let mut rows = stmt.query([])?;

    let mut chapters = 0usize;
    let mut touched = 0usize;
    let mut removals: BTreeMap<String, usize> = BTreeMap::new();
    let mut prose_eaten: Vec<(i64, u32, String)> = Vec::new();
    let mut longest = 0usize;

    while let Some(row) = rows.next()? {
        let (novel_id, number, body): (i64, u32, String) = (row.get(0)?, row.get(1)?, row.get(2)?);
        chapters += 1;
        let mut changed = false;
        for paragraph in body.split("\n\n") {
            let kept = strip_promo(paragraph);
            if kept.as_deref() == Some(paragraph.trim()) {
                continue;
            }
            changed = true;
            let removed = match &kept {
                None => paragraph.trim().to_string(),
                Some(k) => difference(paragraph.trim(), k),
            };
            if !names_the_site(&removed) {
                prose_eaten.push((novel_id, number, removed.clone()));
            }
            longest = longest.max(removed.chars().count());
            *removals.entry(fold_key(&removed)).or_default() += 1;
        }
        if changed {
            touched += 1;
        }
    }

    let total: usize = removals.values().sum();
    println!("chapters scanned: {chapters}");
    println!("chapters the matcher would change: {touched}");
    println!("removals: {total} ({} distinct wordings)", removals.len());
    println!("longest removal: {longest} chars");

    if prose_eaten.is_empty() {
        println!("removals that do not name the site: none");
    } else {
        println!("\nPROSE EATEN ({}):", prose_eaten.len());
        for (novel_id, number, text) in &prose_eaten {
            println!("  novel {novel_id} ch.{number}: {text}");
        }
    }

    if total > 0 {
        println!("\nwordings:");
        for (text, count) in &removals {
            println!("  x{count}: {text}");
        }
    }
    Ok(())
}

fn scrub_copy(path: &PathBuf) -> Result<()> {
    let live = default_db_path()?;
    if path.canonicalize().ok() == live.canonicalize().ok() {
        bail!("refusing to scrub the live library; point --apply at a copy");
    }
    let store = Store::open(path)?;
    let ids: Vec<i64> = store.list_subscriptions()?.iter().map(|n| n.id).collect();
    let mut chapters = 0usize;
    let mut marks = 0usize;
    for id in ids {
        let report = scrub_novel(&store, id, false)?;
        chapters += report.chapters.len();
        marks += report.marks;
    }
    println!("\nscrubbed {marks} marks across {chapters} chapters in {}", path.display());
    Ok(())
}

fn difference(original: &str, kept: &str) -> String {
    let head = original
        .char_indices()
        .zip(kept.chars())
        .take_while(|((_, a), b)| a == b)
        .map(|((i, a), _)| i + a.len_utf8())
        .last()
        .unwrap_or(0);

    let (rest_original, rest_kept) = (&original[head..], &kept[head..]);
    let tail: usize = rest_original
        .char_indices()
        .rev()
        .zip(rest_kept.chars().rev())
        .take_while(|((_, a), b)| a == b)
        .map(|((_, a), _)| a.len_utf8())
        .sum();

    original[head..rest_original.len() + head - tail].trim().to_string()
}

fn fold_key(removed: &str) -> String {
    let folded = fold(removed);
    if folded.len() > 44 {
        format!("{}~", &folded[..folded.floor_char_boundary(44)])
    } else {
        folded
    }
}

fn names_the_site(text: &str) -> bool {
    let folded = fold(text);
    let tld = ["com", "net", "org"].iter().any(|t| folded.ends_with(t));
    let lower = text.to_lowercase();
    tld || SITE_WORDS.iter().any(|w| folded.contains(w)) || LINK_SIGNS.iter().any(|s| lower.contains(s))
}
