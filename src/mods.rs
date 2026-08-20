//! Working out what happened to the mod list between two pack versions.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Strip the version from a jar filename so the same mod matches across builds.
///
/// `angelica-2.1.56.jar` and `angelica-2.2.0.jar` both reduce to `angelica`.
/// The exact stem does not matter as long as it is stable between versions, so
/// the rule is simply: cut at the first dash-separated token that is a number.
pub fn mod_key(filename: &str) -> String {
    let stem = filename
        .strip_suffix(".jar")
        .or_else(|| filename.strip_suffix(".litemod"))
        .or_else(|| filename.strip_suffix(".jar.disabled"))
        .unwrap_or(filename);
    let parts: Vec<&str> = stem.split('-').collect();
    let mut cut = parts.len();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            continue;
        }
        if looks_like_version(part) {
            cut = i;
            break;
        }
    }
    parts[..cut].join("-").to_lowercase()
}

fn looks_like_version(token: &str) -> bool {
    let t = token.strip_prefix('v').unwrap_or(token);
    if t.is_empty() {
        return false;
    }
    let mut chars = t.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    t.chars()
        .all(|c| c.is_ascii_digit() || c == '.' || c == '_')
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModKind {
    /// Shipped in the version you are on, gone from the new one.
    RemovedByPack,
    /// Never part of the pack — something you added yourself.
    UserAdded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModDecision {
    /// Path relative to `.minecraft`, e.g. `mods/angelica-2.1.56.jar`.
    pub rel: String,
    pub file: String,
    pub kind: ModKind,
    /// Whether to carry this jar into the new instance.
    pub keep: bool,
    pub size: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModPlan {
    /// Jars needing a yes/no from the user.
    pub decisions: Vec<ModDecision>,
    /// Mods the new pack updated in place, as `(old file, new file)`.
    pub updated: Vec<(String, String)>,
    /// Jars only the new pack has.
    pub added: Vec<String>,
    /// Jars that carry across unchanged.
    pub unchanged: usize,
}

impl ModPlan {
    pub fn removed_count(&self) -> usize {
        self.decisions
            .iter()
            .filter(|d| d.kind == ModKind::RemovedByPack)
            .count()
    }
    pub fn extra_count(&self) -> usize {
        self.decisions
            .iter()
            .filter(|d| d.kind == ModKind::UserAdded)
            .count()
    }
}

/// Compare the three mod lists. `base` is what the version you are on shipped;
/// pass an empty set when that is unknown — everything unmatched then reads as
/// "you added this", which is the safer default because it keeps files.
pub fn plan(
    base: &BTreeMap<String, u64>,
    ours: &BTreeMap<String, u64>,
    theirs: &BTreeMap<String, u64>,
) -> ModPlan {
    let key_index = |files: &BTreeMap<String, u64>| -> BTreeMap<String, Vec<String>> {
        let mut m: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for rel in files.keys() {
            let file = file_name(rel);
            m.entry(mod_key(&file)).or_default().push(rel.clone());
        }
        m
    };
    let ours_by_key = key_index(ours);
    let theirs_by_key = key_index(theirs);
    let base_keys: BTreeSet<String> = key_index(base).into_keys().collect();

    let mut out = ModPlan::default();

    for (key, their_files) in &theirs_by_key {
        match ours_by_key.get(key) {
            None => out.added.extend(their_files.iter().cloned()),
            Some(our_files) => {
                let our_names: BTreeSet<&str> = our_files.iter().map(|s| s.as_str()).collect();
                for tf in their_files {
                    if our_names.contains(tf.as_str()) {
                        out.unchanged += 1;
                    } else {
                        let from = our_files.first().cloned().unwrap_or_default();
                        out.updated.push((file_name(&from), file_name(tf)));
                    }
                }
            }
        }
    }

    for (key, our_files) in &ours_by_key {
        if theirs_by_key.contains_key(key) {
            continue;
        }
        let kind = if base_keys.contains(key) {
            ModKind::RemovedByPack
        } else {
            ModKind::UserAdded
        };
        for rel in our_files {
            out.decisions.push(ModDecision {
                rel: rel.clone(),
                file: file_name(rel),
                kind,
                // The pack dropping a mod is deliberate; your own additions are not.
                keep: kind == ModKind::UserAdded,
                size: ours.get(rel).copied().unwrap_or(0),
            });
        }
    }
    out.decisions.sort_by(|a, b| {
        (a.kind == ModKind::UserAdded, a.file.to_lowercase())
            .cmp(&(b.kind == ModKind::UserAdded, b.file.to_lowercase()))
    });
    out
}

fn file_name(rel: &str) -> String {
    rel.rsplit('/').next().unwrap_or(rel).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeMap<String, u64> {
        items.iter().map(|s| (s.to_string(), 1u64)).collect()
    }

    #[test]
    fn versions_normalise_to_a_stable_key() {
        assert_eq!(mod_key("angelica-2.1.56.jar"), "angelica");
        assert_eq!(mod_key("angelica-2.2.0.jar"), "angelica");
        assert_eq!(mod_key("AmunRa-GC-0.8.13.jar"), "amunra-gc");
        assert_eq!(
            mod_key("appliedenergistics2-rv3-beta-1019-GTNH.jar"),
            mod_key("appliedenergistics2-rv3-beta-1034-GTNH.jar")
        );
    }

    #[test]
    fn distinguishes_pack_removals_from_user_extras() {
        let base = set(&["mods/dropped-1.0.jar", "mods/kept-1.0.jar"]);
        let ours = set(&[
            "mods/dropped-1.0.jar",
            "mods/kept-1.0.jar",
            "mods/mine-3.0.jar",
        ]);
        let theirs = set(&["mods/kept-1.1.jar", "mods/brandnew-1.0.jar"]);
        let p = plan(&base, &ours, &theirs);

        assert_eq!(p.added, vec!["mods/brandnew-1.0.jar"]);
        assert_eq!(
            p.updated,
            vec![("kept-1.0.jar".into(), "kept-1.1.jar".into())]
        );
        assert_eq!(p.removed_count(), 1);
        assert_eq!(p.extra_count(), 1);

        let dropped = p
            .decisions
            .iter()
            .find(|d| d.file == "dropped-1.0.jar")
            .unwrap();
        assert_eq!(dropped.kind, ModKind::RemovedByPack);
        assert!(!dropped.keep, "pack removals default to being dropped");

        let mine = p
            .decisions
            .iter()
            .find(|d| d.file == "mine-3.0.jar")
            .unwrap();
        assert_eq!(mine.kind, ModKind::UserAdded);
        assert!(mine.keep, "your own mods default to being kept");
    }

    #[test]
    fn without_a_base_everything_reads_as_user_added() {
        let ours = set(&["mods/whatever-1.0.jar"]);
        let theirs = set(&["mods/other-1.0.jar"]);
        let p = plan(&BTreeMap::new(), &ours, &theirs);
        assert_eq!(p.extra_count(), 1);
        assert!(p.decisions[0].keep);
    }
}
