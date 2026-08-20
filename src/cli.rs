//! Headless modes. `--check` and `--plan` are read-only; `--apply` runs the same
//! update the GUI does, using a fixed policy instead of asking.

use anyhow::{bail, Context, Result};
use gtnh_updater::github::{Gh, VARIANT_JAVA17_26};
use gtnh_updater::merge::Choice;
use gtnh_updater::prism::{self, Instance};
use gtnh_updater::state::{self, InstanceState};
use gtnh_updater::util::human_bytes;
use gtnh_updater::worker::{self, ApplyJob, Cancel, Evt, PrepareJob, Prepared};
use std::path::PathBuf;

const HELP: &str = "\
gtnh-updater — daily GTNH updater for Prism Launcher

  gtnh-updater [INSTANCE_DIR]            open the graphical updater
  gtnh-updater --check [--instance DIR]  print the installed and latest builds
  gtnh-updater --plan --instance DIR     download and compare, then report
  gtnh-updater --apply --instance DIR    create the updated instance

Options:
  --instance DIR       instance to update (default: auto-detected)
  --build N            daily build number (default: newest)
  --variant NAME       mmcprism-java17-26 (default) or mmcprism-java8
  --name NAME          name for the new instance (default: GTNH-daily-<build>)
  --dest-root DIR      where to create it (default: alongside the old instance)
  --on-conflict WHICH  pack (default) or yours — how --apply resolves conflicts
  --keep-removed       keep mods the new build dropped instead of removing them
  --keep-download      do not delete the downloaded pack zip afterwards
  --token TOKEN        GitHub token (default: $GH_TOKEN, $GITHUB_TOKEN, gh CLI)
  -h, --help           show this
";

pub enum Mode {
    Gui(Option<PathBuf>),
    Check,
    Plan,
    Apply,
}

pub struct Args {
    pub mode: Mode,
    pub instance: Option<PathBuf>,
    pub build: Option<u32>,
    pub variant: String,
    pub name: Option<String>,
    pub dest_root: Option<PathBuf>,
    pub on_conflict: Choice,
    pub keep_removed: bool,
    pub keep_download: bool,
    pub token: Option<String>,
}

/// Returns `None` when help was printed and the process should just exit.
pub fn parse() -> Result<Option<Args>> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut args = Args {
        mode: Mode::Gui(None),
        instance: None,
        build: None,
        variant: VARIANT_JAVA17_26.to_string(),
        name: None,
        dest_root: None,
        on_conflict: Choice::Theirs,
        keep_removed: false,
        keep_download: false,
        token: None,
    };
    let mut i = 0;
    let mut positional: Option<PathBuf> = None;
    while i < raw.len() {
        let arg = raw[i].as_str();
        let next = |i: &mut usize| -> Result<String> {
            *i += 1;
            raw.get(*i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))
        };
        match arg {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "--check" => args.mode = Mode::Check,
            "--plan" => args.mode = Mode::Plan,
            "--apply" => args.mode = Mode::Apply,
            "--instance" => args.instance = Some(PathBuf::from(next(&mut i)?)),
            "--build" => args.build = Some(next(&mut i)?.parse().context("--build")?),
            "--variant" => args.variant = next(&mut i)?,
            "--name" => args.name = Some(next(&mut i)?),
            "--dest-root" => args.dest_root = Some(PathBuf::from(next(&mut i)?)),
            "--on-conflict" => {
                args.on_conflict = match next(&mut i)?.as_str() {
                    "pack" | "theirs" => Choice::Theirs,
                    "yours" | "ours" => Choice::Ours,
                    other => bail!("--on-conflict must be `pack` or `yours`, got `{other}`"),
                }
            }
            "--keep-removed" => args.keep_removed = true,
            "--keep-download" => args.keep_download = true,
            "--token" => args.token = Some(next(&mut i)?),
            other if other.starts_with('-') => bail!("unknown option {other}"),
            other => positional = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    if let Mode::Gui(_) = args.mode {
        args.mode = Mode::Gui(positional.clone());
    }
    if args.instance.is_none() {
        args.instance = positional;
    }
    Ok(Some(args))
}

/// Locate the instance to work on: the one given, or the only GTNH-looking one.
fn resolve_instance(args: &Args) -> Result<(Instance, PathBuf)> {
    if let Some(dir) = &args.instance {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.clone());
        if !dir.join("instance.cfg").is_file() {
            bail!("{} does not look like a Prism instance", dir.display());
        }
        let name = prism::Ini::load(&dir.join("instance.cfg"))
            .ok()
            .and_then(|i| i.get("General", "name"))
            .unwrap_or_else(|| dir.file_name().unwrap_or_default().to_string_lossy().into());
        let root = dir
            .parent()
            .ok_or_else(|| anyhow::anyhow!("instance has no parent directory"))?
            .to_path_buf();
        return Ok((Instance { dir, name }, root));
    }
    for root in prism::candidate_roots() {
        let mut found: Vec<Instance> = prism::list_instances(&root)
            .into_iter()
            .filter(|i| state::detect_build_from_changelogs(&i.minecraft()).is_some())
            .collect();
        if found.len() == 1 {
            return Ok((found.remove(0), root));
        }
        if found.len() > 1 {
            bail!(
                "several GTNH instances found in {}; pass --instance",
                root.display()
            );
        }
    }
    bail!("no GTNH instance found; pass --instance DIR")
}

pub fn run(args: Args) -> Result<()> {
    let token = args.token.clone().or_else(Gh::discover_token);
    let (instance, root) = resolve_instance(&args)?;
    let installed = state::load(&instance.dir)
        .build
        .or_else(|| state::detect_build_from_changelogs(&instance.minecraft()));

    let gh = Gh::new(token.clone());
    let builds = gh.recent_builds(&args.variant, 1)?;
    let artifact = match args.build {
        Some(b) => gh
            .find_build(b, &args.variant, 8)?
            .ok_or_else(|| anyhow::anyhow!("daily {b} is not available as an artifact"))?,
        None => builds
            .first()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no daily builds found"))?,
    };

    println!("instance : {} ({})", instance.name, instance.dir.display());
    println!(
        "installed: {}",
        installed.map_or("unknown".to_string(), |b| format!("daily {b}"))
    );
    println!(
        "available: {} ({})",
        artifact.label(),
        human_bytes(artifact.size)
    );

    if matches!(args.mode, Mode::Check) {
        return Ok(());
    }
    if installed == Some(artifact.build) && !matches!(args.mode, Mode::Plan) {
        println!("already up to date.");
        return Ok(());
    }

    let dest_root = args.dest_root.clone().unwrap_or(root);
    std::fs::create_dir_all(&dest_root)?;
    let display_name = args
        .name
        .clone()
        .unwrap_or_else(|| format!("GTNH-daily-{}", artifact.build));
    let final_name = prism::unique_dir_name(&dest_root, &display_name)?;

    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = Cancel::new();
    worker::spawn_prepare(
        tx,
        PrepareJob {
            token: token.clone(),
            instance: instance.clone(),
            instances_root: dest_root.clone(),
            artifact: artifact.clone(),
            final_name: final_name.clone(),
            display_name: display_name.clone(),
            cancel,
        },
    );

    let prepared = match pump(rx)? {
        Outcome::Prepared(p) => *p,
        Outcome::Applied(_) => unreachable!(),
    };
    report(&prepared);

    if matches!(args.mode, Mode::Plan) {
        worker::discard_staging(&prepared.staging);
        println!("\n(--plan: staging discarded, nothing was created)");
        return Ok(());
    }

    let mut plan = prepared.plan;
    for conflict in plan.conflicts.iter_mut() {
        for hunk in conflict.merge.hunks.iter_mut() {
            hunk.choice = args.on_conflict;
        }
        conflict.reviewed = true;
    }
    for bc in plan.binary_conflicts.iter_mut() {
        bc.take_pack = args.on_conflict == Choice::Theirs;
    }
    if args.keep_removed {
        for d in plan.mods.decisions.iter_mut() {
            d.keep = true;
        }
    }

    let state = InstanceState {
        build: Some(artifact.build),
        date: Some(artifact.date.clone()),
        variant: Some(artifact.variant.clone()),
        artifact_id: Some(artifact.id),
        updated_at: Some(state::now_iso()),
        updated_from: instance
            .dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string()),
    };
    let (tx, rx) = std::sync::mpsc::channel();
    worker::spawn_apply(
        tx,
        ApplyJob {
            plan: Box::new(plan),
            prepared_staging: prepared.staging,
            old_instance: instance.dir,
            instances_root: dest_root,
            final_name,
            display_name,
            state,
            download_path: prepared.download_path,
            keep_download: args.keep_download,
            cancel: Cancel::new(),
        },
    );
    match pump(rx)? {
        Outcome::Applied(dir) => println!("\ncreated {}", dir.display()),
        Outcome::Prepared(_) => unreachable!(),
    }
    Ok(())
}

enum Outcome {
    Prepared(Box<Prepared>),
    Applied(PathBuf),
}

/// Drain worker events, drawing a single-line progress indicator.
fn pump(rx: std::sync::mpsc::Receiver<Evt>) -> Result<Outcome> {
    use std::io::Write;
    let mut last = String::new();
    loop {
        match rx.recv() {
            Ok(Evt::Progress { phase, frac, msg }) => {
                let line = format!("{}: {:>3.0}%  {msg}", phase.label(), frac * 100.0);
                if line != last {
                    print!("\r\x1b[2K{line}");
                    let _ = std::io::stdout().flush();
                    last = line;
                }
            }
            Ok(Evt::Prepared(p)) => {
                println!();
                return Ok(Outcome::Prepared(p));
            }
            Ok(Evt::Applied(dir)) => {
                println!();
                return Ok(Outcome::Applied(dir));
            }
            Ok(Evt::Failed(e)) => {
                println!();
                bail!(e);
            }
            Ok(Evt::Builds(_)) => {}
            Err(_) => bail!("worker stopped unexpectedly"),
        }
    }
}

fn report(prepared: &Prepared) {
    let p = &prepared.plan;
    if let Some(old) = prepared.old_build {
        println!(
            "\nupdating daily {old} -> daily {}",
            prepared.artifact.build
        );
    }
    println!("{}", prepared.base_note);
    println!("\nconfig files");
    println!("  {:>6}  identical", p.identical);
    println!(
        "  {:>6}  taken from the pack (you had not edited them)",
        p.take_pack
    );
    println!("  {:>6}  merged automatically", p.auto_merged.len());
    println!(
        "  {:>6}  your version kept (pack unchanged)",
        p.keep_yours.len()
    );
    println!("  {:>6}  conflicts", p.conflicts.len());
    println!("  {:>6}  non-text conflicts", p.binary_conflicts.len());
    println!("\nmods");
    println!("  {:>6}  added by this build", p.mods.added.len());
    println!("  {:>6}  updated", p.mods.updated.len());
    println!("  {:>6}  unchanged", p.mods.unchanged);
    println!("  {:>6}  removed by this build", p.mods.removed_count());
    println!("  {:>6}  yours, not in the pack", p.mods.extra_count());
    println!("\nuser data");
    for g in &p.carry {
        println!(
            "  {:>10}  {} ({} files)",
            human_bytes(g.bytes),
            g.name,
            g.files.len()
        );
    }
    if !p.conflicts.is_empty() {
        println!("\nconflicting files");
        for c in p.conflicts.iter().take(40) {
            let tag = if c.no_base { " (no original)" } else { "" };
            println!("  {:>3} × {}{tag}", c.merge.hunks.len(), c.rel);
        }
        if p.conflicts.len() > 40 {
            println!("  … and {} more", p.conflicts.len() - 40);
        }
    }
}
