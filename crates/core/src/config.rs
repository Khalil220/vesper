//! Global configuration (`config.ini`).
//!
//! A single flat INI file, generated with defaults on first use, holding
//! settings that aren't per-novel. Per-novel overrides live in the DB. The file
//! is written with explanatory comments; reading is tolerant — any missing key
//! falls back to its default.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use directories::{ProjectDirs, UserDirs};
use ini::{Ini, ParseOption};

#[derive(Debug, Clone)]
pub struct Config {
    /// Where EPUBs are written (`<output_dir>/<author>/<novel>/...`).
    pub output_dir: PathBuf,
    /// Base politeness delay between requests, in milliseconds.
    pub request_delay_ms: u64,
    /// How often the background task runs, in minutes (used at install time).
    pub poll_interval_minutes: u32,
    /// Keep exported chapters this many days before pruning (0 = purge on export).
    pub retention_days: u32,
    /// Days a completed novel must be quiet before it's judged LikelyComplete.
    pub quiet_grace_days: u32,
    /// How often (days) to re-check a LikelyComplete novel; between re-checks the
    /// scheduled sync skips it instead of polling every interval.
    pub likely_complete_recheck_days: u32,
    /// Export automatically once a novel's initial backfill completes.
    pub auto_export: bool,
    /// Re-export automatically when a Live novel gains new chapters.
    pub auto_append: bool,
    /// Split EPUBs into volumes of this many chapters (0 = single file).
    pub split_every_chapters: u32,
    /// Where the background sync appends its log (its stderr is discarded).
    pub log_path: PathBuf,
}

/// Default log path: `<data_local>/crawler.log`.
pub fn default_log_path() -> PathBuf {
    ProjectDirs::from("", "", "webnovel-crawler")
        .map(|d| d.data_local_dir().join("crawler.log"))
        .unwrap_or_else(|| PathBuf::from("crawler.log"))
}

/// Default EPUB output directory: `<Documents>/lightnovels`, or `./lightnovels`.
pub fn default_output_dir() -> PathBuf {
    UserDirs::new()
        .and_then(|u| u.document_dir().map(|d| d.join("lightnovels")))
        .unwrap_or_else(|| PathBuf::from("lightnovels"))
}

impl Default for Config {
    fn default() -> Self {
        Self {
            output_dir: default_output_dir(),
            request_delay_ms: 1500,
            poll_interval_minutes: 60,
            retention_days: 30,
            quiet_grace_days: 30,
            likely_complete_recheck_days: 7,
            auto_export: false,
            auto_append: false,
            split_every_chapters: 0,
            log_path: default_log_path(),
        }
    }
}

/// Path to the config file: `<config_dir>/config.ini`.
pub fn config_path() -> Result<PathBuf> {
    let dirs = ProjectDirs::from("", "", "webnovel-crawler")
        .ok_or_else(|| anyhow!("could not resolve a config directory"))?;
    Ok(dirs.config_dir().join("config.ini"))
}

impl Config {
    /// Load the config, creating it with defaults if it doesn't exist yet.
    pub fn load_or_create() -> Result<Self> {
        let path = config_path()?;
        if path.exists() {
            Self::read(&path)
        } else {
            let cfg = Self::default();
            cfg.write(&path)?;
            Ok(cfg)
        }
    }

    fn read(path: &Path) -> Result<Self> {
        // Disable backslash escaping so Windows paths (C:\Users\...) round-trip
        // literally instead of the parser eating the separators.
        let opt = ParseOption {
            enabled_escape: false,
            ..ParseOption::default()
        };
        let ini = Ini::load_from_file_opt(path, opt)
            .with_context(|| format!("reading config {}", path.display()))?;
        let g = ini.section(Some("general"));
        let d = Config::default();

        let get = |key: &str| g.and_then(|s| s.get(key)).map(str::trim);
        let parse_u64 = |key, dflt| get(key).and_then(|v| v.parse().ok()).unwrap_or(dflt);
        let parse_u32 = |key, dflt| get(key).and_then(|v| v.parse().ok()).unwrap_or(dflt);
        let parse_bool = |key, dflt| {
            get(key)
                .map(|v| matches!(v.to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "on"))
                .unwrap_or(dflt)
        };

        Ok(Config {
            output_dir: get("output_dir")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or(d.output_dir),
            request_delay_ms: parse_u64("request_delay_ms", d.request_delay_ms),
            poll_interval_minutes: parse_u32("poll_interval_minutes", d.poll_interval_minutes),
            retention_days: parse_u32("retention_days", d.retention_days),
            quiet_grace_days: parse_u32("quiet_grace_days", d.quiet_grace_days),
            likely_complete_recheck_days: parse_u32(
                "likely_complete_recheck_days",
                d.likely_complete_recheck_days,
            ),
            auto_export: parse_bool("auto_export", d.auto_export),
            auto_append: parse_bool("auto_append", d.auto_append),
            split_every_chapters: parse_u32("split_every_chapters", d.split_every_chapters),
            log_path: get("log_path")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or(d.log_path),
        })
    }

    /// Write the config with explanatory comments (used to generate defaults).
    pub fn write(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let content = format!(
            "; webnovel-crawler configuration\n\
             ; Edit values below; delete this file to regenerate defaults.\n\n\
             [general]\n\
             ; Where EPUBs are written: <output_dir>/<author>/<novel>/...\n\
             ; Note: on Windows, Documents may be OneDrive-backed (files sync to the cloud).\n\
             output_dir = {}\n\n\
             ; Base politeness delay between requests, in milliseconds.\n\
             request_delay_ms = {}\n\n\
             ; How often the background sync runs, in minutes (applied when installing the service).\n\
             poll_interval_minutes = {}\n\n\
             ; Keep exported chapters this many days before pruning completed novels (0 = purge on export).\n\
             retention_days = {}\n\n\
             ; Days a completed novel must be quiet before it's treated as finished.\n\
             quiet_grace_days = {}\n\n\
             ; How often (days) to re-check a finished novel; it's skipped between re-checks.\n\
             likely_complete_recheck_days = {}\n\n\
             ; Automatically export once a novel's initial backfill completes.\n\
             auto_export = {}\n\n\
             ; Automatically re-export when a caught-up novel gains new chapters.\n\
             auto_append = {}\n\n\
             ; Split EPUBs into volumes of this many chapters (0 = one file per novel).\n\
             split_every_chapters = {}\n\n\
             ; Where the background sync writes its log (its console output is hidden).\n\
             log_path = {}\n",
            self.output_dir.display(),
            self.request_delay_ms,
            self.poll_interval_minutes,
            self.retention_days,
            self.quiet_grace_days,
            self.likely_complete_recheck_days,
            self.auto_export,
            self.auto_append,
            self.split_every_chapters,
            self.log_path.display(),
        );
        std::fs::write(path, content)
            .with_context(|| format!("writing config {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_roundtrips() {
        let dir = std::env::temp_dir().join(format!("crawler-cfg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.ini");

        let mut cfg = Config::default();
        cfg.request_delay_ms = 2000;
        cfg.auto_export = true;
        cfg.split_every_chapters = 100;
        cfg.retention_days = 7;
        cfg.write(&path).unwrap();

        let read = Config::read(&path).unwrap();
        assert_eq!(read.request_delay_ms, 2000);
        assert!(read.auto_export);
        assert!(!read.auto_append);
        assert_eq!(read.split_every_chapters, 100);
        assert_eq!(read.retention_days, 7);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn windows_paths_roundtrip_without_escaping() {
        let dir = std::env::temp_dir().join(format!("crawler-cfg-win-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.ini");

        let mut cfg = Config::default();
        cfg.output_dir = PathBuf::from(r"C:\Users\test\Documents\lightnovels");
        cfg.log_path = PathBuf::from(r"C:\Users\test\AppData\Local\wc\crawler.log");
        cfg.write(&path).unwrap();

        let read = Config::read(&path).unwrap();
        assert_eq!(read.output_dir, PathBuf::from(r"C:\Users\test\Documents\lightnovels"));
        assert_eq!(read.log_path, PathBuf::from(r"C:\Users\test\AppData\Local\wc\crawler.log"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_keys_fall_back_to_defaults() {
        let dir = std::env::temp_dir().join(format!("crawler-cfg-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.ini");
        std::fs::write(&path, "[general]\nrequest_delay_ms = 500\n").unwrap();

        let read = Config::read(&path).unwrap();
        assert_eq!(read.request_delay_ms, 500);
        // Unspecified keys use defaults.
        assert_eq!(read.poll_interval_minutes, Config::default().poll_interval_minutes);
        assert!(!read.auto_export);

        std::fs::remove_dir_all(&dir).ok();
    }
}
