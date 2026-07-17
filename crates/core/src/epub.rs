//! EPUB packaging.
//!
//! Chapters are rendered to clean, reconstructed XHTML (we emit our own
//! `<p>` elements from extracted text rather than passing site HTML through),
//! which keeps the EPUB valid regardless of the source markup. The file is
//! written atomically: a temp file is generated first, then renamed over the
//! target, so a crash mid-write never corrupts an existing EPUB.

use std::fs::File;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use epub_builder::{EpubBuilder, EpubContent, EpubVersion, ReferenceType, ZipLibrary};

use crate::model::{Chapter, NovelMeta};

/// `epub-builder` reports errors as `eyre::Report`, which is not a
/// `std::error::Error`, so `?` can't lift it into `anyhow`. Convert via Display.
macro_rules! epub_try {
    ($e:expr) => {
        ($e).map_err(|e| anyhow!("epub: {e}"))?
    };
}

/// Build an EPUB for `meta`/`chapters` at `out_path` (atomic write).
pub fn build_epub(meta: &NovelMeta, chapters: &[Chapter], out_path: &Path) -> Result<()> {
    let zip = epub_try!(ZipLibrary::new());
    let mut builder = epub_try!(EpubBuilder::new(zip));
    builder.epub_version(EpubVersion::V30);
    epub_try!(builder.metadata("title", &meta.title));
    if let Some(author) = &meta.author {
        epub_try!(builder.metadata("author", author));
    }
    epub_try!(builder.metadata("lang", "en"));

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
