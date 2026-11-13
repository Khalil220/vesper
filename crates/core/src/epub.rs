
use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use epub_builder::{EpubBuilder, EpubContent, EpubVersion, ReferenceType, ZipLibrary};

use crate::model::{Chapter, NovelMeta};

fn chapter_number_of(entry: &str) -> Option<u32> {
    entry
        .rsplit('/')
        .next()?
        .strip_prefix("chapter_")?
        .strip_suffix(".xhtml")?
        .parse()
        .ok()
}

pub fn chapters_in_epub(path: &Path) -> BTreeSet<u32> {
    let Ok(file) = File::open(path) else {
        return BTreeSet::new();
    };
    match zip::ZipArchive::new(file) {
        Ok(archive) => archive.file_names().filter_map(chapter_number_of).collect(),
        Err(_) => BTreeSet::new(),
    }
}

pub fn chapters_dropped(existing: &[PathBuf], chapters: &[Chapter]) -> BTreeSet<u32> {
    let keeping: BTreeSet<u32> = chapters.iter().map(|c| c.number).collect();
    existing
        .iter()
        .flat_map(|path| chapters_in_epub(path))
        .filter(|n| !keeping.contains(n))
        .collect()
}

pub struct Cover {
    pub bytes: Vec<u8>,
    pub mime: String,
}

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

macro_rules! epub_try {
    ($e:expr) => {
        ($e).map_err(|e| anyhow!("epub: {e}"))?
    };
}

pub fn build_epub(
    meta: &NovelMeta,
    chapters: &[Chapter],
    out_path: &Path,
    cover: Option<&Cover>,
    gaps: &[u32],
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

    if !gaps.is_empty() {
        let xhtml = render_gap_notice(meta, gaps);
        epub_try!(builder.add_content(
            EpubContent::new("missing-chapters.xhtml", xhtml.as_bytes())
                .title("Missing Chapters")
                .reftype(ReferenceType::Preface),
        ));
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

fn render_gap_notice(meta: &NovelMeta, gaps: &[u32]) -> String {
    let mut sorted = gaps.to_vec();
    sorted.sort_unstable();
    let list = sorted
        .iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let n = sorted.len();
    let word = if n == 1 { "chapter is" } else { "chapters are" };
    let source = url_host(&meta.source_url).unwrap_or_else(|| "the source".to_string());
    let body = format!(
        "<h1>Missing Chapters</h1>\n\
         <p>{n} {word} not included in this book because {source} did not \
         provide it (the chapter URL returned “not found”). This is usually a gap \
         in the source's own numbering, but it may be a genuinely missing chapter.</p>\n\
         <p>Unavailable chapter number(s): {list}.</p>\n\
         <p>If the chapter reappears at the source, or you add an alternate source \
         for this novel, it will be filled in automatically on a later sync.</p>",
        source = escape(&source),
        list = escape(&list),
    );
    wrap_xhtml("Missing Chapters", &body)
}

fn url_host(url: &str) -> Option<String> {
    url::Url::parse(url).ok()?.host_str().map(|h| h.to_string())
}

fn render_chapter_xhtml(heading: &str, ch: &Chapter) -> String {
    let mut body = format!("<h1>{}</h1>\n", escape(heading));
    for p in &ch.paragraphs {
        body.push_str(&format!("<p>{}</p>\n", escape(p).replace('\n', "<br/>")));
    }
    wrap_xhtml(heading, &body)
}

fn wrap_xhtml(title: &str, body_html: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <html xmlns=\"http://www.w3.org/1999/xhtml\">\
         <head><title>{}</title></head><body>{}</body></html>",
        escape(title),
        body_html
    )
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::NovelStatus;

    fn meta() -> NovelMeta {
        NovelMeta {
            title: "Guarded Novel".into(),
            author: Some("An Author".into()),
            cover_url: None,
            genre: None,
            status_hint: NovelStatus::Completed,
            source_url: "https://example.com/n".into(),
        }
    }

    fn chapters(numbers: impl IntoIterator<Item = u32>) -> Vec<Chapter> {
        numbers
            .into_iter()
            .map(|n| Chapter {
                number: n,
                title: format!("Chapter {n}"),
                paragraphs: vec![format!("Text of chapter {n}.")],
            })
            .collect()
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vesper-epub-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_back_which_chapters_an_epub_holds() {
        let dir = scratch("read");
        let path = dir.join("book.epub");
        build_epub(&meta(), &chapters([1, 2, 5]), &path, None, &[3, 4]).unwrap();
        assert_eq!(chapters_in_epub(&path), BTreeSet::from([1, 2, 5]));
        assert!(chapters_in_epub(&dir.join("absent.epub")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_line_break_inside_a_paragraph_shows_in_the_book() {
        let dir = scratch("breaks");
        let path = dir.join("book.epub");
        let mut chapter = chapters([1]);
        chapter[0].paragraphs = vec!["Strength: 10\nAgility: 12".into(), "A <b> & c".into()];
        build_epub(&meta(), &chapter, &path, None, &[]).unwrap();

        let mut archive = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let name = archive
            .file_names()
            .find(|n| n.ends_with("chapter_00001.xhtml"))
            .unwrap()
            .to_string();
        let mut page = String::new();
        std::io::Read::read_to_string(&mut archive.by_name(&name).unwrap(), &mut page).unwrap();
        assert!(page.contains("<p>Strength: 10<br/>Agility: 12</p>"), "{page}");
        assert!(page.contains("<p>A &lt;b&gt; &amp; c</p>"), "{page}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pruned_library_would_drop_chapters_from_the_book() {
        let dir = scratch("dropped");
        let path = dir.join("book.epub");
        build_epub(&meta(), &chapters(1..=10), &path, None, &[]).unwrap();
        let existing = vec![path];

        assert_eq!(chapters_dropped(&existing, &chapters(8..=11)), BTreeSet::from_iter(1..=7));
        assert!(chapters_dropped(&existing, &chapters(1..=11)).is_empty(), "a longer book is fine");
        assert!(chapters_dropped(&[], &chapters(1..=2)).is_empty(), "nothing exported yet");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
