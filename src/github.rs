//! GitHub Actions access for the DreamAssemblerXXL daily modpack builds.
//!
//! Daily artifacts are named like `GTNH-daily-2026-08-19+690-mmcprism-java17-26.zip`.
//! The repo-level artifacts endpoint returns them newest-first with their run id,
//! so a single page covers the last ~16 builds.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

pub const OWNER: &str = "GTNewHorizons";
pub const REPO: &str = "DreamAssemblerXXL";

/// The Prism/MultiMC client variant we know how to install.
pub const VARIANT_JAVA17_26: &str = "mmcprism-java17-26";
pub const VARIANT_JAVA8: &str = "mmcprism-java8";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyArtifact {
    pub id: u64,
    pub run_id: u64,
    pub name: String,
    pub size: u64,
    /// `2026-08-19`
    pub date: String,
    /// `690`
    pub build: u32,
    /// `mmcprism-java17-26`
    pub variant: String,
}

impl DailyArtifact {
    /// Parse `GTNH-daily-<date>+<build>-<variant>.zip`. Returns `None` for
    /// anything else in the artifact list (server packs, manifests, bundles).
    pub fn parse(id: u64, run_id: u64, name: &str, size: u64) -> Option<Self> {
        let stem = name.strip_suffix(".zip")?;
        let rest = stem.strip_prefix("GTNH-daily-")?;
        let (date, rest) = rest.split_once('+')?;
        let (build, variant) = rest.split_once('-')?;
        let build: u32 = build.parse().ok()?;
        if !variant.starts_with("mmcprism") {
            return None;
        }
        Some(Self {
            id,
            run_id,
            name: name.to_string(),
            size,
            date: date.to_string(),
            build,
            variant: variant.to_string(),
        })
    }

    pub fn label(&self) -> String {
        format!("daily {} ({})", self.build, self.date)
    }
}

#[derive(Deserialize)]
struct ArtifactsPage {
    artifacts: Vec<RawArtifact>,
}

#[derive(Deserialize)]
struct RawArtifact {
    id: u64,
    name: String,
    size_in_bytes: u64,
    expired: bool,
    workflow_run: Option<RawRun>,
}

#[derive(Deserialize)]
struct RawRun {
    id: u64,
}

pub struct Gh {
    agent: ureq::Agent,
    /// Agent that stops at the first redirect so we can read the signed blob URL.
    no_redirect: ureq::Agent,
    pub token: Option<String>,
}

impl Gh {
    pub fn new(token: Option<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(300)))
            .user_agent("gtnh-updater")
            .build()
            .new_agent();
        let no_redirect = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .user_agent("gtnh-updater")
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self {
            agent,
            no_redirect,
            token: token.filter(|t| !t.trim().is_empty()),
        }
    }

    /// Look for a token in the environment, then fall back to the `gh` CLI and
    /// its config file. Artifact downloads need auth even on public repos.
    pub fn discover_token() -> Option<String> {
        for key in ["GH_TOKEN", "GITHUB_TOKEN"] {
            if let Ok(v) = std::env::var(key) {
                if !v.trim().is_empty() {
                    return Some(v.trim().to_string());
                }
            }
        }
        if let Ok(out) = std::process::Command::new("gh")
            .args(["auth", "token"])
            .output()
        {
            if out.status.success() {
                let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !t.is_empty() {
                    return Some(t);
                }
            }
        }
        // hosts.yml: a flat `github.com:` block with an `oauth_token:` line.
        let hosts = dirs::config_dir()?.join("gh/hosts.yml");
        let text = std::fs::read_to_string(hosts).ok()?;
        for line in text.lines() {
            let line = line.trim();
            if let Some(v) = line.strip_prefix("oauth_token:") {
                let v = v.trim().trim_matches('"');
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        None
    }

    fn api_get(&self, url: &str) -> Result<String> {
        let mut req = self
            .agent
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let mut resp = req.call().with_context(|| format!("GET {url}"))?;
        Ok(resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_string()?)
    }

    /// One page (100 entries) of the repo's artifacts, newest first, keeping only
    /// non-expired Prism client packs.
    pub fn artifacts_page(&self, page: u32) -> Result<Vec<DailyArtifact>> {
        let url = format!(
            "https://api.github.com/repos/{OWNER}/{REPO}/actions/artifacts?per_page=100&page={page}"
        );
        let body = self.api_get(&url)?;
        let page: ArtifactsPage =
            serde_json::from_str(&body).context("parse artifacts response")?;
        Ok(page
            .artifacts
            .into_iter()
            .filter(|a| !a.expired)
            .filter_map(|a| {
                let run = a.workflow_run.as_ref().map(|r| r.id).unwrap_or(0);
                DailyArtifact::parse(a.id, run, &a.name, a.size_in_bytes)
            })
            .collect())
    }

    /// The newest available builds for `variant`, newest first.
    pub fn recent_builds(&self, variant: &str, pages: u32) -> Result<Vec<DailyArtifact>> {
        let mut out: Vec<DailyArtifact> = Vec::new();
        for page in 1..=pages.max(1) {
            let batch = self.artifacts_page(page)?;
            if batch.is_empty() && page > 1 {
                break;
            }
            out.extend(batch.into_iter().filter(|a| a.variant == variant));
        }
        out.sort_by_key(|a| std::cmp::Reverse(a.build));
        out.dedup_by(|a, b| a.build == b.build);
        Ok(out)
    }

    /// Walk back through the artifact list looking for one specific build number.
    /// Used to recover the pristine files of the version currently installed.
    pub fn find_build(
        &self,
        build: u32,
        variant: &str,
        max_pages: u32,
    ) -> Result<Option<DailyArtifact>> {
        for page in 1..=max_pages.max(1) {
            let batch = self.artifacts_page(page)?;
            if batch.is_empty() {
                break;
            }
            let oldest = batch.iter().map(|a| a.build).min().unwrap_or(u32::MAX);
            if let Some(found) = batch
                .into_iter()
                .find(|a| a.build == build && a.variant == variant)
            {
                return Ok(Some(found));
            }
            // Artifacts come back newest-first, so once the page is entirely
            // older than the target there is no point looking further.
            if oldest < build {
                break;
            }
        }
        Ok(None)
    }

    /// Resolve the artifact download endpoint to its signed storage URL without
    /// pulling the body. That URL supports byte ranges; the API endpoint does not.
    pub fn resolve_download_url(&self, artifact_id: u64) -> Result<String> {
        let url = format!(
            "https://api.github.com/repos/{OWNER}/{REPO}/actions/artifacts/{artifact_id}/zip"
        );
        let mut req = self
            .no_redirect
            .get(&url)
            .header("Accept", "application/vnd.github+json");
        if let Some(t) = &self.token {
            req = req.header("Authorization", format!("Bearer {t}"));
        }
        let resp = req.call().with_context(|| format!("resolve {url}"))?;
        let status = resp.status().as_u16();
        if !(300..400).contains(&status) {
            if status == 401 || status == 403 {
                bail!(
                    "GitHub rejected the artifact request (HTTP {status}). \
                     Artifact downloads require a token with `repo` or `actions:read` scope."
                );
            }
            bail!("expected a redirect to the artifact storage, got HTTP {status}");
        }
        resp.headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("artifact redirect had no Location header"))
    }

    pub fn agent(&self) -> &ureq::Agent {
        &self.agent
    }

    /// Stream an artifact to disk, reporting `(downloaded, total)` as it goes.
    pub fn download_to(
        &self,
        url: &str,
        dest: &Path,
        mut progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<()> {
        let mut resp = self
            .agent
            .get(url)
            .call()
            .with_context(|| "start artifact download")?;
        let total = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = dest.with_extension("part");
        let mut file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        let mut reader = resp.body_mut().with_config().limit(u64::MAX).reader();
        let mut buf = vec![0u8; 1024 * 1024];
        let mut done: u64 = 0;
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n])?;
            done += n as u64;
            if !progress(done, total) {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                bail!("download cancelled");
            }
        }
        file.flush()?;
        drop(file);
        std::fs::rename(&tmp, dest)?;
        Ok(())
    }
}
