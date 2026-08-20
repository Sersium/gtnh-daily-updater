//! Reading a GTNH daily pack zip, locally or over HTTP ranges.
//!
//! Every artifact wraps the instance in a single top-level folder
//! (`GT New Horizons daily/`), so paths are re-rooted to the instance layout:
//! `.minecraft/config/…`, `instance.cfg`, `patches/…`, `mmc-pack.json`.

use crate::util::norm_path;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::Path;

pub struct Pack<R: Read + Seek> {
    zip: zip::ZipArchive<R>,
    /// Zip index for each re-rooted path. Directories are excluded.
    index: BTreeMap<String, usize>,
}

impl<R: Read + Seek> Pack<R> {
    pub fn open(reader: R) -> Result<Self> {
        let zip = zip::ZipArchive::new(reader).context("read pack zip directory")?;
        let mut names: Vec<(String, usize)> = Vec::new();
        for i in 0..zip.len() {
            let name = zip.name_for_index(i).map(norm_path);
            if let Some(name) = name {
                names.push((name, i));
            }
        }
        let root = detect_root(names.iter().map(|(n, _)| n.as_str()))?;
        let mut index = BTreeMap::new();
        for (name, i) in names {
            if name.ends_with('/') {
                continue;
            }
            let rel = match &root {
                Some(prefix) => match name.strip_prefix(prefix.as_str()) {
                    Some(r) => r.to_string(),
                    None => continue,
                },
                None => name,
            };
            if rel.is_empty() {
                continue;
            }
            index.insert(rel, i);
        }
        if index.is_empty() {
            bail!("pack zip contained no files");
        }
        Ok(Self { zip, index })
    }

    /// Re-rooted paths of every file in the pack.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(|s| s.as_str())
    }

    pub fn read(&mut self, rel: &str) -> Result<Vec<u8>> {
        let Some(&i) = self.index.get(rel) else {
            bail!("{rel} is not in the pack");
        };
        let mut f = self.zip.by_index(i)?;
        let mut out = Vec::with_capacity(f.size() as usize);
        f.read_to_end(&mut out)?;
        Ok(out)
    }

    /// Extract the whole pack into `dest`, re-rooted. `progress` receives
    /// `(files_done, files_total)` and returns false to abort.
    pub fn extract_all(
        &mut self,
        dest: &Path,
        mut progress: impl FnMut(usize, usize) -> bool,
    ) -> Result<()> {
        let entries: Vec<(String, usize)> =
            self.index.iter().map(|(k, v)| (k.clone(), *v)).collect();
        let total = entries.len();
        std::fs::create_dir_all(dest)?;
        for (n, (rel, idx)) in entries.into_iter().enumerate() {
            let out_path = safe_join(dest, &rel)?;
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = self.zip.by_index(idx)?;
            let mut out = std::io::BufWriter::new(
                std::fs::File::create(&out_path)
                    .with_context(|| format!("create {}", out_path.display()))?,
            );
            std::io::copy(&mut f, &mut out)?;
            if n % 64 == 0 && !progress(n, total) {
                bail!("extraction cancelled");
            }
        }
        progress(total, total);
        Ok(())
    }
}

/// Reject entries that would escape `base` via `..` or absolute paths.
fn safe_join(base: &Path, rel: &str) -> Result<std::path::PathBuf> {
    let mut out = base.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            bail!("pack entry {rel} tried to escape the target directory");
        }
        out.push(part);
    }
    Ok(out)
}

/// If every entry sits under one top-level folder, return it (with trailing slash).
fn detect_root<'a>(names: impl Iterator<Item = &'a str>) -> Result<Option<String>> {
    let mut root: Option<String> = None;
    for name in names {
        let Some((first, _)) = name.split_once('/') else {
            // A file at the top level means the pack is not wrapped.
            return Ok(None);
        };
        match &root {
            None => root = Some(first.to_string()),
            Some(existing) if existing == first => {}
            Some(_) => return Ok(None),
        }
    }
    Ok(root.map(|r| format!("{r}/")))
}
