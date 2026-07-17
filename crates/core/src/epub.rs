//! EPUB packaging.
//!
//! Chapters are rendered to clean, reconstructed XHTML (we emit our own
//! `<p>` elements from extracted text rather than passing site HTML through),
//! which keeps the EPUB valid regardless of the source markup. The file is
//! written atomically: a temp file is generated first, then renamed over the
//! target, so a crash mid-write never corrupts an existing EPUB.

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use epub_builder::{EpubBuilder, EpubContent, EpubVersion, ReferenceType, ZipLibrary};

use crate::model::{Chapter, NovelMeta};

/// A cover image to embed: raw bytes plus its MIME type.
pub struct Cover {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// Best-effort download of a cover image (browser UA). Returns `None` on any
/// failure so export never breaks over a missing/blocked cover.
pub async fn download_cover(url: &str) -> Option<Cover> {
    let client = reqwest::Client::builder()
        .user_agent(crate::fetch::DEFAULT_UA)
        .timeout(Duration::from_secs(30))
        .build()
        .ok()?;
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
        .filter(|s| s.starts_with("image/"))
        .unwrap_or_else(|| "image/jpeg".to_string());
    let bytes = resp.bytes().await.ok()?.to_vec();
    (!bytes.is_empty()).then_some(Cover { bytes, mime })
}

fn cover_filename(mime: &str) -> &'static str {
    match mime {
        "image/png" => "cover.png",
        "image/webp" => "cover.webp",
        "image/gif" => "cover.gif",
        _ => "cover.jpg",
    }
}

/// `epub-builder` reports errors as `eyre::Report`, which is not a
/// `std::error::Error`, so `?` can't lift it into `anyhow`. Convert via Display.
macro_rules! epub_try {
    ($e:expr) => {
        ($e).map_err(|e| anyhow!("epub: {e}"))?
    };
}

/// Build an EPUB for `meta`/`chapters` at `out_path` (atomic write). Optionally
/// embeds a cover image.
pub fn build_epub(
    meta: &NovelMeta,
    chapters: &[Chapter],
    out_path: &Path,
    cover: Option<&Cover>,
) -> Result<()> {
    let zip = epub_try!(ZipLibrary::new());
    let mut builder = epub_try!(EpubBuilder::new(zip));
    builder.epub_version(EpubVersion::V30);
    epub_try!(builder.metadata("title", &meta.title));
    if let Some(author) = &meta.author {
        epub_try!(builder.metadata("author", author));
    }
    if let Some(genre) = &meta.genre {
        epub_try!(builder.metadata("subject", genre));
    }
    epub_try!(builder.metadata("lang", "en"));
    if let Some(cover) = cover {
        epub_try!(builder.add_cover_image(cover_filename(&cover.mime), &cover.bytes[..], &cover.mime));
    }

    for ch in chapters {
        let filename = format!("chapter_{:05}.xhtml", ch.number);
        let heading = format!("Chapter {}: {}", ch.number, ch.title);
        let xhtml = render_chapter_xhtml(&heading, ch);
        epub_try!(builder.add_content(
            EpubContent::new(&filename, xhtml.as_bytes())
                .title(heading)
                .reftype(ReferenceType::Text),
        ));
    }

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating output directory {}", parent.display()))?;
    }

    let tmp = out_path.with_extension("epub.tmp");
    {
        let mut f =
            File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        epub_try!(builder.generate(&mut f));
    }
    std::fs::rename(&tmp, out_path)
        .with_context(|| format!("renaming {} -> {}", tmp.display(), out_path.display()))?;
    Ok(())
}

fn render_chapter_xhtml(heading: &str, ch: &Chapter) -> String {
    let mut body = format!("<h1>{}</h1>\n", escape(heading));
    for p in &ch.paragraphs {
        body.push_str(&format!("<p>{}</p>\n", escape(p)));
    }
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\">\
         <head><title>{}</title></head><body>{}</body></html>",
        escape(heading),
        body
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
