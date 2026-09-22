use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use rusqlite::{Connection, OpenFlags};
use vesper_core::freewebnovel::strip_promo;

fn main() -> Result<()> {
    let path: PathBuf = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow!("usage: promo_audit <path-to-library.db>"))?
        .into();

    let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare("SELECT novel_id, number, body FROM chapters")?;
    let mut rows = stmt.query([])?;

    let mut chapters = 0usize;
    let mut touched = 0usize;
    let mut removals: BTreeMap<String, usize> = BTreeMap::new();
    let mut prose_eaten: Vec<(i64, u32, String)> = Vec::new();

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
            *removals.entry(removed).or_default() += 1;
        }
        if changed {
            touched += 1;
        }
    }

    let total: usize = removals.values().sum();
    println!("chapters scanned: {chapters}");
    println!("chapters the matcher would change: {touched}");
    println!("removals: {total} ({} distinct wordings)", removals.len());
    println!(
        "longest removal: {} chars",
        removals.keys().map(|r| r.chars().count()).max().unwrap_or(0)
    );

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

fn names_the_site(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("freewebnovel") || lower.contains("empire")
}
