//! Live smoke check: fetch a handful of real chapters through the actual
//! adapter + fetch tier and print the parsed titles.
//!
//! Not part of `cargo test` (it hits the network). Run it by hand:
//!   cargo run -p vesper-core --example live_titles -- <novel-url> 1 2 61 62

use std::time::Duration;

use vesper_core::model::ChapterRef;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().expect("usage: live_titles <novel-url> <chapter numbers...>");
    let numbers: Vec<u32> = args.map(|a| a.parse().expect("chapter number")).collect();

    let source = vesper_core::build_source(&url, Duration::from_millis(1200))
        .expect("no adapter handles that host");

    let refs = source.discover_chapters(&url, None).await?;
    println!("discovered {} chapters", refs.len());

    for n in numbers {
        let Some(r) = refs.iter().find(|r| r.number == n) else {
            println!("  {n}: not in the discovered range");
            continue;
        };
        let placeholder: &ChapterRef = r;
        match source.fetch_chapter(placeholder).await {
            Ok(ch) => println!(
                "  {n}: title={:?}  ({} paragraphs)",
                ch.title,
                ch.paragraphs.len()
            ),
            Err(e) => println!("  {n}: ERROR {e}"),
        }
    }
    Ok(())
}
