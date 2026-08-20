//! Background jobs. Everything slow (network, zip, disk walking) runs off the UI
//! thread and reports back over a channel.

use crate::github::{DailyArtifact, Gh};
use crate::httpzip::HttpRangeReader;
use crate::pack::Pack;
use crate::plan::{self, BaseSource, NoBase, Plan, RemoteBase, SnapshotBase};
use crate::prism::Instance;
use crate::state::{self, InstanceState};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Query,
    Download,
    Extract,
    Snapshot,
    Scan,
    Apply,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Query => "Checking for builds",
            Phase::Download => "Downloading pack",
            Phase::Extract => "Extracting",
            Phase::Snapshot => "Saving merge base",
            Phase::Scan => "Comparing files",
            Phase::Apply => "Writing new instance",
        }
    }
}

pub enum Evt {
    Progress {
        phase: Phase,
        frac: f32,
        msg: String,
    },
    Builds(Vec<DailyArtifact>),
    Prepared(Box<Prepared>),
    Applied(PathBuf),
    Failed(String),
}

pub struct Prepared {
    pub plan: Plan,
    pub staging: PathBuf,
    pub artifact: DailyArtifact,
    pub instance: Instance,
    pub instances_root: PathBuf,
    pub final_name: String,
    pub display_name: String,
    pub download_path: Option<PathBuf>,
    /// Human-readable note about where the merge base came from.
    pub base_note: String,
    pub old_build: Option<u32>,
}

#[derive(Clone)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

fn send(tx: &Sender<Evt>, evt: Evt) {
    let _ = tx.send(evt);
}

fn progress(tx: &Sender<Evt>, phase: Phase, frac: f32, msg: impl Into<String>) {
    send(
        tx,
        Evt::Progress {
            phase,
            frac,
            msg: msg.into(),
        },
    );
}

pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("gtnh-updater")
}

/// Fetch the list of installable daily builds.
pub fn spawn_fetch_builds(tx: Sender<Evt>, token: Option<String>, variant: String) {
    std::thread::spawn(move || {
        progress(
            &tx,
            Phase::Query,
            0.3,
            "Asking GitHub for recent artifacts…",
        );
        let gh = Gh::new(token);
        match gh.recent_builds(&variant, 1) {
            Ok(builds) if builds.is_empty() => send(
                &tx,
                Evt::Failed(format!(
                    "No non-expired `{variant}` artifacts were found. \
                     GitHub keeps daily artifacts for a limited time."
                )),
            ),
            Ok(builds) => send(&tx, Evt::Builds(builds)),
            Err(e) => send(&tx, Evt::Failed(format!("{e:#}"))),
        }
    });
}

pub struct PrepareJob {
    pub token: Option<String>,
    pub instance: Instance,
    pub instances_root: PathBuf,
    pub artifact: DailyArtifact,
    pub final_name: String,
    pub display_name: String,
    pub cancel: Cancel,
}

pub fn spawn_prepare(tx: Sender<Evt>, job: PrepareJob) {
    std::thread::spawn(move || {
        let staging = job.instances_root.join(format!("{}.part", job.final_name));
        match prepare(&tx, job, &staging) {
            Ok(prepared) => send(&tx, Evt::Prepared(Box::new(prepared))),
            Err(e) => {
                // Cancelled or failed halfway: do not leave a half-instance behind.
                discard_staging(&staging);
                send(&tx, Evt::Failed(format!("{e:#}")));
            }
        }
    });
}

fn prepare(tx: &Sender<Evt>, job: PrepareJob, staging: &Path) -> Result<Prepared> {
    let gh = Gh::new(job.token.clone());
    if staging.exists() {
        std::fs::remove_dir_all(staging)
            .with_context(|| format!("clear stale staging dir {}", staging.display()))?;
    }

    // --- download --------------------------------------------------------
    let zip_path = cache_dir().join(&job.artifact.name);
    let already_have = std::fs::metadata(&zip_path)
        .map(|m| m.len() == job.artifact.size)
        .unwrap_or(false);
    if already_have {
        progress(tx, Phase::Download, 1.0, "Using previously downloaded pack");
    } else {
        let url = gh.resolve_download_url(job.artifact.id)?;
        let cancel = job.cancel.clone();
        let tx2 = tx.clone();
        let total_hint = job.artifact.size;
        gh.download_to(&url, &zip_path, move |done, total| {
            let total = if total > 0 { total } else { total_hint };
            let frac = if total > 0 {
                done as f32 / total as f32
            } else {
                0.0
            };
            progress(
                &tx2,
                Phase::Download,
                frac,
                format!(
                    "{} of {}",
                    crate::util::human_bytes(done),
                    crate::util::human_bytes(total)
                ),
            );
            !cancel.is_cancelled()
        })?;
    }

    // --- extract ---------------------------------------------------------
    let file =
        std::fs::File::open(&zip_path).with_context(|| format!("open {}", zip_path.display()))?;
    let mut pack = Pack::open(std::io::BufReader::new(file))?;
    {
        let cancel = job.cancel.clone();
        let tx2 = tx.clone();
        pack.extract_all(staging, move |done, total| {
            progress(
                &tx2,
                Phase::Extract,
                done as f32 / total.max(1) as f32,
                format!("{done} / {total} files"),
            );
            !cancel.is_cancelled()
        })?;
    }
    let new_mc = staging.join(".minecraft");

    // --- snapshot the pristine pack for the *next* update ----------------
    {
        let cancel = job.cancel.clone();
        let tx2 = tx.clone();
        plan::write_base_snapshot(&new_mc, &state::base_zip(staging), move |done, total| {
            progress(
                &tx2,
                Phase::Snapshot,
                done as f32 / total.max(1) as f32,
                format!("{done} / {total}"),
            );
            !cancel.is_cancelled()
        })?;
    }

    // --- find the pristine files of the version being replaced -----------
    let old_build = state::load(&job.instance.dir)
        .build
        .or_else(|| state::detect_build_from_changelogs(&job.instance.minecraft()));

    progress(tx, Phase::Scan, 0.0, "Locating the version you are on…");
    let mut snapshot_base;
    let mut remote_base;
    let mut no_base = NoBase::default();
    let base_note;
    let base: &mut dyn BaseSource = if state::has_base_snapshot(&job.instance.dir) {
        snapshot_base = SnapshotBase::open(&state::base_zip(&job.instance.dir))?;
        base_note = "Merge base: snapshot saved by a previous update.".to_string();
        &mut snapshot_base
    } else if let Some(build) = old_build {
        match remote_base_for(&gh, build, &job.artifact.variant, job.token.clone()) {
            Ok(Some(rb)) => {
                remote_base = rb;
                base_note =
                    format!("Merge base: daily {build} artifact, read on demand over the network.");
                &mut remote_base
            }
            Ok(None) => {
                base_note = format!(
                    "No artifact for daily {build} is still on GitHub, so every changed \
                     file needs a manual choice."
                );
                &mut no_base
            }
            Err(e) => {
                base_note = format!("Could not read the daily {build} artifact ({e}).");
                &mut no_base
            }
        }
    } else {
        base_note = "Could not tell which daily build this instance is on, so every changed file \
             needs a manual choice."
            .to_string();
        &mut no_base
    };

    let plan = {
        let cancel = job.cancel.clone();
        let tx2 = tx.clone();
        plan::build(
            plan::BuildInput {
                old_mc: &job.instance.minecraft(),
                new_mc: &new_mc,
            },
            base,
            move |done, total, label| {
                progress(
                    &tx2,
                    Phase::Scan,
                    done as f32 / total.max(1) as f32,
                    label.to_string(),
                );
                !cancel.is_cancelled()
            },
        )?
    };

    Ok(Prepared {
        plan,
        staging: staging.to_path_buf(),
        artifact: job.artifact,
        instance: job.instance,
        instances_root: job.instances_root,
        final_name: job.final_name,
        display_name: job.display_name,
        download_path: Some(zip_path),
        base_note,
        old_build,
    })
}

fn remote_base_for(
    gh: &Gh,
    build: u32,
    variant: &str,
    token: Option<String>,
) -> Result<Option<RemoteBase<HttpRangeReader>>> {
    let Some(artifact) = gh.find_build(build, variant, 8)? else {
        return Ok(None);
    };
    let url = gh.resolve_download_url(artifact.id)?;
    let artifact_id = artifact.id;
    let refresh = Box::new(move || {
        let gh = Gh::new(token.clone());
        gh.resolve_download_url(artifact_id)
    });
    let reader = HttpRangeReader::new(gh.agent().clone(), url, Some(refresh))?;
    let pack = Pack::open(reader)?;
    Ok(Some(RemoteBase::new(pack)))
}

pub struct ApplyJob {
    pub plan: Box<Plan>,
    pub prepared_staging: PathBuf,
    pub old_instance: PathBuf,
    pub instances_root: PathBuf,
    pub final_name: String,
    pub display_name: String,
    pub state: InstanceState,
    pub download_path: Option<PathBuf>,
    pub keep_download: bool,
    pub cancel: Cancel,
}

pub fn spawn_apply(tx: Sender<Evt>, job: ApplyJob) {
    std::thread::spawn(move || match run_apply(&tx, job) {
        Ok(dir) => send(&tx, Evt::Applied(dir)),
        Err(e) => send(&tx, Evt::Failed(format!("{e:#}"))),
    });
}

fn run_apply(tx: &Sender<Evt>, job: ApplyJob) -> Result<PathBuf> {
    let cancel = job.cancel.clone();
    let tx2 = tx.clone();
    let dir = crate::apply::apply(
        crate::apply::ApplyInput {
            old_instance: &job.old_instance,
            staging: &job.prepared_staging,
            final_name: &job.final_name,
            instances_root: &job.instances_root,
            display_name: &job.display_name,
            state: job.state,
        },
        &job.plan,
        move |done, total, label| {
            progress(
                &tx2,
                Phase::Apply,
                done as f32 / total.max(1) as f32,
                label.to_string(),
            );
            !cancel.is_cancelled()
        },
    )?;
    if !job.keep_download {
        if let Some(zip) = &job.download_path {
            let _ = std::fs::remove_file(zip);
        }
    }
    Ok(dir)
}

/// Remove a staged instance that was abandoned.
pub fn discard_staging(staging: &Path) {
    let _ = std::fs::remove_dir_all(staging);
}
