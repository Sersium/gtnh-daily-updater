//! Small shared helpers: hashing, text sniffing, byte formatting, filesystem copies.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// True when the bytes look like mergeable text: valid UTF-8 and no NUL bytes.
///
/// Config files in GTNH are all UTF-8 `.cfg`/`.json`/`.txt`; anything that fails
/// this check (quest icons, `.dat` NBT, jars) gets the pick-one-side treatment
/// instead of a line merge.
pub fn is_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return true;
    }
    let probe = &bytes[..bytes.len().min(8192)];
    if probe.contains(&0) {
        return false;
    }
    std::str::from_utf8(bytes).is_ok()
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

pub fn copy_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(src, dst).with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

pub fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Normalise a zip entry path to forward slashes with no leading `./`.
pub fn norm_path(p: &str) -> String {
    p.replace('\\', "/").trim_start_matches("./").to_string()
}
