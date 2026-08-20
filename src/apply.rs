//! Writing the decisions out into the staged instance and putting it in place.

use crate::plan::Plan;
use crate::prism::{self, Ini};
use crate::state::{self, InstanceState};
use crate::util;
use anyhow::{Context, Result};
use std::path::Path;

pub struct ApplyInput<'a> {
    /// The old instance directory (not `.minecraft`).
    pub old_instance: &'a Path,
    /// Where the new pack was extracted, e.g. `…/GTNH-daily-690.part`.
    pub staging: &'a Path,
    /// Final directory name inside the instances root.
    pub final_name: &'a str,
    pub instances_root: &'a Path,
    pub display_name: &'a str,
    pub state: InstanceState,
}

/// Apply every decision, then move the staged instance into place.
/// Returns the final instance directory.
pub fn apply(
    input: ApplyInput<'_>,
    plan: &Plan,
    mut progress: impl FnMut(usize, usize, &str) -> bool,
) -> Result<std::path::PathBuf> {
    let old_mc = input.old_instance.join(".minecraft");
    let new_mc = input.staging.join(".minecraft");

    let total = plan.auto_merged.len()
        + plan.conflicts.len()
        + plan.keep_yours.len()
        + plan.binary_conflicts.len()
        + plan.mods.decisions.iter().filter(|d| d.keep).count()
        + plan.mods.state_changes.len()
        + plan
            .carry
            .iter()
            .filter(|g| g.enabled)
            .map(|g| g.files.len())
            .sum::<usize>()
        + 1;
    let mut done = 0usize;
    let tick = |done: &mut usize,
                label: &str,
                progress: &mut dyn FnMut(usize, usize, &str) -> bool|
     -> Result<()> {
        *done += 1;
        if *done % 32 == 0 && !progress(*done, total, label) {
            anyhow::bail!("cancelled");
        }
        Ok(())
    };

    for (rel, text) in &plan.auto_merged {
        util::write_file(&new_mc.join(rel), text.as_bytes())?;
        tick(&mut done, rel, &mut progress)?;
    }

    for conflict in &plan.conflicts {
        let text = conflict.merge.render();
        util::write_file(&new_mc.join(&conflict.rel), text.as_bytes())?;
        tick(&mut done, &conflict.rel, &mut progress)?;
    }

    for rel in &plan.keep_yours {
        util::copy_file(&old_mc.join(rel), &new_mc.join(rel))?;
        tick(&mut done, rel, &mut progress)?;
    }

    for bc in &plan.binary_conflicts {
        if !bc.take_pack {
            util::copy_file(&old_mc.join(&bc.rel), &new_mc.join(&bc.rel))?;
        }
        tick(&mut done, &bc.rel, &mut progress)?;
    }

    for decision in plan.mods.decisions.iter().filter(|d| d.keep) {
        util::copy_file(&old_mc.join(&decision.rel), &new_mc.join(&decision.rel))?;
        tick(&mut done, &decision.file, &mut progress)?;
    }

    // Prism switches a mod off by renaming the jar, so restoring the state you
    // had it in is a rename of the file the new pack shipped.
    for change in &plan.mods.state_changes {
        let shipped = new_mc.join(&change.rel);
        let enabled = strip_disabled(&shipped);
        let wanted = if change.enable {
            enabled.clone()
        } else {
            add_disabled(&enabled)
        };
        if shipped != wanted && shipped.exists() {
            let _ = std::fs::remove_file(&wanted);
            std::fs::rename(&shipped, &wanted).with_context(|| {
                format!(
                    "{} {}",
                    if change.enable { "enable" } else { "disable" },
                    change.file
                )
            })?;
        }
        tick(&mut done, &change.file, &mut progress)?;
    }

    for group in plan.carry.iter().filter(|g| g.enabled) {
        for rel in &group.files {
            let src = old_mc.join(rel);
            if src.is_file() {
                util::copy_file(&src, &new_mc.join(rel))?;
            }
            tick(&mut done, rel, &mut progress)?;
        }
    }

    write_instance_cfg(&input)?;
    state::save(input.staging, &input.state)?;
    progress(total, total, "finishing");

    let final_dir = input.instances_root.join(input.final_name);
    std::fs::rename(input.staging, &final_dir).with_context(|| {
        format!(
            "move {} into place at {}",
            input.staging.display(),
            final_dir.display()
        )
    })?;

    if let Some(old_dir_name) = input.old_instance.file_name().and_then(|n| n.to_str()) {
        let _ = prism::register_in_group(input.instances_root, old_dir_name, input.final_name);
    }
    Ok(final_dir)
}

fn strip_disabled(path: &Path) -> std::path::PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().to_string());
    match name {
        Some(name) => match name.strip_suffix(crate::mods::DISABLED_SUFFIX) {
            Some(base) => path.with_file_name(base),
            None => path.to_path_buf(),
        },
        None => path.to_path_buf(),
    }
}

fn add_disabled(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!("{name}{}", crate::mods::DISABLED_SUFFIX))
}

/// Start from the pack's `instance.cfg`, overlay the old instance's Java and
/// launch settings, and carry a custom icon across if there is one.
fn write_instance_cfg(input: &ApplyInput<'_>) -> Result<()> {
    let pack_cfg_path = input.staging.join("instance.cfg");
    let old_cfg_path = input.old_instance.join("instance.cfg");
    let pack_cfg = Ini::load(&pack_cfg_path).unwrap_or_default();
    let old_cfg =
        Ini::load(&old_cfg_path).with_context(|| format!("read {}", old_cfg_path.display()))?;
    let mut merged = prism::merge_instance_cfg(&pack_cfg, &old_cfg, input.display_name);

    // Instance-local icons live next to instance.cfg; keep the user's if they set one.
    if let Some(icon_key) = old_cfg.get("General", "iconKey") {
        for ext in ["png", "jpg", "jpeg", "ico", "svg"] {
            let src = input.old_instance.join(format!("{icon_key}.{ext}"));
            if src.is_file() {
                let _ = util::copy_file(&src, &input.staging.join(format!("{icon_key}.{ext}")));
                merged.set("General", "iconKey", &icon_key);
                break;
            }
        }
    }
    merged.save(&pack_cfg_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mods::ModStateChange;
    use crate::plan::Plan;

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gtnh-apply-test-{tag}-{}-{}",
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
            util::write_file(&self.0.join(rel), body).unwrap();
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn mod_states_are_restored_and_the_instance_moves_into_place() {
        let root = Scratch::new("root");
        let old = Scratch::new("old");
        old.write(
            "instance.cfg",
            b"[General]\nname=GTNH-DAILY\nJvmArgs=-Xmx9G\n",
        );
        old.write(".minecraft/mods/journeymap-5.2.20.jar.disabled", b"old jar");

        let staging = root.0.join("GTNH-daily-690.part");
        util::write_file(
            &staging.join("instance.cfg"),
            b"InstanceType=OneSix\nname=pack\n",
        )
        .unwrap();
        util::write_file(
            &staging.join(".minecraft/mods/journeymap-5.2.21.jar"),
            b"new jar",
        )
        .unwrap();
        util::write_file(&staging.join(".minecraft/mods/keepme-1.0.jar"), b"jar").unwrap();

        let mut plan = Plan::default();
        plan.mods.state_changes.push(ModStateChange {
            rel: "mods/journeymap-5.2.21.jar".into(),
            file: "journeymap-5.2.21.jar".into(),
            enable: false,
        });

        let dir = apply(
            ApplyInput {
                old_instance: &old.0,
                staging: &staging,
                final_name: "GTNH-daily-690",
                instances_root: &root.0,
                display_name: "GTNH-daily-690",
                state: InstanceState::default(),
            },
            &plan,
            |_, _, _| true,
        )
        .unwrap();

        assert_eq!(dir, root.0.join("GTNH-daily-690"));
        assert!(!staging.exists(), "staging should have been moved");

        let mods = dir.join(".minecraft/mods");
        assert!(
            mods.join("journeymap-5.2.21.jar.disabled").is_file(),
            "the mod you had switched off should arrive switched off"
        );
        assert!(!mods.join("journeymap-5.2.21.jar").exists());
        assert!(
            mods.join("keepme-1.0.jar").is_file(),
            "others are untouched"
        );

        let cfg = crate::prism::Ini::load(&dir.join("instance.cfg")).unwrap();
        assert_eq!(cfg.get("General", "JvmArgs").unwrap(), "-Xmx9G");
        assert_eq!(cfg.get("General", "name").unwrap(), "GTNH-daily-690");
    }
}
