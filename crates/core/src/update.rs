use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context, Result};

use crate::store::Store;

const LATEST_RELEASE: &str = "https://api.github.com/repos/Khalil220/vesper/releases/latest";
const LATEST_KEY: &str = "latest_release";
const CHECKED_KEY: &str = "update_checked_at";
const DAY: i64 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub page: String,
    pub assets: Vec<Asset>,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }
}

pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let mut parts = text.trim().trim_start_matches('v').split('.');
    let mut next = || parts.next()?.parse::<u32>().ok();
    let version = (next()?, next()?, next()?);
    parts.next().is_none().then_some(version)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(c), Some(k)) if c > k)
}

pub fn parse_release(json: &str) -> Result<Release> {
    let value: serde_json::Value = serde_json::from_str(json).context("reading GitHub's answer")?;
    let tag = value["tag_name"]
        .as_str()
        .ok_or_else(|| anyhow!("GitHub's answer names no release"))?;
    ensure!(
        parse_version(tag).is_some(),
        "the latest release is tagged {tag}, which isn't a version number"
    );
    let assets = value["assets"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|a| {
                    Some(Asset {
                        name: a["name"].as_str()?.to_string(),
                        url: a["browser_download_url"].as_str()?.to_string(),
                        size: a["size"].as_u64()?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Release {
        version: tag.trim_start_matches('v').to_string(),
        page: value["html_url"].as_str().unwrap_or_default().to_string(),
        assets,
    })
}

fn asset_for(os: &str, arch: &str, version: &str) -> Option<String> {
    let target = match (os, arch) {
        ("windows", "x86_64") => return Some("vesper.exe".to_string()),
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        _ => return None,
    };
    Some(format!("vesper-v{version}-{target}.tar.gz"))
}

pub fn asset_name(version: &str) -> Option<String> {
    asset_for(std::env::consts::OS, std::env::consts::ARCH, version)
}

fn client(timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("vesper/", env!("CARGO_PKG_VERSION")))
        .timeout(timeout)
        .build()?)
}

pub async fn latest_release() -> Result<Release> {
    let response = client(Duration::from_secs(20))?
        .get(LATEST_RELEASE)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("asking GitHub for the latest release")?;
    let status = response.status();
    ensure!(
        status.is_success(),
        "GitHub answered {status} when asked for the latest release"
    );
    parse_release(&response.text().await?)
}

pub async fn download(asset: &Asset) -> Result<Vec<u8>> {
    let response = client(Duration::from_secs(300))?
        .get(&asset.url)
        .send()
        .await
        .with_context(|| format!("downloading {}", asset.name))?;
    let status = response.status();
    ensure!(status.is_success(), "GitHub answered {status} for {}", asset.name);
    let bytes = response
        .bytes()
        .await
        .with_context(|| format!("downloading {}", asset.name))?;
    ensure!(
        bytes.len() as u64 == asset.size,
        "{} came down as {} bytes instead of {}",
        asset.name,
        bytes.len(),
        asset.size
    );
    Ok(bytes.to_vec())
}

pub fn binary_from_archive(archive: &[u8]) -> Result<Vec<u8>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in archive.entries().context("reading the downloaded archive")? {
        let mut entry = entry.context("reading the downloaded archive")?;
        if entry.header().entry_type().is_file()
            && entry.path()?.file_name() == Some(OsStr::new("vesper"))
        {
            let mut binary = Vec::new();
            entry.read_to_end(&mut binary)?;
            return Ok(binary);
        }
    }
    bail!("the downloaded archive has no vesper binary in it")
}

pub fn staged_path(exe: &Path) -> PathBuf {
    exe.with_file_name(if cfg!(windows) { "vesper-update.exe" } else { "vesper-update" })
}

pub fn retired_path(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    exe.with_file_name(name)
}

pub fn remove_retired(exe: &Path) {
    let _ = std::fs::remove_file(retired_path(exe));
}

pub fn install(exe: &Path, binary: &[u8], version: &str) -> Result<()> {
    let staged = staged_path(exe);
    std::fs::write(&staged, binary)
        .with_context(|| format!("saving the new version next to {}", exe.display()))?;
    if let Err(e) = make_runnable(&staged).and_then(|()| check_version(&staged, version)) {
        let _ = std::fs::remove_file(&staged);
        return Err(e);
    }
    swap(exe, &staged)
}

#[cfg(unix)]
fn make_runnable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_runnable(_: &Path) -> Result<()> {
    Ok(())
}

fn check_version(binary: &Path, version: &str) -> Result<()> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .context("the download doesn't run on this system, so nothing was replaced")?;
    let said = String::from_utf8_lossy(&output.stdout);
    ensure!(
        output.status.success() && said.trim() == format!("vesper {version}"),
        "the download doesn't run as Vesper {version}, so nothing was replaced"
    );
    Ok(())
}

#[cfg(windows)]
fn swap(exe: &Path, staged: &Path) -> Result<()> {
    let retired = retired_path(exe);
    let _ = std::fs::remove_file(&retired);
    if let Err(e) = std::fs::rename(exe, &retired) {
        let _ = std::fs::remove_file(staged);
        return Err(anyhow!(e).context(format!("moving {} aside", exe.display())));
    }
    if let Err(e) = std::fs::rename(staged, exe) {
        let _ = std::fs::rename(&retired, exe);
        let _ = std::fs::remove_file(staged);
        return Err(anyhow!(e).context(format!("putting the new version at {}", exe.display())));
    }
    Ok(())
}

#[cfg(not(windows))]
fn swap(exe: &Path, staged: &Path) -> Result<()> {
    if let Err(e) = std::fs::rename(staged, exe) {
        let _ = std::fs::remove_file(staged);
        return Err(anyhow!(e).context(format!("putting the new version at {}", exe.display())));
    }
    Ok(())
}

pub fn check_due(store: &Store, now: i64) -> Result<bool> {
    let last = store
        .meta_get(CHECKED_KEY)?
        .and_then(|v| v.parse::<i64>().ok());
    Ok(last.is_none_or(|at| now - at >= DAY || now < at))
}

pub fn record_latest(store: &Store, version: &str, now: i64) -> Result<()> {
    store.meta_set(LATEST_KEY, version)?;
    store.meta_set(CHECKED_KEY, &now.to_string())
}

pub fn newer_release(store: &Store, current: &str) -> Result<Option<String>> {
    Ok(store.meta_get(LATEST_KEY)?.filter(|v| is_newer(v, current)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vesper-update-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn versions_compare_by_number() {
        assert_eq!(parse_version("1.4.0"), Some((1, 4, 0)));
        assert_eq!(parse_version("v1.10.0"), Some((1, 10, 0)));
        assert_eq!(parse_version("1.4"), None);
        assert_eq!(parse_version("1.4.0.1"), None);
        assert_eq!(parse_version("nightly"), None);
        assert!(is_newer("1.10.0", "1.9.0"));
        assert!(is_newer("v1.5.0", "1.4.0"));
        assert!(!is_newer("1.4.0", "1.4.0"));
        assert!(!is_newer("1.3.0", "1.4.0"), "never a downgrade");
        assert!(!is_newer("nightly", "1.4.0"));
    }

    #[test]
    fn reads_githubs_latest_release() {
        let json = r#"{
            "tag_name": "v1.4.0",
            "html_url": "https://github.com/Khalil220/vesper/releases/tag/v1.4.0",
            "assets": [
                {"name": "vesper.exe", "size": 8381440,
                 "browser_download_url": "https://github.com/Khalil220/vesper/releases/download/v1.4.0/vesper.exe"},
                {"name": "vesper-v1.4.0-x86_64-unknown-linux-gnu.tar.gz", "size": 4000000,
                 "browser_download_url": "https://github.com/Khalil220/vesper/releases/download/v1.4.0/vesper-v1.4.0-x86_64-unknown-linux-gnu.tar.gz"},
                {"name": "broken"}
            ]
        }"#;
        let release = parse_release(json).unwrap();
        assert_eq!(release.version, "1.4.0");
        assert_eq!(release.page, "https://github.com/Khalil220/vesper/releases/tag/v1.4.0");
        assert_eq!(release.assets.len(), 2);
        assert_eq!(release.asset("vesper.exe").unwrap().size, 8_381_440);
        assert!(release.asset("broken").is_none());

        let err = parse_release(r#"{"tag_name": "nightly"}"#).unwrap_err().to_string();
        assert!(err.contains("nightly"), "{err}");
        assert!(parse_release(r#"{"message": "Not Found"}"#).is_err());
    }

    #[test]
    fn asset_names_match_what_the_release_workflow_uploads() {
        assert_eq!(asset_for("windows", "x86_64", "1.5.0").unwrap(), "vesper.exe");
        assert_eq!(
            asset_for("linux", "x86_64", "1.5.0").unwrap(),
            "vesper-v1.5.0-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_for("macos", "aarch64", "1.5.0").unwrap(),
            "vesper-v1.5.0-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_for("macos", "x86_64", "1.5.0").unwrap(),
            "vesper-v1.5.0-x86_64-apple-darwin.tar.gz"
        );
        assert_eq!(asset_for("linux", "aarch64", "1.5.0"), None);
        assert!(asset_name("1.5.0").is_some(), "the platform running the tests has a build");
    }

    fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(flate2::write::GzEncoder::new(
            Vec::new(),
            flate2::Compression::default(),
        ));
        for (path, data) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, path, *data).unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn the_binary_comes_out_of_the_release_tarball() {
        let archive = tarball(&[
            ("vesper-v1.5.0-x86_64-unknown-linux-gnu/README.md", b"# Vesper"),
            ("vesper-v1.5.0-x86_64-unknown-linux-gnu/vesper", b"\x7fELF binary"),
        ]);
        assert_eq!(binary_from_archive(&archive).unwrap(), b"\x7fELF binary");

        let without = tarball(&[("vesper-v1.5.0-x86_64-unknown-linux-gnu/README.md", b"# Vesper")]);
        assert!(binary_from_archive(&without).is_err());
        assert!(binary_from_archive(b"not an archive").is_err());
    }

    #[test]
    fn a_download_that_does_not_run_leaves_the_old_version_alone() {
        let dir = scratch("junk");
        let exe = dir.join(if cfg!(windows) { "vesper.exe" } else { "vesper" });
        std::fs::write(&exe, b"the old version").unwrap();

        assert!(install(&exe, b"not a program", "9.9.0").is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"the old version");
        assert!(!staged_path(&exe).exists());
        assert!(!retired_path(&exe).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_swap_puts_the_new_version_at_the_old_path() {
        let dir = scratch("swap");
        let exe = dir.join(if cfg!(windows) { "vesper.exe" } else { "vesper" });
        for round in ["first", "second"] {
            std::fs::write(&exe, format!("before the {round} update")).unwrap();
            let staged = staged_path(&exe);
            std::fs::File::create(&staged).unwrap().write_all(round.as_bytes()).unwrap();
            swap(&exe, &staged).unwrap();
            assert_eq!(std::fs::read_to_string(&exe).unwrap(), round);
            assert!(!staged.exists());
        }
        if cfg!(windows) {
            assert_eq!(
                std::fs::read_to_string(retired_path(&exe)).unwrap(),
                "before the second update"
            );
        }
        remove_retired(&exe);
        assert!(!retired_path(&exe).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_daily_check_remembers_what_it_found() {
        let store = Store::open_in_memory().unwrap();
        let now = 1_800_000_000;
        assert!(check_due(&store, now).unwrap());
        assert_eq!(newer_release(&store, "1.4.0").unwrap(), None);

        record_latest(&store, "1.5.0", now).unwrap();
        assert!(!check_due(&store, now + DAY - 1).unwrap());
        assert!(check_due(&store, now + DAY).unwrap());
        assert!(check_due(&store, now - 60).unwrap(), "a clock set back doesn't stop the checks");

        assert_eq!(newer_release(&store, "1.4.0").unwrap().as_deref(), Some("1.5.0"));
        assert_eq!(newer_release(&store, "1.5.0").unwrap(), None);
        assert_eq!(newer_release(&store, "1.6.0").unwrap(), None);
    }
}
