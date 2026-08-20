# GTNH Daily Updater

A desktop updater for [GregTech: New Horizons](https://www.gtnewhorizons.com/) daily
builds on [Prism Launcher](https://prismlauncher.org/). It follows the wiki's
[recommended migration method](https://wiki.gtnewhorizons.com/wiki/Installing_and_Migrating#Method_1:_Migrating_to_a_New_Instance_(Recommended))
— a brand-new instance every time, the old one left untouched — but does the tedious
parts for you:

- **Configs are merged, not clobbered.** A real three-way merge (the same algorithm
  `git merge-file` uses) against the version you are on. Lines you added come across,
  lines the pack changed come across, and you only get asked about genuine collisions.
- **A conflict editor for the collisions.** Per-hunk *yours / pack / both / hand-edit*,
  with the original text one click away, plus a raw whole-file mode.
- **Mod removals are your call.** Anything the new build dropped, and anything you added
  yourself, is listed with a checkbox instead of silently disappearing.
- **Everything else just moves.** Saves, resource packs, shaders, waypoints, screenshots,
  schematics — carried over as-is.
- **Your launch settings follow the instance.** Java path and version, JVM arguments,
  memory limits, window and console preferences are copied onto the new instance, while
  Forge and lwjgl3ify come from the new pack.

`serverutilities/` is treated exactly like `config/`, because it is config.

## How the merge works

Three-way merging needs the *original* file — the one the pack shipped before you edited
anything. The updater gets it in one of two ways:

1. **From a snapshot.** Every time it creates an instance it stores the pack's pristine
   text config under `<instance>/.gtnh-updater/base/`. Later updates use that directly.
2. **From the build you are on.** On a first run there is no snapshot, so it works out
   your build number (daily zips drop a `changelog from daily N to M.md` in `.minecraft`),
   finds that build's artifact on GitHub, and reads the handful of files that actually
   differ **over HTTP range requests** — a few hundred KB out of a 700 MB zip, no full
   download.

If neither is available (the old artifact has expired, say), the updater says so and every
changed file becomes a straight yours-or-pack choice instead.

From there each file falls out of the same question git asks:

| you changed it | the pack changed it | result |
| --- | --- | --- |
| no | no | nothing to do |
| no | yes | the pack's version |
| yes | no | your version |
| yes | yes, elsewhere in the file | merged automatically |
| yes | yes, same lines | **conflict** — you choose |
| yes | yes, and it is not text | **conflict** — pick a side |

## Requirements

- Linux x86_64 (the release binary; it builds anywhere `eframe` does)
- Prism Launcher or MultiMC
- A **GitHub token**. Daily builds are GitHub Actions artifacts, and artifact downloads
  need authentication even though the repository is public. Any token with `repo` or
  `actions:read` scope works. The updater picks one up automatically from `$GH_TOKEN`,
  `$GITHUB_TOKEN`, or the `gh` CLI if you are signed in with it.

## Install

Grab `gtnh-updater-linux-x86_64.tar.gz` from the
[releases page](../../releases), or build it:

```bash
cargo build --release
```

## Use

Run it with no arguments for the graphical updater:

```bash
gtnh-updater
```

It finds your Prism instances, works out which build each one is on, lists the daily
builds still available on GitHub, and walks through four steps: pick, download, review,
create.

### Command line

Useful for a scheduled check, or if you would rather not click:

```bash
gtnh-updater --check                    # what am I on, what is available
gtnh-updater --plan --instance DIR      # download, compare, print the report, change nothing
gtnh-updater --apply --instance DIR     # do it, resolving conflicts by policy
```

| option | meaning |
| --- | --- |
| `--instance DIR` | which instance to update (auto-detected if there is only one) |
| `--build N` | a specific daily build instead of the newest |
| `--variant NAME` | `mmcprism-java17-26` (default) or `mmcprism-java8` |
| `--name NAME` | name for the new instance (default `GTNH-daily-<build>`) |
| `--dest-root DIR` | where to create it (default: next to the old instance) |
| `--on-conflict pack\|yours` | how `--apply` resolves conflicts it cannot merge |
| `--keep-removed` | keep mods the new build dropped |
| `--keep-download` | do not delete the downloaded pack zip afterwards |
| `--token TOKEN` | GitHub token, if you would rather not use the environment |

A nightly check, for example:

```bash
gtnh-updater --check --instance ~/.local/share/PrismLauncher/instances/GTNH-DAILY
```

## What ends up where

The new instance is built in `<instances>/<name>.part` and moved into place only once
everything has been written, so an interrupted or cancelled run cleans up after itself
rather than leaving a half-instance Prism might try to launch.

Each instance the updater creates carries `<instance>/.gtnh-updater/`:

- `state.json` — which build it is, so the next update knows where it started
- `base.zip` — the pack's pristine text config (~25 MB compressed) plus a checksum for
  every file it shipped, which is what makes the *next* merge a real three-way one
  without touching the network

Downloaded pack zips are cached in `~/.cache/gtnh-updater/` and deleted after a
successful update unless you pass `--keep-download` or tick the box.

Prism caches its instance list, so restart it if the new instance does not show up.

## Caveats

- GitHub expires Actions artifacts. Builds older than the retention window cannot be
  downloaded at all, and if the build you are *on* has expired, first-run merges lose
  their original side.
- Adjacent edits count as conflicts. If you added a line directly below one the pack
  changed, that is a collision — `git merge-file` reports it the same way.
- The updater never writes to the instance you are updating.

## Layout

| module | what it does |
| --- | --- |
| `github.rs` | finds daily artifacts, resolves signed download URLs |
| `httpzip.rs` | `Read + Seek` over a remote file, so `zip` can read one entry |
| `pack.rs` | reads a pack zip, re-rooted to the instance layout |
| `merge.rs` | three-way merge and the hunk model the editor edits |
| `plan.rs` | decides what happens to every path |
| `mods.rs` | works out added / updated / removed / yours |
| `prism.rs` | instance discovery, `instance.cfg` handling |
| `apply.rs` | writes the decisions out and moves the instance into place |
| `app.rs`, `merge_ui.rs` | the interface |

## Prior art

[Caedis/gtnh-daily-updater](https://github.com/Caedis/gtnh-daily-updater) updates an
instance in place from the pack manifest and keeps configs on a git branch. This one
installs the prebuilt daily artifact into a fresh instance instead, and puts the conflict
resolution in front of you rather than defaulting to "pack wins".

## License

MIT
