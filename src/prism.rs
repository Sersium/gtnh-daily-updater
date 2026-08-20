//! Prism / MultiMC instance discovery and `instance.cfg` handling.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// `[General]` keys describing how the instance launches rather than what pack it
/// is. These are carried from the old instance onto the freshly extracted one, so
/// the new instance keeps the same Java binary, JVM arguments and memory limits.
pub const CARRY_OVER_KEYS: &[&str] = &[
    "AutoCloseConsole",
    "AutomaticJava",
    "CloseAfterLaunch",
    "CustomGLFWPath",
    "CustomOpenALPath",
    "EnableFeralGamemode",
    "EnableMangoHud",
    "Env",
    "IgnoreJavaCompatibility",
    "InstanceAccountId",
    "JavaArchitecture",
    "JavaPath",
    "JavaRealArchitecture",
    "JavaSignature",
    "JavaVendor",
    "JavaVersion",
    "JoinServerOnLaunch",
    "JoinServerOnLaunchAddress",
    "JoinWorldOnLaunch",
    "JvmArgs",
    "LaunchMaximized",
    "LogPrePostOutput",
    "LowMemWarning",
    "MaxMemAlloc",
    "MinMemAlloc",
    "MinecraftWinHeight",
    "MinecraftWinWidth",
    "ModDownloadLoaders",
    "OnlineFixes",
    "OverrideCommands",
    "OverrideConsole",
    "OverrideEnv",
    "OverrideGameTime",
    "OverrideJavaArgs",
    "OverrideJavaLocation",
    "OverrideMemory",
    "OverrideMiscellaneous",
    "OverrideModDownloadLoaders",
    "OverrideNativeWorkarounds",
    "OverridePerformance",
    "OverrideWindow",
    "PermGen",
    "PostExitCommand",
    "PreLaunchCommand",
    "Profiler",
    "QuitAfterGameStop",
    "RecordGameTime",
    "ShowConsole",
    "ShowConsoleOnError",
    "ShowGameTime",
    "UseAccountForInstance",
    "UseDiscreteGpu",
    "UseNativeGLFW",
    "UseNativeOpenAL",
    "UseZink",
    "WrapperCommand",
    "notes",
    "totalTimePlayed",
];

#[derive(Debug, Clone)]
pub struct Instance {
    pub dir: PathBuf,
    pub name: String,
}

impl Instance {
    pub fn minecraft(&self) -> PathBuf {
        self.dir.join(".minecraft")
    }
}

/// Every plausible Prism/MultiMC instances directory on this machine.
pub fn candidate_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut push = |p: PathBuf| {
        if p.is_dir() && !out.contains(&p) {
            out.push(p);
        }
    };
    if let Some(data) = dirs::data_dir() {
        push(data.join("PrismLauncher/instances"));
        push(data.join("multimc/instances"));
        push(data.join("MultiMC/instances"));
    }
    if let Some(home) = dirs::home_dir() {
        push(home.join(".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/instances"));
        push(home.join(".local/share/PrismLauncher/instances"));
        push(home.join("PrismLauncher/instances"));
    }
    if let Some(cfg) = dirs::config_dir() {
        push(cfg.join("PrismLauncher/instances"));
    }
    out
}

pub fn list_instances(root: &Path) -> Vec<Instance> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() || !dir.join("instance.cfg").is_file() {
            continue;
        }
        let name = Ini::load(&dir.join("instance.cfg"))
            .ok()
            .and_then(|ini| ini.get("General", "name"))
            .unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
            });
        out.push(Instance { dir, name });
    }
    out.sort_by_key(|i| i.name.to_lowercase());
    out
}

/// Keys written before any `[section]` header. Prism writes `instance.cfg`
/// through Qt's QSettings, which files those under `General` — and the pack ships
/// exactly that shape, with no header at all.
pub const DEFAULT_SECTION: &str = "General";

/// A line-preserving INI reader/writer. Prism rewrites `instance.cfg` itself, but
/// keeping comments and ordering intact makes diffs between instances readable.
#[derive(Debug, Clone, Default)]
pub struct Ini {
    lines: Vec<Line>,
}

#[derive(Debug, Clone)]
enum Line {
    Section(String),
    Kv {
        section: String,
        key: String,
        value: String,
    },
    Other(String),
}

impl Ini {
    pub fn parse(text: &str) -> Self {
        let mut lines = Vec::new();
        let mut section = DEFAULT_SECTION.to_string();
        for raw in text.lines() {
            let t = raw.trim();
            if t.starts_with('[') && t.ends_with(']') {
                section = t[1..t.len() - 1].to_string();
                lines.push(Line::Section(section.clone()));
            } else if let Some((k, v)) = t.split_once('=') {
                if t.starts_with('#') || t.starts_with(';') {
                    lines.push(Line::Other(raw.to_string()));
                } else {
                    lines.push(Line::Kv {
                        section: section.clone(),
                        key: k.trim().to_string(),
                        value: v.to_string(),
                    });
                }
            } else {
                lines.push(Line::Other(raw.to_string()));
            }
        }
        Self { lines }
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Ok(Self::parse(&text))
    }

    pub fn get(&self, section: &str, key: &str) -> Option<String> {
        self.lines.iter().find_map(|l| match l {
            Line::Kv {
                section: s,
                key: k,
                value,
            } if s == section && k == key => Some(value.clone()),
            _ => None,
        })
    }

    pub fn set(&mut self, section: &str, key: &str, value: &str) {
        for line in self.lines.iter_mut() {
            if let Line::Kv {
                section: s,
                key: k,
                value: v,
            } = line
            {
                if s == section && k == key {
                    *v = value.to_string();
                    return;
                }
            }
        }
        // Append at the end of the section, or create the section.
        let insert_at = self
            .lines
            .iter()
            .rposition(|l| matches!(l, Line::Kv { section: s, .. } if s == section))
            .map(|i| i + 1);
        let entry = Line::Kv {
            section: section.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        };
        match insert_at {
            Some(i) => self.lines.insert(i, entry),
            None => {
                self.lines.push(Line::Section(section.to_string()));
                self.lines.push(entry);
            }
        }
    }

    /// All keys present in a section.
    pub fn keys(&self, section: &str) -> Vec<String> {
        self.lines
            .iter()
            .filter_map(|l| match l {
                Line::Kv {
                    section: s, key, ..
                } if s == section => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        std::fs::write(path, self.to_string()).with_context(|| format!("write {}", path.display()))
    }
}

impl std::fmt::Display for Ini {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for line in &self.lines {
            match line {
                Line::Section(s) => writeln!(f, "[{s}]")?,
                Line::Kv { key, value, .. } => writeln!(f, "{key}={value}")?,
                Line::Other(raw) => writeln!(f, "{raw}")?,
            }
        }
        Ok(())
    }
}

/// Build the new instance's `instance.cfg`: pack defaults, overlaid with the old
/// instance's launch settings, renamed, and given a fresh UUID.
pub fn merge_instance_cfg(pack_cfg: &Ini, old_cfg: &Ini, new_name: &str) -> Ini {
    let mut out = pack_cfg.clone();
    for key in CARRY_OVER_KEYS {
        if let Some(v) = old_cfg.get("General", key) {
            out.set("General", key, &v);
        }
    }
    // Column layouts and other view state are harmless and nice to keep.
    for key in old_cfg.keys("UI") {
        if let Some(v) = old_cfg.get("UI", &key) {
            out.set("UI", &key, &v);
        }
    }
    out.set("General", "name", new_name);
    out.set("General", "uuid", &new_uuid());
    out.set("General", "lastLaunchTime", "0");
    out.set("General", "lastTimePlayed", "0");
    out
}

/// A 32-char hex id in the shape Prism writes. Not a real UUIDv4 — Prism only
/// needs it to be unique — but it is seeded from the clock and process id.
fn new_uuid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mut state = nanos ^ (pid << 64) ^ 0x9E37_79B9_7F4A_7C15;
    let mut out = String::with_capacity(32);
    for _ in 0..32 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let nibble = ((state >> 60) & 0xF) as u8;
        out.push(char::from_digit(nibble as u32, 16).unwrap_or('0'));
    }
    out
}

/// Add the new instance to whatever group the old one was in, so it shows up in
/// the same place in Prism's instance list.
pub fn register_in_group(root: &Path, old_dir_name: &str, new_dir_name: &str) -> Result<()> {
    let path = root.join("instgroups.json");
    if !path.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&path)?;
    let mut json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    let Some(groups) = json.get_mut("groups").and_then(|g| g.as_object_mut()) else {
        return Ok(());
    };
    let mut target: Option<String> = None;
    for (name, group) in groups.iter() {
        let in_group = group
            .get("instances")
            .and_then(|i| i.as_array())
            .map(|a| a.iter().any(|v| v.as_str() == Some(old_dir_name)))
            .unwrap_or(false);
        if in_group {
            target = Some(name.clone());
            break;
        }
    }
    let Some(target) = target else {
        return Ok(());
    };
    if let Some(list) = groups
        .get_mut(&target)
        .and_then(|g| g.get_mut("instances"))
        .and_then(|i| i.as_array_mut())
    {
        if !list.iter().any(|v| v.as_str() == Some(new_dir_name)) {
            list.push(serde_json::Value::String(new_dir_name.to_string()));
        }
    }
    std::fs::write(&path, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

/// Pick a directory name that does not exist yet, appending `-2`, `-3`, … if needed.
pub fn unique_dir_name(root: &Path, wanted: &str) -> Result<String> {
    if wanted.trim().is_empty() {
        bail!("instance name cannot be empty");
    }
    let sanitized: String = wanted
        .chars()
        .map(|c| if "/\\:*?\"<>|".contains(c) { '-' } else { c })
        .collect();
    if !root.join(&sanitized).exists() {
        return Ok(sanitized);
    }
    for n in 2..1000 {
        let candidate = format!("{sanitized}-{n}");
        if !root.join(&candidate).exists() {
            return Ok(candidate);
        }
    }
    bail!("could not find a free directory name for {wanted}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carries_launch_settings_but_not_identity() {
        let pack = Ini::parse(
            "[General]\nname=GT New Horizons daily\nJvmArgs=\nMaxMemAlloc=6144\nuuid=aaa\n",
        );
        let old = Ini::parse("[General]\nname=GTNH-DAILY\nJvmArgs=-XX:+UseShenandoahGC\nMaxMemAlloc=10240\nuuid=bbb\n[UI]\nfoo=bar\n");
        let merged = merge_instance_cfg(&pack, &old, "GTNH-daily-690");
        assert_eq!(
            merged.get("General", "JvmArgs").unwrap(),
            "-XX:+UseShenandoahGC"
        );
        assert_eq!(merged.get("General", "MaxMemAlloc").unwrap(), "10240");
        assert_eq!(merged.get("General", "name").unwrap(), "GTNH-daily-690");
        assert_ne!(merged.get("General", "uuid").unwrap(), "bbb");
        assert_eq!(merged.get("UI", "foo").unwrap(), "bar");
    }

    /// The pack's own `instance.cfg` has no `[General]` header, so settings must
    /// still be found and replaced in place rather than appended a second time.
    #[test]
    fn headerless_keys_belong_to_general() {
        let pack = Ini::parse("InstanceType=OneSix\nOverrideJavaArgs=false\nname=GTNH 2.9.x\n");
        assert_eq!(pack.get("General", "OverrideJavaArgs").unwrap(), "false");

        let old = Ini::parse("[General]\nOverrideJavaArgs=true\nJvmArgs=-Xmx8G\n");
        let merged = merge_instance_cfg(&pack, &old, "GTNH-daily-690");

        let text = merged.to_string();
        assert_eq!(text.matches("OverrideJavaArgs=").count(), 1, "{text}");
        assert_eq!(
            text.matches("\nname=").count() + usize::from(text.starts_with("name=")),
            1,
            "{text}"
        );
        assert_eq!(merged.get("General", "OverrideJavaArgs").unwrap(), "true");
        assert_eq!(merged.get("General", "name").unwrap(), "GTNH-daily-690");
        assert_eq!(merged.get("General", "InstanceType").unwrap(), "OneSix");
    }

    #[test]
    fn ini_roundtrips_unknown_lines() {
        let text = "[General]\n# a comment\nkey=value\n\n[UI]\nother=1\n";
        let ini = Ini::parse(text);
        assert_eq!(ini.to_string(), text);
    }
}
