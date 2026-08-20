//! Per-instance updater state: which build is installed, and a pristine copy of
//! the pack's text config so the next update has a real merge base.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const STATE_DIR: &str = ".gtnh-updater";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstanceState {
    pub build: Option<u32>,
    pub date: Option<String>,
    pub variant: Option<String>,
    pub artifact_id: Option<u64>,
    pub updated_at: Option<String>,
    /// Directory name of the instance this one was updated from.
    pub updated_from: Option<String>,
}

pub fn state_dir(instance_dir: &Path) -> PathBuf {
    instance_dir.join(STATE_DIR)
}

/// The pristine pack files this tool saved when it created the instance.
pub fn base_zip(instance_dir: &Path) -> PathBuf {
    state_dir(instance_dir).join("base.zip")
}

pub fn state_path(instance_dir: &Path) -> PathBuf {
    state_dir(instance_dir).join("state.json")
}

pub fn load(instance_dir: &Path) -> InstanceState {
    std::fs::read_to_string(state_path(instance_dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save(instance_dir: &Path, state: &InstanceState) -> Result<()> {
    let path = state_path(instance_dir);
    std::fs::create_dir_all(path.parent().unwrap())?;
    std::fs::write(&path, serde_json::to_string_pretty(state)?)
        .with_context(|| format!("write {}", path.display()))
}

/// True when this instance carries a usable pristine snapshot.
pub fn has_base_snapshot(instance_dir: &Path) -> bool {
    base_zip(instance_dir).is_file()
}

/// Every daily zip drops a `changelog from daily <a> to <b>.md` in `.minecraft`.
/// The highest `<b>` across those files is the build the instance is on — the
/// fallback when there is no updater state yet.
pub fn detect_build_from_changelogs(minecraft: &Path) -> Option<u32> {
    let mut best: Option<u32> = None;
    for entry in std::fs::read_dir(minecraft).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix("changelog from daily ") else {
            continue;
        };
        let Some(rest) = rest.strip_suffix(".md") else {
            continue;
        };
        let Some((_, to)) = rest.split_once(" to ") else {
            continue;
        };
        if let Ok(n) = to.trim().parse::<u32>() {
            best = Some(best.map_or(n, |b: u32| b.max(n)));
        }
    }
    best
}

pub fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Plain civil-date conversion; good enough for a "last updated" stamp.
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-from-civil, inverted.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
