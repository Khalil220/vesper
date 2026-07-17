//! Background-service management: registering the periodic `sync` job with the
//! OS scheduler.
//!
//! Windows (Task Scheduler, via `schtasks`) is implemented. Other platforms are
//! stubbed behind the same trait so a systemd user unit / launchd agent can be
//! added later without touching the CLI. Task Scheduler is chosen over a real
//! Windows Service because the sync is a scheduled invocation, not a resident
//! daemon (see DESIGN.md), and a per-user task needs no elevation.

#![allow(dead_code)] // Some fields/impls are platform-gated.

use std::path::Path;

use anyhow::Result;

/// Name of the scheduled task / service.
pub const TASK_NAME: &str = "WebnovelCrawlerSync";

pub struct ServiceStatus {
    pub installed: bool,
    pub detail: String,
}

/// Platform-agnostic install/uninstall/status for the background sync job.
pub trait ServiceManager {
    fn install(&self, exe: &Path, interval_minutes: u32) -> Result<()>;
    fn uninstall(&self) -> Result<()>;
    fn status(&self) -> Result<ServiceStatus>;
}

/// The service manager for the current platform.
pub fn manager() -> Result<Box<dyn ServiceManager>> {
    #[cfg(windows)]
    {
        Ok(Box::new(windows::WindowsTaskScheduler))
    }
    #[cfg(not(windows))]
    {
        anyhow::bail!("background-service install is only implemented on Windows so far")
    }
}

#[cfg(windows)]
mod windows {
    use std::path::Path;
    use std::process::Command;

    use anyhow::{bail, Result};

    use super::{ServiceManager, ServiceStatus, TASK_NAME};

    pub struct WindowsTaskScheduler;

    impl ServiceManager for WindowsTaskScheduler {
        fn install(&self, exe: &Path, interval_minutes: u32) -> Result<()> {
            // /TR is the command to run each interval: the quoted exe path plus
            // the `sync` subcommand. Built as one argv element so std handles the
            // quoting; schtasks then parses the embedded quotes.
            let tr = format!("\"{}\" sync", exe.display());
            let out = Command::new("schtasks")
                .args([
                    "/Create",
                    "/TN",
                    TASK_NAME,
                    "/TR",
                    &tr,
                    "/SC",
                    "MINUTE",
                    "/MO",
                    &interval_minutes.to_string(),
                    "/F",
                ])
                .output()?;
            if !out.status.success() {
                bail!(
                    "schtasks /Create failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        }

        fn uninstall(&self) -> Result<()> {
            let out = Command::new("schtasks")
                .args(["/Delete", "/TN", TASK_NAME, "/F"])
                .output()?;
            if !out.status.success() {
                bail!(
                    "schtasks /Delete failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        }

        fn status(&self) -> Result<ServiceStatus> {
            let out = Command::new("schtasks")
                .args(["/Query", "/TN", TASK_NAME])
                .output()?;
            Ok(ServiceStatus {
                installed: out.status.success(),
                detail: if out.status.success() {
                    String::from_utf8_lossy(&out.stdout).trim().to_string()
                } else {
                    "not installed".to_string()
                },
            })
        }
    }
}
