//! Turning "old instance" + "new pack" + "what the old pack shipped" into a
//! concrete list of things to do.
//!
//! The rule for every path is the same three-way question git asks: if you did
//! not touch a file, take the pack's version; if the pack did not touch it, keep
//! yours; if you both did, merge the lines and only stop for real collisions.

use crate::merge::{self, MergeOutcome, TextMerge};
use crate::mods::{self, ModPlan};
use crate::util;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Never carried over from the old instance: noise, or updater bookkeeping.
const SKIP_CARRY: &[&str] = &[
    "logs",
    "crash-reports",
    crate::state::STATE_DIR,
    ".fabric",
    "usercache.json",
    "usernamecache.json",
];

/// Files bigger than this are not snapshotted as a merge base; a config that
/// large is not something anyone hand-edits.
const MAX_BASE_FILE: u64 = 32 * 1024 * 1024;

const MANIFEST: &str = "manifest.tsv";
const FILES_PREFIX: &str = "files/";

#[derive(Debug, Clone)]
pub struct ConflictFile {
    pub rel: String,
    pub merge: TextMerge,
    /// The original version could not be recovered, so this is a straight
    /// yours-or-theirs choice rather than a real three-way merge.
    pub no_base: bool,
    pub reviewed: bool,
}

#[derive(Debug, Clone)]
pub struct BinaryConflict {
    pub rel: String,
    pub ours_size: u64,
    pub theirs_size: u64,
    pub take_pack: bool,
}

/// A top-level folder or file that only exists in the old instance.
#[derive(Debug, Clone)]
pub struct CarryGroup {
    pub name: String,
    pub files: Vec<String>,
    pub bytes: u64,
    pub enabled: bool,
}

#[derive(Debug, Default)]
pub struct Plan {
    /// Merged cleanly; write the result over the extracted pack file.
    pub auto_merged: Vec<(String, String)>,
    /// Needs a decision from the user.
    pub conflicts: Vec<ConflictFile>,
    pub binary_conflicts: Vec<BinaryConflict>,
    /// You edited it, the pack did not: copy the old file across.
    pub keep_yours: Vec<String>,
    /// The pack changed it and you did not: leave the extracted file alone.
    pub take_pack: usize,
    /// Byte-identical on both sides.
    pub identical: usize,
    pub carry: Vec<CarryGroup>,
    pub mods: ModPlan,
    /// Set when no pristine copy of the installed version was available.
    pub base_missing: bool,
}

impl Plan {
    pub fn unresolved(&self) -> usize {
        self.conflicts.iter().filter(|c| !c.reviewed).count()
    }
    pub fn carry_bytes(&self) -> u64 {
        self.carry
            .iter()
            .filter(|g| g.enabled)
            .map(|g| g.bytes)
            .sum()
    }
}

/// Where the pristine files of the currently-installed version come from.
pub trait BaseSource {
    /// Every path the old pack shipped, relative to `.minecraft`.
    fn paths(&self) -> &BTreeMap<String, u64>;

    /// CRC32 of the pristine file, when it is known without fetching anything.
    ///
    /// This answers "did anyone actually change this?" for files whose contents
    /// are not worth keeping or downloading — textures, quest icons, sounds —
    /// which is most of a GTNH pack by volume.
    fn digest(&self, _rel: &str) -> Option<u32> {
        None
    }

    /// The pristine bytes of one path, if obtainable.
    fn read(&mut self, rel: &str) -> Option<Vec<u8>>;
}

/// No pristine copy available; every difference becomes a manual choice.
#[derive(Default)]
pub struct NoBase {
    empty: BTreeMap<String, u64>,
}

impl BaseSource for NoBase {
    fn paths(&self) -> &BTreeMap<String, u64> {
        &self.empty
    }
    fn read(&mut self, _rel: &str) -> Option<Vec<u8>> {
        None
    }
}

/// A pristine snapshot this tool wrote during a previous update: one zip holding
/// the pack's text files plus a manifest of everything the pack contained.
pub struct SnapshotBase {
    zip: zip::ZipArchive<std::io::BufReader<std::fs::File>>,
    paths: BTreeMap<String, u64>,
    digests: BTreeMap<String, u32>,
}

impl SnapshotBase {
    pub fn open(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("open snapshot {}", path.display()))?;
        let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file))
            .with_context(|| format!("read snapshot {}", path.display()))?;
        let mut paths = BTreeMap::new();
        let mut digests = BTreeMap::new();
        if let Ok(mut entry) = zip.by_name(MANIFEST) {
            let mut text = String::new();
            use std::io::Read;
            if entry.read_to_string(&mut text).is_ok() {
                for line in text.lines() {
                    // rel \t size \t crc32
                    let mut parts = line.rsplitn(3, '\t');
                    let (Some(crc), Some(size), Some(rel)) =
                        (parts.next(), parts.next(), parts.next())
                    else {
                        continue;
                    };
                    paths.insert(rel.to_string(), size.parse().unwrap_or(0));
                    if let Ok(crc) = crc.parse() {
                        digests.insert(rel.to_string(), crc);
                    }
                }
            }
        }
        Ok(Self {
            zip,
            paths,
            digests,
        })
    }
}

impl BaseSource for SnapshotBase {
    fn paths(&self) -> &BTreeMap<String, u64> {
        &self.paths
    }
    fn digest(&self, rel: &str) -> Option<u32> {
        self.digests.get(rel).copied()
    }
    fn read(&mut self, rel: &str) -> Option<Vec<u8>> {
        use std::io::Read;
        let mut entry = self.zip.by_name(&format!("{FILES_PREFIX}{rel}")).ok()?;
        let mut out = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut out).ok()?;
        Some(out)
    }
}

/// The old build's artifact, read over HTTP ranges so only the handful of files
/// that actually differ get downloaded.
pub struct RemoteBase<R: std::io::Read + std::io::Seek> {
    pack: crate::pack::Pack<R>,
    paths: BTreeMap<String, u64>,
}

impl<R: std::io::Read + std::io::Seek> RemoteBase<R> {
    pub fn new(pack: crate::pack::Pack<R>) -> Self {
        let paths = pack
            .paths()
            .filter_map(|p| p.strip_prefix(".minecraft/").map(|r| (r.to_string(), 0u64)))
            .collect();
        Self { pack, paths }
    }
}

impl<R: std::io::Read + std::io::Seek> BaseSource for RemoteBase<R> {
    fn paths(&self) -> &BTreeMap<String, u64> {
        &self.paths
    }
    fn read(&mut self, rel: &str) -> Option<Vec<u8>> {
        self.pack.read(&format!(".minecraft/{rel}")).ok()
    }
}

fn is_mod_file(rel: &str) -> bool {
    rel.starts_with("mods/")
        && (rel.ends_with(".jar") || rel.ends_with(".litemod") || rel.ends_with(".jar.disabled"))
}

/// Loose files directly in `.minecraft` (`options.txt`, `servers.dat`, …) are
/// collected into one entry instead of a row each.
pub const LOOSE_FILES_GROUP: &str = "(loose files)";

fn carry_group_name(rel: &str) -> &str {
    match rel.split_once('/') {
        Some((dir, _)) => dir,
        None => LOOSE_FILES_GROUP,
    }
}

/// Index every file under `root`, keyed by path relative to it.
pub fn index_tree(root: &Path) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel = util::norm_path(&rel.to_string_lossy());
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.insert(rel, size);
    }
    out
}

pub struct BuildInput<'a> {
    pub old_mc: &'a Path,
    pub new_mc: &'a Path,
}

/// Build the plan. `progress` is called with `(done, total, label)`.
pub fn build(
    input: BuildInput<'_>,
    base: &mut dyn BaseSource,
    mut progress: impl FnMut(usize, usize, &str) -> bool,
) -> Result<Plan> {
    let ours = index_tree(input.old_mc);
    let theirs = index_tree(input.new_mc);

    let mut plan = Plan {
        base_missing: base.paths().is_empty(),
        ..Default::default()
    };

    // --- mods ------------------------------------------------------------
    let filter_mods = |m: &BTreeMap<String, u64>| -> BTreeMap<String, u64> {
        m.iter()
            .filter(|(k, _)| is_mod_file(k))
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    };
    plan.mods = mods::plan(
        &filter_mods(base.paths()),
        &filter_mods(&ours),
        &filter_mods(&theirs),
    );

    // --- files present on both sides -------------------------------------
    let shared: Vec<&String> = theirs.keys().filter(|k| ours.contains_key(*k)).collect();
    let total = shared.len();
    for (n, rel) in shared.iter().enumerate() {
        if n % 128 == 0 && !progress(n, total, rel) {
            anyhow::bail!("cancelled");
        }
        let rel = rel.as_str();
        if is_mod_file(rel) {
            continue; // jars are decided by the mod plan
        }
        let our_path = input.old_mc.join(rel);
        let their_path = input.new_mc.join(rel);
        if files_equal(&our_path, &their_path)? {
            plan.identical += 1;
            continue;
        }

        // A cheap CRC check settles most files without reading the original at
        // all, which matters when the original lives behind HTTP range requests.
        if let Some(base_crc) = base.digest(rel) {
            if crc32_file(&our_path)? == base_crc {
                plan.take_pack += 1;
                continue;
            }
            if crc32_file(&their_path)? == base_crc {
                plan.keep_yours.push(rel.to_string());
                continue;
            }
        }

        let Some(base_bytes) = base.read(rel) else {
            let our_bytes = std::fs::read(&our_path)?;
            let their_bytes = std::fs::read(&their_path)?;
            if util::is_text(&our_bytes) && util::is_text(&their_bytes) {
                plan.conflicts.push(ConflictFile {
                    rel: rel.to_string(),
                    merge: whole_file_choice(
                        &String::from_utf8_lossy(&our_bytes),
                        &String::from_utf8_lossy(&their_bytes),
                    ),
                    no_base: true,
                    reviewed: false,
                });
            } else {
                plan.binary_conflicts.push(BinaryConflict {
                    rel: rel.to_string(),
                    ours_size: our_bytes.len() as u64,
                    theirs_size: their_bytes.len() as u64,
                    take_pack: true,
                });
            }
            continue;
        };

        let our_bytes = std::fs::read(&our_path)?;
        if our_bytes == base_bytes {
            plan.take_pack += 1; // untouched by you: the extracted file already wins
            continue;
        }
        let their_bytes = std::fs::read(&their_path)?;
        if their_bytes == base_bytes {
            plan.keep_yours.push(rel.to_string());
            continue;
        }

        if util::is_text(&base_bytes) && util::is_text(&our_bytes) && util::is_text(&their_bytes) {
            let base_s = String::from_utf8_lossy(&base_bytes);
            let our_s = String::from_utf8_lossy(&our_bytes);
            let their_s = String::from_utf8_lossy(&their_bytes);
            match merge::three_way(&base_s, &our_s, &their_s) {
                MergeOutcome::Clean(text) => plan.auto_merged.push((rel.to_string(), text)),
                MergeOutcome::Conflicted(m) => plan.conflicts.push(ConflictFile {
                    rel: rel.to_string(),
                    merge: m,
                    no_base: false,
                    reviewed: false,
                }),
            }
        } else {
            plan.binary_conflicts.push(BinaryConflict {
                rel: rel.to_string(),
                ours_size: our_bytes.len() as u64,
                theirs_size: their_bytes.len() as u64,
                take_pack: true,
            });
        }
    }

    // --- files only the old instance has ---------------------------------
    let mut groups: BTreeMap<String, CarryGroup> = BTreeMap::new();
    for (rel, size) in &ours {
        if theirs.contains_key(rel) {
            continue;
        }
        if is_mod_file(rel) {
            continue; // the mod plan decides jars
        }
        let group = carry_group_name(rel).to_string();
        if SKIP_CARRY.contains(&group.as_str()) {
            continue;
        }
        let g = groups.entry(group.clone()).or_insert_with(|| CarryGroup {
            name: group,
            files: Vec::new(),
            bytes: 0,
            enabled: true,
        });
        g.files.push(rel.clone());
        g.bytes += size;
    }
    plan.carry = groups.into_values().collect();
    plan.carry.sort_by_key(|g| std::cmp::Reverse(g.bytes));

    progress(total, total, "done");
    Ok(plan)
}

/// A file that differs with no known original: one conflict covering everything.
fn whole_file_choice(ours: &str, theirs: &str) -> TextMerge {
    TextMerge {
        segments: vec![merge::Segment::Conflict {
            ours: ours.to_string(),
            base: String::new(),
            theirs: theirs.to_string(),
        }],
        hunks: vec![merge::Hunk {
            ours: ours.to_string(),
            base: String::new(),
            theirs: theirs.to_string(),
            choice: merge::Choice::Theirs,
            custom: String::new(),
        }],
        manual: None,
    }
}

fn files_equal(a: &Path, b: &Path) -> Result<bool> {
    let (ma, mb) = (std::fs::metadata(a)?, std::fs::metadata(b)?);
    if ma.len() != mb.len() {
        return Ok(false);
    }
    let mut fa = std::io::BufReader::new(std::fs::File::open(a)?);
    let mut fb = std::io::BufReader::new(std::fs::File::open(b)?);
    let mut ba = [0u8; 64 * 1024];
    let mut bb = [0u8; 64 * 1024];
    loop {
        let na = read_full(&mut fa, &mut ba)?;
        let nb = read_full(&mut fb, &mut bb)?;
        if na != nb || ba[..na] != bb[..nb] {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
    }
}

/// CRC32 of a file, streamed. Same checksum zips use, and fast enough to run
/// over the whole pack while snapshotting.
fn crc32_file(path: &Path) -> Result<u32> {
    use std::io::Read;
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; 128 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
}

fn read_full(r: &mut impl std::io::Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Save the pack's pristine text files so the *next* update has a merge base,
/// as one compressed archive. Called on the freshly extracted instance, before
/// any merged output is written over it.
pub fn write_base_snapshot(
    new_mc: &Path,
    dest: &Path,
    mut progress: impl FnMut(usize, usize) -> bool,
) -> Result<()> {
    use std::io::Write;

    let files = index_tree(new_mc);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = dest.with_extension("zip.part");
    let mut zw = zip::ZipWriter::new(std::io::BufWriter::new(std::fs::File::create(&tmp)?));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // The manifest lists everything, jars included, so the next update can still
    // tell "the pack removed this mod" from "you added it".
    zw.start_file(MANIFEST, opts)?;
    for (rel, size) in &files {
        let crc = crc32_file(&new_mc.join(rel)).unwrap_or(0);
        writeln!(zw, "{rel}\t{size}\t{crc}")?;
    }

    let total = files.len();
    for (n, (rel, size)) in files.iter().enumerate() {
        if n % 256 == 0 && !progress(n, total) {
            anyhow::bail!("cancelled");
        }
        // Jars are never text and are the bulk of the pack; skipping them keeps
        // the snapshot small.
        if is_mod_file(rel) || *size > MAX_BASE_FILE {
            continue;
        }
        let Ok(bytes) = std::fs::read(new_mc.join(rel)) else {
            continue;
        };
        if !util::is_text(&bytes) {
            continue;
        }
        zw.start_file(format!("{FILES_PREFIX}{rel}"), opts)?;
        zw.write_all(&bytes)?;
    }
    zw.finish()?.flush()?;
    std::fs::rename(&tmp, dest)?;
    progress(total, total);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A base whose text files are readable and whose binaries are digest-only —
    /// the shape a saved snapshot has.
    struct FakeBase {
        paths: BTreeMap<String, u64>,
        text: BTreeMap<String, Vec<u8>>,
        digests: BTreeMap<String, u32>,
    }

    impl BaseSource for FakeBase {
        fn paths(&self) -> &BTreeMap<String, u64> {
            &self.paths
        }
        fn digest(&self, rel: &str) -> Option<u32> {
            self.digests.get(rel).copied()
        }
        fn read(&mut self, rel: &str) -> Option<Vec<u8>> {
            self.text.get(rel).cloned()
        }
    }

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gtnh-updater-test-{tag}-{}-{:?}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn write(&self, rel: &str, body: &[u8]) {
            crate::util::write_file(&self.0.join(rel), body).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn crc(bytes: &[u8]) -> u32 {
        let mut h = crc32fast::Hasher::new();
        h.update(bytes);
        h.finalize()
    }

    #[test]
    fn every_kind_of_change_lands_in_the_right_bucket() {
        let old = Scratch::new("old");
        let new = Scratch::new("new");

        // untouched by you, changed by the pack
        old.write("config/a.cfg", b"v=1\n");
        new.write("config/a.cfg", b"v=2\n");
        // changed by you, untouched by the pack
        old.write("config/b.cfg", b"v=mine\n");
        new.write("config/b.cfg", b"v=orig\n");
        // both changed, different places -> merges
        old.write(
            "config/c.cfg",
            b"one=1\ntwo=2\nthree=3\nfour=4\nfive=MINE\n",
        );
        new.write(
            "config/c.cfg",
            b"one=PACK\ntwo=2\nthree=3\nfour=4\nfive=5\n",
        );
        // both changed, same line -> conflict
        old.write("config/d.cfg", b"x=mine\n");
        new.write("config/d.cfg", b"x=pack\n");
        // identical
        old.write("config/e.cfg", b"same\n");
        new.write("config/e.cfg", b"same\n");
        // binary the pack changed and you did not: digest alone decides
        old.write("config/tex.png", b"\x00\x01original");
        new.write("config/tex.png", b"\x00\x01updated");
        // yours only -> carried over
        old.write("saves/world/level.dat", b"\x00save");
        old.write("options.txt", b"fov:1\n");
        // mods
        old.write("mods/kept-1.0.jar", b"jar");
        old.write("mods/dropped-1.0.jar", b"jar");
        old.write("mods/mine-9.9.jar", b"jar");
        old.write("mods/ic2/settings.cfg", b"a=1\n");
        new.write("mods/kept-1.1.jar", b"jar");
        new.write("mods/brandnew-1.0.jar", b"jar");

        let base_text: BTreeMap<String, Vec<u8>> = [
            ("config/a.cfg", b"v=1\n".to_vec()),
            ("config/b.cfg", b"v=orig\n".to_vec()),
            (
                "config/c.cfg",
                b"one=1\ntwo=2\nthree=3\nfour=4\nfive=5\n".to_vec(),
            ),
            ("config/d.cfg", b"x=orig\n".to_vec()),
            ("config/e.cfg", b"same\n".to_vec()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();

        let mut digests: BTreeMap<String, u32> =
            base_text.iter().map(|(k, v)| (k.clone(), crc(v))).collect();
        // The snapshot keeps no bytes for binaries, only their checksum.
        digests.insert("config/tex.png".into(), crc(b"\x00\x01original"));

        let paths: BTreeMap<String, u64> = [
            "config/a.cfg",
            "config/b.cfg",
            "config/c.cfg",
            "config/d.cfg",
            "config/e.cfg",
            "config/tex.png",
            "mods/kept-1.0.jar",
            "mods/dropped-1.0.jar",
        ]
        .into_iter()
        .map(|p| (p.to_string(), 1u64))
        .collect();

        let mut base = FakeBase {
            paths,
            text: base_text,
            digests,
        };
        let plan = build(
            BuildInput {
                old_mc: &old.0,
                new_mc: &new.0,
            },
            &mut base,
            |_, _, _| true,
        )
        .unwrap();

        assert_eq!(plan.identical, 1, "config/e.cfg");
        assert_eq!(
            plan.take_pack, 2,
            "config/a.cfg by content and config/tex.png by digest"
        );
        assert_eq!(plan.keep_yours, vec!["config/b.cfg".to_string()]);
        assert_eq!(plan.auto_merged.len(), 1);
        assert_eq!(plan.auto_merged[0].0, "config/c.cfg");
        assert_eq!(
            plan.auto_merged[0].1,
            "one=PACK\ntwo=2\nthree=3\nfour=4\nfive=MINE\n"
        );
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].rel, "config/d.cfg");
        assert!(plan.binary_conflicts.is_empty());

        // Non-jar files under mods/ are data, not mods, so they carry over.
        let carried: Vec<&str> = plan
            .carry
            .iter()
            .flat_map(|g| g.files.iter().map(|s| s.as_str()))
            .collect();
        assert!(carried.contains(&"saves/world/level.dat"));
        assert!(carried.contains(&"options.txt"));
        assert!(carried.contains(&"mods/ic2/settings.cfg"));

        // Loose files share one group instead of getting a row each.
        let loose = plan
            .carry
            .iter()
            .find(|g| g.name == LOOSE_FILES_GROUP)
            .expect("loose group");
        assert_eq!(loose.files, vec!["options.txt".to_string()]);

        assert_eq!(plan.mods.added, vec!["mods/brandnew-1.0.jar"]);
        assert_eq!(plan.mods.updated.len(), 1);
        let dropped = plan
            .mods
            .decisions
            .iter()
            .find(|d| d.file == "dropped-1.0.jar")
            .unwrap();
        assert!(!dropped.keep);
        let mine = plan
            .mods
            .decisions
            .iter()
            .find(|d| d.file == "mine-9.9.jar")
            .unwrap();
        assert!(mine.keep);
    }

    #[test]
    fn snapshot_roundtrips_text_and_digests() {
        let src = Scratch::new("snap");
        src.write("config/a.cfg", b"hello\n");
        src.write("config/tex.png", b"\x00\x01binary");
        src.write("mods/big-1.0.jar", b"jar bytes");

        let out = Scratch::new("snapout");
        let zip = out.0.join("base.zip");
        write_base_snapshot(&src.0, &zip, |_, _| true).unwrap();

        let mut base = SnapshotBase::open(&zip).unwrap();
        assert_eq!(base.read("config/a.cfg").unwrap(), b"hello\n");
        // Binaries are digest-only, jars are not stored at all.
        assert!(base.read("config/tex.png").is_none());
        assert_eq!(base.digest("config/tex.png"), Some(crc(b"\x00\x01binary")));
        // …but every path stays in the manifest so the mod diff still works.
        assert!(base.paths().contains_key("mods/big-1.0.jar"));
    }
}
