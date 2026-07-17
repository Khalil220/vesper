//! Site profiles for the generic adapter.
//!
//! Built-in profiles live here in code (novgo). Users can add more
//! *config-driven* profiles by dropping `.ini` files into
//! `<config_dir>/vesper/profiles/` — no recompile — for any site that
//! fits the generic shape (server-rendered, CSS-selectable content, `?page=N`
//! table of contents). A `README.txt` documenting the format is generated
//! there on first use.

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Result};
use directories::ProjectDirs;
use ini::{Ini, ParseOption};
use url::Url;

use crate::source::SiteProfile;

/// Profile for novgo.net (built in).
pub fn novgo() -> SiteProfile {
    SiteProfile {
        name: "novgo".into(),
        host: "novgo.net".into(),
        content_selector: "#chapter-content".into(),
        paragraph_selector: "p".into(),
        chapter_marker: "/chapter-".into(),
        page_param: "page".into(),
        max_pages: 500,
    }
}

/// All profiles: built-ins plus any loaded from external `.ini` files.
pub fn all() -> Vec<SiteProfile> {
    let mut v = vec![novgo()];
    v.extend(external());
    v
}

/// The profile whose host matches `url`, if any.
pub fn for_url(url: &str) -> Option<SiteProfile> {
    let host = Url::parse(url).ok()?.host_str()?.to_string();
    all()
        .into_iter()
        .find(|p| host.eq_ignore_ascii_case(&p.host))
}

/// Directory holding user-supplied profile files.
pub fn profiles_dir() -> Option<PathBuf> {
    ProjectDirs::from("", "", "vesper").map(|d| d.config_dir().join("profiles"))
}

/// Load external profiles, skipping (with a warning) any that fail to parse.
/// Best-effort: returns an empty list if the directory is unavailable.
pub fn external() -> Vec<SiteProfile> {
    let Some(dir) = profiles_dir() else {
        return Vec::new();
    };
    let _ = ensure_readme(&dir);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("ini") {
            continue;
        }
        match load_profile(&path) {
            Ok(p) => out.push(p),
            Err(e) => eprintln!("  ! ignoring profile {}: {e}", path.display()),
        }
    }
    out
}

fn load_profile(path: &std::path::Path) -> Result<SiteProfile> {
    let opt = ParseOption {
        enabled_escape: false,
        ..ParseOption::default()
    };
    let ini = Ini::load_from_file_opt(path, opt)?;
    let s = ini
        .section(Some("profile"))
        .ok_or_else(|| anyhow!("missing [profile] section"))?;
    let req = |key: &str| {
        s.get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("missing required key `{key}`"))
    };
    let opt = |key: &str, dflt: &str| {
        s.get(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| dflt.to_string())
    };

    Ok(SiteProfile {
        name: req("name")?,
        host: req("host")?,
        content_selector: req("content_selector")?,
        paragraph_selector: opt("paragraph_selector", "p"),
        chapter_marker: opt("chapter_marker", "/chapter-"),
        page_param: opt("page_param", "page"),
        max_pages: s
            .get("max_pages")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(500),
    })
}

/// Write a README documenting the profile format (once), so users can discover
/// how to add sites.
fn ensure_readme(dir: &std::path::Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let readme = dir.join("README.txt");
    if readme.exists() {
        return Ok(());
    }
    let text = "\
Add a site by creating a `.ini` file in this folder (one per site). Vesper\n\
loads every `.ini` here at startup. Files that fail to parse are skipped with a\n\
warning. This works for sites with the generic shape: server-rendered HTML,\n\
CSS-selectable chapter text, and a `?page=N` paginated table of contents. Sites\n\
that need JavaScript or an odd layout require a built-in hand-written adapter.\n\
\n\
Format (example `mysite.ini`):\n\
\n\
[profile]\n\
name = mysite                     ; label shown in listings\n\
host = mysite.com                 ; URLs on this host use this profile\n\
content_selector = #chapter-content   ; CSS selector for the chapter body\n\
paragraph_selector = p            ; optional (default: p)\n\
chapter_marker = /chapter-        ; optional: substring marking chapter links\n\
page_param = page                 ; optional: ToC pagination query param\n\
max_pages = 500                   ; optional safety cap (default: 500)\n\
\n\
Required: name, host, content_selector. The rest have the defaults shown.\n";
    fs::write(&readme, text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_profile_reads_required_and_defaults() {
        let dir = std::env::temp_dir().join(format!("vesper-prof-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mysite.ini");
        fs::write(
            &path,
            "[profile]\nname = mysite\nhost = mysite.com\ncontent_selector = .article\n",
        )
        .unwrap();

        let p = load_profile(&path).unwrap();
        assert_eq!(p.name, "mysite");
        assert_eq!(p.host, "mysite.com");
        assert_eq!(p.content_selector, ".article");
        assert_eq!(p.paragraph_selector, "p"); // default
        assert_eq!(p.chapter_marker, "/chapter-"); // default
        assert_eq!(p.max_pages, 500); // default

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_profile_rejects_missing_required_key() {
        let dir = std::env::temp_dir().join(format!("vesper-prof2-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bad.ini");
        fs::write(&path, "[profile]\nname = incomplete\n").unwrap();
        assert!(load_profile(&path).is_err());
        fs::remove_dir_all(&dir).ok();
    }
}
