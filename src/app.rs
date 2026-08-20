//! Wizard shell: pick an instance and a build, review what will change, apply.

use crate::merge_ui::{self, Palette};
use eframe::egui;
use egui::{Color32, RichText};
use gtnh_updater::github::{DailyArtifact, Gh, VARIANT_JAVA17_26, VARIANT_JAVA8};
use gtnh_updater::mods::ModKind;
use gtnh_updater::prism::{self, Instance};
use gtnh_updater::state::{self, InstanceState};
use gtnh_updater::util::human_bytes;
use gtnh_updater::worker::{self, ApplyJob, Cancel, Evt, Phase, PrepareJob, Prepared};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

#[derive(PartialEq, Eq, Clone, Copy)]
enum Step {
    Setup,
    Working,
    Review,
    Applying,
    Done,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum Tab {
    Conflicts,
    Mods,
    Files,
    Summary,
}

struct InstanceRow {
    inst: Instance,
    build: Option<u32>,
    has_snapshot: bool,
}

pub struct App {
    pal: Palette,
    step: Step,
    roots: Vec<PathBuf>,
    root_idx: usize,
    instances: Vec<InstanceRow>,
    inst_idx: Option<usize>,
    token: String,
    variant: String,
    builds: Vec<DailyArtifact>,
    build_idx: usize,
    fetching: bool,
    new_name: String,
    name_edited: bool,
    keep_download: bool,

    tx: Sender<Evt>,
    rx: Receiver<Evt>,
    cancel: Cancel,
    phase: Phase,
    frac: f32,
    msg: String,
    error: Option<String>,

    prepared: Option<Prepared>,
    tab: Tab,
    sel_conflict: usize,
    filter: String,
    done_dir: Option<PathBuf>,
}

impl App {
    pub fn new(preselect: Option<PathBuf>) -> Self {
        Self::build(preselect, true)
    }

    fn build(preselect: Option<PathBuf>, online: bool) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let roots = prism::candidate_roots();
        let mut app = Self {
            pal: Palette::dark(),
            step: Step::Setup,
            roots,
            root_idx: 0,
            instances: Vec::new(),
            inst_idx: None,
            token: if online {
                Gh::discover_token().unwrap_or_default()
            } else {
                String::new()
            },
            variant: VARIANT_JAVA17_26.to_string(),
            builds: Vec::new(),
            build_idx: 0,
            fetching: false,
            new_name: String::new(),
            name_edited: false,
            keep_download: false,
            tx,
            rx,
            cancel: Cancel::new(),
            phase: Phase::Query,
            frac: 0.0,
            msg: String::new(),
            error: None,
            prepared: None,
            tab: Tab::Conflicts,
            sel_conflict: 0,
            filter: String::new(),
            done_dir: None,
        };
        // A path on the command line wins over auto-discovery.
        if let Some(dir) = preselect {
            if let Some(parent) = dir.parent() {
                if !app.roots.iter().any(|r| r == parent) {
                    app.roots.insert(0, parent.to_path_buf());
                }
                app.root_idx = app.roots.iter().position(|r| r == parent).unwrap_or(0);
            }
        }
        app.reload_instances();
        if online {
            app.fetch_builds();
        }
        app
    }

    fn reload_instances(&mut self) {
        self.instances.clear();
        self.inst_idx = None;
        let Some(root) = self.roots.get(self.root_idx) else {
            return;
        };
        for inst in prism::list_instances(root) {
            let build = state::load(&inst.dir)
                .build
                .or_else(|| state::detect_build_from_changelogs(&inst.minecraft()));
            let has_snapshot = state::has_base_snapshot(&inst.dir);
            self.instances.push(InstanceRow {
                inst,
                build,
                has_snapshot,
            });
        }
        // Default to the GTNH-looking instance if there is an obvious one.
        if let Some(i) = self.instances.iter().position(|r| r.build.is_some()) {
            self.select_instance(i);
        }
    }

    fn select_instance(&mut self, i: usize) {
        self.inst_idx = Some(i);
        self.refresh_default_name();
    }

    fn refresh_default_name(&mut self) {
        if self.name_edited {
            return;
        }
        if let Some(a) = self.builds.get(self.build_idx) {
            self.new_name = format!("GTNH-daily-{}", a.build);
        }
    }

    fn fetch_builds(&mut self) {
        if self.fetching {
            return;
        }
        self.fetching = true;
        self.error = None;
        let token = if self.token.trim().is_empty() {
            None
        } else {
            Some(self.token.trim().to_string())
        };
        worker::spawn_fetch_builds(self.tx.clone(), token, self.variant.clone());
    }

    fn current_instance(&self) -> Option<&InstanceRow> {
        self.inst_idx.and_then(|i| self.instances.get(i))
    }

    fn pump(&mut self, ctx: &egui::Context) {
        let mut repaint = false;
        while let Ok(evt) = self.rx.try_recv() {
            repaint = true;
            match evt {
                Evt::Progress { phase, frac, msg } => {
                    self.phase = phase;
                    self.frac = frac;
                    self.msg = msg;
                }
                Evt::Builds(builds) => {
                    self.fetching = false;
                    self.builds = builds;
                    self.build_idx = 0;
                    self.refresh_default_name();
                }
                Evt::Prepared(prepared) => {
                    self.prepared = Some(*prepared);
                    self.step = Step::Review;
                    self.tab = Tab::Conflicts;
                    self.sel_conflict = 0;
                }
                Evt::Applied(dir) => {
                    self.done_dir = Some(dir);
                    self.step = Step::Done;
                }
                Evt::Failed(e) => {
                    self.fetching = false;
                    self.error = Some(e);
                    if matches!(self.step, Step::Working | Step::Applying) {
                        self.step = if self.prepared.is_some() {
                            Step::Review
                        } else {
                            Step::Setup
                        };
                    }
                }
            }
        }
        if repaint || matches!(self.step, Step::Working | Step::Applying) || self.fetching {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }
    }

    fn start_update(&mut self) {
        let Some(instance) = self.current_instance().map(|r| r.inst.clone()) else {
            return;
        };
        let Some(artifact) = self.builds.get(self.build_idx).cloned() else {
            return;
        };
        let Some(root) = self.roots.get(self.root_idx).cloned() else {
            return;
        };
        let final_name = match prism::unique_dir_name(&root, &self.new_name) {
            Ok(n) => n,
            Err(e) => {
                self.error = Some(e.to_string());
                return;
            }
        };
        self.error = None;
        self.cancel = Cancel::new();
        self.step = Step::Working;
        self.phase = Phase::Download;
        self.frac = 0.0;
        self.msg = "Starting…".into();
        worker::spawn_prepare(
            self.tx.clone(),
            PrepareJob {
                token: if self.token.trim().is_empty() {
                    None
                } else {
                    Some(self.token.trim().to_string())
                },
                instance,
                instances_root: root,
                artifact,
                display_name: self.new_name.clone(),
                final_name,
                cancel: self.cancel.clone(),
            },
        );
    }

    fn start_apply(&mut self) {
        let Some(prepared) = self.prepared.take() else {
            return;
        };
        self.cancel = Cancel::new();
        self.step = Step::Applying;
        self.phase = Phase::Apply;
        self.frac = 0.0;
        let state = InstanceState {
            build: Some(prepared.artifact.build),
            date: Some(prepared.artifact.date.clone()),
            variant: Some(prepared.artifact.variant.clone()),
            artifact_id: Some(prepared.artifact.id),
            updated_at: Some(state::now_iso()),
            updated_from: prepared
                .instance
                .dir
                .file_name()
                .map(|n| n.to_string_lossy().to_string()),
        };
        worker::spawn_apply(
            self.tx.clone(),
            ApplyJob {
                plan: Box::new(prepared.plan),
                prepared_staging: prepared.staging,
                old_instance: prepared.instance.dir,
                instances_root: prepared.instances_root,
                final_name: prepared.final_name,
                display_name: prepared.display_name,
                state,
                download_path: prepared.download_path,
                keep_download: self.keep_download,
                cancel: self.cancel.clone(),
            },
        );
    }

    fn discard(&mut self) {
        if let Some(prepared) = self.prepared.take() {
            worker::discard_staging(&prepared.staging);
        }
        self.step = Step::Setup;
        self.reload_instances();
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.render(ui);
    }
}

impl App {
    /// The whole interface, independent of eframe so it can be driven headlessly.
    pub fn render(&mut self, ui: &mut egui::Ui) {
        self.pump(ui.ctx());
        apply_style(ui.ctx());

        egui::Panel::top("header").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("GTNH Daily Updater");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(step_caption(self.step))
                            .color(self.pal.muted)
                            .size(12.0),
                    );
                });
            });
            ui.add_space(6.0);
        });

        if let Some(err) = self.error.clone() {
            egui::Panel::top("error").show(ui, |ui| {
                egui::Frame::default()
                    .fill(Color32::from_rgb(62, 32, 34))
                    .inner_margin(egui::Margin::same(8))
                    .corner_radius(egui::CornerRadius::same(6))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(err).color(Color32::from_rgb(240, 170, 170)));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui.small_button("Dismiss").clicked() {
                                        self.error = None;
                                    }
                                },
                            );
                        });
                    });
                ui.add_space(4.0);
            });
        }

        match self.step {
            Step::Setup => self.setup_ui(ui),
            Step::Working | Step::Applying => self.progress_ui(ui),
            Step::Review => self.review_ui(ui),
            Step::Done => self.done_ui(ui),
        }
    }
}

/// First few paths in a group, for a hover tooltip.
fn preview(files: &[String]) -> String {
    let mut out: Vec<String> = files.iter().take(12).cloned().collect();
    if files.len() > out.len() {
        out.push(format!("… and {} more", files.len() - out.len()));
    }
    out.join("\n")
}

fn step_caption(step: Step) -> &'static str {
    match step {
        Step::Setup => "1 · Choose what to update",
        Step::Working => "2 · Fetching and comparing",
        Step::Review => "3 · Review changes",
        Step::Applying => "4 · Creating the new instance",
        Step::Done => "Done",
    }
}

fn apply_style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = Color32::from_rgb(23, 24, 28);
        style.visuals.window_fill = Color32::from_rgb(27, 28, 33);
        style.visuals.override_text_color = Some(Color32::from_rgb(224, 226, 232));
    });
}

impl App {
    // ---------------------------------------------------------------- setup
    fn setup_ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("setup-actions").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let ready = self.inst_idx.is_some()
                    && !self.builds.is_empty()
                    && !self.new_name.trim().is_empty();
                if ui
                    .add_enabled(
                        ready,
                        egui::Button::new(RichText::new("Start update").strong()),
                    )
                    .clicked()
                {
                    self.start_update();
                }
                if !ready {
                    ui.label(
                        RichText::new("Pick an instance and a build to continue")
                            .color(self.pal.muted)
                            .size(12.0),
                    );
                }
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    self.section(ui, "Instance to update");
                    if self.roots.len() > 1 {
                        ui.horizontal(|ui| {
                            ui.label("Instances folder");
                            let current = self
                                .roots
                                .get(self.root_idx)
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            let mut changed = None;
                            egui::ComboBox::from_id_salt("root")
                                .width(520.0)
                                .selected_text(current)
                                .show_ui(ui, |ui| {
                                    for (i, root) in self.roots.iter().enumerate() {
                                        if ui
                                            .selectable_label(
                                                i == self.root_idx,
                                                root.display().to_string(),
                                            )
                                            .clicked()
                                        {
                                            changed = Some(i);
                                        }
                                    }
                                });
                            if let Some(i) = changed {
                                self.root_idx = i;
                                self.reload_instances();
                            }
                        });
                    }
                    if self.instances.is_empty() {
                        ui.label(
                            RichText::new(
                                "No Prism instances found. Launch Prism once, or pass an \
                                 instance path on the command line.",
                            )
                            .color(self.pal.warn),
                        );
                    }
                    let mut clicked = None;
                    egui::Frame::default()
                        .fill(self.pal.card_bg)
                        .corner_radius(egui::CornerRadius::same(6))
                        .inner_margin(egui::Margin::same(6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            egui::ScrollArea::vertical()
                                .id_salt("instances")
                                .max_height(190.0)
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for (i, row) in self.instances.iter().enumerate() {
                                        let detail = match row.build {
                                            Some(b) => format!("daily {b}"),
                                            None => "version unknown".to_string(),
                                        };
                                        let extra = if row.has_snapshot { " · merge base saved" } else { "" };
                                        let label = format!("{}\n{detail}{extra}", row.inst.name);
                                        if ui
                                            .selectable_label(Some(i) == self.inst_idx, label)
                                            .clicked()
                                        {
                                            clicked = Some(i);
                                        }
                                    }
                                });
                        });
                    if let Some(i) = clicked {
                        self.select_instance(i);
                    }

                    ui.add_space(10.0);
                    self.section(ui, "Daily build to install");
                    ui.horizontal(|ui| {
                        let mut variant = self.variant.clone();
                        if ui
                            .selectable_label(variant == VARIANT_JAVA17_26, "Java 17–26")
                            .clicked()
                        {
                            variant = VARIANT_JAVA17_26.to_string();
                        }
                        if ui
                            .selectable_label(variant == VARIANT_JAVA8, "Java 8")
                            .clicked()
                        {
                            variant = VARIANT_JAVA8.to_string();
                        }
                        if variant != self.variant {
                            self.variant = variant;
                            self.builds.clear();
                            self.fetch_builds();
                        }
                        ui.add_space(12.0);
                        if self.fetching {
                            ui.spinner();
                            ui.label(RichText::new("Checking GitHub…").color(self.pal.muted));
                        } else {
                            let text = self
                                .builds
                                .get(self.build_idx)
                                .map(|a| format!("{} · {}", a.label(), human_bytes(a.size)))
                                .unwrap_or_else(|| "no builds found".into());
                            let mut pick = None;
                            egui::ComboBox::from_id_salt("build")
                                .width(340.0)
                                .selected_text(text)
                                .show_ui(ui, |ui| {
                                    for (i, a) in self.builds.iter().enumerate() {
                                        if ui
                                            .selectable_label(
                                                i == self.build_idx,
                                                format!("{} · {}", a.label(), human_bytes(a.size)),
                                            )
                                            .clicked()
                                        {
                                            pick = Some(i);
                                        }
                                    }
                                });
                            if let Some(i) = pick {
                                self.build_idx = i;
                                self.refresh_default_name();
                            }
                            if ui.button("Refresh").clicked() {
                                self.fetch_builds();
                            }
                        }
                    });
                    if let (Some(row), Some(a)) =
                        (self.current_instance(), self.builds.get(self.build_idx))
                    {
                        if let Some(b) = row.build {
                            let note = if a.build > b {
                                format!("Updating daily {b} → daily {}.", a.build)
                            } else if a.build == b {
                                format!("This instance is already on daily {b}.")
                            } else {
                                format!("Careful: this would move daily {b} back to {}.", a.build)
                            };
                            ui.label(RichText::new(note).color(self.pal.muted).size(12.0));
                        }
                    }

                    ui.add_space(10.0);
                    self.section(ui, "New instance");
                    ui.horizontal(|ui| {
                        ui.label("Name");
                        if ui.text_edit_singleline(&mut self.new_name).changed() {
                            self.name_edited = true;
                        }
                    });
                    ui.label(
                        RichText::new(
                            "The instance you picked is never modified — a new one is created \
                             next to it.",
                        )
                        .color(self.pal.muted)
                        .size(12.0),
                    );
                    ui.checkbox(
                        &mut self.keep_download,
                        "Keep the downloaded pack zip after updating",
                    );

                    ui.add_space(10.0);
                    egui::CollapsingHeader::new("GitHub token")
                        .default_open(self.token.is_empty())
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(
                                    "Daily builds are GitHub Actions artifacts, which always \
                                     need a token — even though the repository is public. \
                                     A token with `repo` or `actions:read` scope works.",
                                )
                                .color(self.pal.muted)
                                .size(12.0),
                            );
                            ui.horizontal(|ui| {
                                ui.label("Token");
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.token)
                                        .password(true)
                                        .desired_width(360.0),
                                );
                                if ui.button("Detect").clicked() {
                                    if let Some(t) = Gh::discover_token() {
                                        self.token = t;
                                        self.fetch_builds();
                                    } else {
                                        self.error = Some(
                                            "No token found in GH_TOKEN, GITHUB_TOKEN or the gh CLI."
                                                .into(),
                                        );
                                    }
                                }
                            });
                        });
                    ui.add_space(12.0);
                });
        });
    }

    fn section(&self, ui: &mut egui::Ui, title: &str) {
        ui.label(RichText::new(title).strong().size(15.0));
        ui.add_space(4.0);
    }

    // ------------------------------------------------------------- progress
    fn progress_ui(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(40.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(self.phase.label()).size(17.0).strong());
                ui.add_space(10.0);
                ui.allocate_ui(egui::vec2(480.0, 24.0), |ui| {
                    ui.add(
                        egui::ProgressBar::new(self.frac.clamp(0.0, 1.0))
                            .animate(true)
                            .show_percentage(),
                    );
                });
                ui.add_space(6.0);
                ui.label(RichText::new(&self.msg).color(self.pal.muted).size(12.0));
                ui.add_space(20.0);
                if ui.button("Cancel").clicked() {
                    self.cancel.cancel();
                }
            });
        });
    }

    // --------------------------------------------------------------- review
    fn review_ui(&mut self, ui: &mut egui::Ui) {
        let Some(prepared) = &self.prepared else {
            self.step = Step::Setup;
            return;
        };
        let unresolved = prepared.plan.unresolved();
        let conflicts = prepared.plan.conflicts.len();
        let mods_pending = prepared.plan.mods.decisions.len();

        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                let tabs = [
                    (Tab::Conflicts, format!("Config conflicts ({conflicts})")),
                    (Tab::Mods, format!("Mods ({mods_pending})")),
                    (Tab::Files, "Files".to_string()),
                    (Tab::Summary, "Summary".to_string()),
                ];
                for (tab, label) in tabs {
                    if ui.selectable_label(self.tab == tab, label).clicked() {
                        self.tab = tab;
                    }
                }
            });
            ui.add_space(4.0);
        });

        egui::Panel::bottom("review-actions").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .add(egui::Button::new(RichText::new("Create instance").strong()))
                    .clicked()
                {
                    self.start_apply();
                    return;
                }
                if ui.button("Discard").clicked() {
                    self.discard();
                    return;
                }
                if unresolved > 0 {
                    ui.label(
                        RichText::new(format!(
                            "{unresolved} conflict file{} still on the default (pack version)",
                            if unresolved == 1 { "" } else { "s" }
                        ))
                        .color(self.pal.warn)
                        .size(12.0),
                    );
                }
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Conflicts => self.conflicts_ui(ui),
            Tab::Mods => self.mods_ui(ui),
            Tab::Files => self.files_ui(ui),
            Tab::Summary => self.summary_ui(ui),
        });
    }

    fn conflicts_ui(&mut self, ui: &mut egui::Ui) {
        let pack_label = self
            .prepared
            .as_ref()
            .map(|p| format!("Daily {}", p.artifact.build))
            .unwrap_or_else(|| "Pack".into());
        let Some(prepared) = self.prepared.as_mut() else {
            return;
        };
        if prepared.plan.conflicts.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("No config conflicts").size(16.0).strong());
                ui.label(
                    RichText::new(
                        "Every changed config merged cleanly, so your edits and the pack's \
                         updates are both in the new instance.",
                    )
                    .color(self.pal.muted),
                );
            });
            return;
        }

        let filter = self.filter.to_lowercase();
        let matching: Vec<usize> = prepared
            .plan
            .conflicts
            .iter()
            .enumerate()
            .filter(|(_, c)| filter.is_empty() || c.rel.to_lowercase().contains(&filter))
            .map(|(i, _)| i)
            .collect();

        egui::Panel::left("conflict-list")
            .resizable(true)
            .default_size(330.0)
            .show(ui, |ui| {
                ui.add_space(4.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .hint_text("Filter files…")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(4.0);
                egui::ScrollArea::vertical()
                    .id_salt("conflict-files")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for i in &matching {
                            let c = &prepared.plan.conflicts[*i];
                            let mark = if c.reviewed { "✔" } else { "•" };
                            let color = if c.reviewed {
                                self.pal.ok
                            } else {
                                self.pal.warn
                            };
                            let short = c.rel.rsplit('/').next().unwrap_or(&c.rel);
                            let dir = c.rel.strip_suffix(short).unwrap_or("");
                            let selected = self.sel_conflict == *i;
                            let resp = ui.selectable_label(
                                selected,
                                RichText::new(format!("{mark}  {short}")).color(color),
                            );
                            if !dir.is_empty() {
                                ui.label(
                                    RichText::new(format!("      {dir}"))
                                        .size(11.0)
                                        .color(self.pal.muted),
                                );
                            }
                            if resp.clicked() {
                                self.sel_conflict = *i;
                            }
                        }
                    });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(file) = prepared.plan.conflicts.get_mut(self.sel_conflict) {
                merge_ui::editor(ui, file, &self.pal, &pack_label);
            } else {
                ui.label("Select a file on the left.");
            }
        });
    }

    fn mods_ui(&mut self, ui: &mut egui::Ui) {
        let pal_muted = self.pal.muted;
        let pal_warn = self.pal.warn;
        let pal_ok = self.pal.ok;
        let Some(prepared) = self.prepared.as_mut() else {
            return;
        };
        let plan = &mut prepared.plan;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} added · {} updated · {} unchanged",
                            plan.mods.added.len(),
                            plan.mods.updated.len(),
                            plan.mods.unchanged
                        ))
                        .color(pal_muted),
                    );
                });
                ui.add_space(8.0);

                for (kind, title, blurb) in [
                    (
                        ModKind::RemovedByPack,
                        "Mods this build removed",
                        "The pack dropped these on purpose. Keeping one means it stays in your \
                         new instance even though the pack no longer ships it.",
                    ),
                    (
                        ModKind::UserAdded,
                        "Mods you added yourself",
                        "These were never part of the pack, so they are carried over by default.",
                    ),
                ] {
                    let indices: Vec<usize> = plan
                        .mods
                        .decisions
                        .iter()
                        .enumerate()
                        .filter(|(_, d)| d.kind == kind)
                        .map(|(i, _)| i)
                        .collect();
                    if indices.is_empty() {
                        continue;
                    }
                    ui.label(RichText::new(title).strong().size(15.0));
                    ui.label(RichText::new(blurb).color(pal_muted).size(12.0));
                    ui.horizontal(|ui| {
                        if ui.small_button("Keep all").clicked() {
                            for i in &indices {
                                plan.mods.decisions[*i].keep = true;
                            }
                        }
                        if ui.small_button("Drop all").clicked() {
                            for i in &indices {
                                plan.mods.decisions[*i].keep = false;
                            }
                        }
                    });
                    ui.add_space(4.0);
                    for i in indices {
                        let d = &mut plan.mods.decisions[i];
                        ui.horizontal(|ui| {
                            ui.checkbox(&mut d.keep, "");
                            let color = if d.keep { pal_ok } else { pal_warn };
                            ui.label(RichText::new(&d.file).monospace().color(color));
                            ui.label(
                                RichText::new(human_bytes(d.size))
                                    .size(11.0)
                                    .color(pal_muted),
                            );
                            ui.label(
                                RichText::new(if d.keep { "keep" } else { "remove" })
                                    .size(11.0)
                                    .color(pal_muted),
                            );
                        });
                    }
                    ui.add_space(12.0);
                }

                if plan.mods.decisions.is_empty() {
                    ui.label(
                        RichText::new(
                            "Nothing to decide — the new build has every mod your instance has.",
                        )
                        .color(pal_muted),
                    );
                    ui.add_space(8.0);
                }

                egui::CollapsingHeader::new(format!(
                    "Added by this build ({})",
                    plan.mods.added.len()
                ))
                .show(ui, |ui| {
                    for rel in &plan.mods.added {
                        ui.label(RichText::new(rel).monospace().size(12.0).color(pal_muted));
                    }
                });
                egui::CollapsingHeader::new(format!("Updated ({})", plan.mods.updated.len())).show(
                    ui,
                    |ui| {
                        for (from, to) in &plan.mods.updated {
                            ui.label(
                                RichText::new(format!("{from}  →  {to}"))
                                    .monospace()
                                    .size(12.0)
                                    .color(pal_muted),
                            );
                        }
                    },
                );
                ui.add_space(12.0);
            });
    }

    fn files_ui(&mut self, ui: &mut egui::Ui) {
        let pal_muted = self.pal.muted;
        let Some(prepared) = self.prepared.as_mut() else {
            return;
        };
        let plan = &mut prepared.plan;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new("Your data to carry over").strong().size(15.0));
                ui.label(
                    RichText::new(
                        "Saves, screenshots, waypoints, resource packs and shaders — anything \
                         the pack does not ship. Uncheck to leave it behind.",
                    )
                    .color(pal_muted)
                    .size(12.0),
                );
                ui.add_space(4.0);
                for group in plan.carry.iter_mut() {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut group.enabled, "");
                        ui.label(RichText::new(&group.name).monospace());
                        ui.label(
                            RichText::new(format!(
                                "{} · {} files",
                                human_bytes(group.bytes),
                                group.files.len()
                            ))
                            .size(11.0)
                            .color(pal_muted),
                        )
                        .on_hover_text(preview(&group.files));
                    });
                }
                ui.add_space(12.0);

                if !plan.binary_conflicts.is_empty() {
                    ui.label(
                        RichText::new("Files that cannot be line-merged")
                            .strong()
                            .size(15.0),
                    );
                    ui.label(
                        RichText::new(
                            "Both you and the pack changed these, but they are not text. \
                             Pick one side.",
                        )
                        .color(pal_muted)
                        .size(12.0),
                    );
                    ui.add_space(4.0);
                    for bc in plan.binary_conflicts.iter_mut() {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&bc.rel).monospace().size(12.0));
                            if ui
                                .selectable_label(
                                    !bc.take_pack,
                                    format!("Yours ({})", human_bytes(bc.ours_size)),
                                )
                                .clicked()
                            {
                                bc.take_pack = false;
                            }
                            if ui
                                .selectable_label(
                                    bc.take_pack,
                                    format!("Pack ({})", human_bytes(bc.theirs_size)),
                                )
                                .clicked()
                            {
                                bc.take_pack = true;
                            }
                        });
                    }
                    ui.add_space(12.0);
                }

                egui::CollapsingHeader::new(format!(
                    "Merged automatically ({})",
                    plan.auto_merged.len()
                ))
                .show(ui, |ui| {
                    for (rel, _) in &plan.auto_merged {
                        ui.label(RichText::new(rel).monospace().size(12.0).color(pal_muted));
                    }
                });
                egui::CollapsingHeader::new(format!(
                    "Your version kept, pack unchanged ({})",
                    plan.keep_yours.len()
                ))
                .show(ui, |ui| {
                    for rel in &plan.keep_yours {
                        ui.label(RichText::new(rel).monospace().size(12.0).color(pal_muted));
                    }
                });
                ui.add_space(12.0);
            });
    }

    fn summary_ui(&mut self, ui: &mut egui::Ui) {
        let pal_muted = self.pal.muted;
        let pal_warn = self.pal.warn;
        let Some(prepared) = &self.prepared else {
            return;
        };
        let plan = &prepared.plan;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!(
                        "{}  →  {}",
                        prepared.instance.name, prepared.display_name
                    ))
                    .size(16.0)
                    .strong(),
                );
                ui.label(
                    RichText::new(format!(
                        "{} · {}",
                        prepared.artifact.label(),
                        prepared.artifact.variant
                    ))
                    .color(pal_muted),
                );
                ui.add_space(10.0);

                let rows = [
                    (
                        "Config files taken from the pack",
                        plan.take_pack.to_string(),
                    ),
                    (
                        "Config files merged automatically",
                        plan.auto_merged.len().to_string(),
                    ),
                    ("Your version kept", plan.keep_yours.len().to_string()),
                    (
                        "Conflicts needing a choice",
                        plan.conflicts.len().to_string(),
                    ),
                    (
                        "Non-text conflicts",
                        plan.binary_conflicts.len().to_string(),
                    ),
                    ("Identical files", plan.identical.to_string()),
                    ("Mods added", plan.mods.added.len().to_string()),
                    ("Mods updated", plan.mods.updated.len().to_string()),
                    (
                        "Mods the pack removed",
                        plan.mods.removed_count().to_string(),
                    ),
                    ("Your extra mods", plan.mods.extra_count().to_string()),
                    ("User data carried over", human_bytes(plan.carry_bytes())),
                ];
                for (label, value) in rows {
                    ui.horizontal(|ui| {
                        ui.allocate_ui(egui::vec2(280.0, 18.0), |ui| {
                            ui.label(RichText::new(label).color(pal_muted));
                        });
                        ui.label(RichText::new(value).monospace());
                    });
                }

                ui.add_space(12.0);
                let color = if plan.base_missing {
                    pal_warn
                } else {
                    pal_muted
                };
                ui.label(RichText::new(&prepared.base_note).color(color).size(12.0));
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!("Staged at {}", prepared.staging.display()))
                        .color(pal_muted)
                        .size(11.0),
                );
                ui.add_space(12.0);
            });
    }

    // ----------------------------------------------------------------- done
    fn done_ui(&mut self, ui: &mut egui::Ui) {
        let dir = self.done_dir.clone();
        egui::CentralPanel::default().show(ui, |ui| {
            ui.add_space(50.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new("New instance created").size(19.0).strong());
                ui.add_space(8.0);
                if let Some(dir) = &dir {
                    ui.label(RichText::new(dir.display().to_string()).monospace());
                }
                ui.add_space(8.0);
                ui.label(
                    RichText::new(
                        "Your old instance is untouched. Restart Prism if the new instance \
                         does not appear in the list yet.",
                    )
                    .color(self.pal.muted),
                );
                ui.add_space(24.0);
                if ui.button("Update another instance").clicked() {
                    self.done_dir = None;
                    self.step = Step::Setup;
                    self.reload_instances();
                }
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtnh_updater::github::DailyArtifact;
    use gtnh_updater::merge::{self, MergeOutcome};
    use gtnh_updater::plan::{BinaryConflict, CarryGroup, ConflictFile, Plan};

    fn sample_conflict(rel: &str) -> ConflictFile {
        let MergeOutcome::Conflicted(m) = merge::three_way(
            "pollution=true\nnoise=1\n",
            "pollution=false\nnoise=1\n",
            "pollution=maybe\nnoise=1\n",
        ) else {
            panic!("fixture should conflict");
        };
        ConflictFile {
            rel: rel.to_string(),
            merge: m,
            no_base: false,
            reviewed: false,
        }
    }

    fn sample_prepared() -> Prepared {
        let mut plan = Plan {
            take_pack: 12,
            identical: 900,
            ..Default::default()
        };
        plan.auto_merged
            .push(("config/angelica.cfg".into(), "x=1\n".into()));
        plan.keep_yours.push("config/NEI/client.cfg".into());
        plan.conflicts.push(sample_conflict("config/GregTech.cfg"));
        plan.conflicts
            .push(sample_conflict("serverutilities/serverutilities.cfg"));
        plan.binary_conflicts.push(BinaryConflict {
            rel: "servers.dat".into(),
            ours_size: 10,
            theirs_size: 20,
            take_pack: true,
        });
        plan.carry.push(CarryGroup {
            name: "saves".into(),
            files: vec!["saves/world/level.dat".into()],
            bytes: 1234,
            enabled: true,
        });
        plan.mods = gtnh_updater::mods::plan(
            &[("mods/gone-1.0.jar".to_string(), 1u64)]
                .into_iter()
                .collect(),
            &[
                ("mods/gone-1.0.jar".to_string(), 1u64),
                ("mods/mine-2.0.jar".to_string(), 1u64),
            ]
            .into_iter()
            .collect(),
            &[("mods/fresh-1.0.jar".to_string(), 1u64)]
                .into_iter()
                .collect(),
        );
        Prepared {
            plan,
            staging: PathBuf::from("/tmp/GTNH-daily-690.part"),
            artifact: DailyArtifact {
                id: 1,
                run_id: 2,
                name: "GTNH-daily-2026-08-19+690-mmcprism-java17-26.zip".into(),
                size: 700_000_000,
                date: "2026-08-19".into(),
                build: 690,
                variant: "mmcprism-java17-26".into(),
            },
            instance: Instance {
                dir: PathBuf::from("/tmp/GTNH-DAILY"),
                name: "GTNH-DAILY".into(),
            },
            instances_root: PathBuf::from("/tmp"),
            final_name: "GTNH-daily-690".into(),
            display_name: "GTNH-daily-690".into(),
            download_path: None,
            base_note: "Merge base: daily 641 artifact.".into(),
            old_build: Some(641),
        }
    }

    /// Render every screen a few frames. egui panics on bad layout nesting, so
    /// this catches structural mistakes without needing a window.
    #[test]
    fn every_screen_renders() {
        let ctx = egui::Context::default();
        let mut app = App::build(None, false);
        let screens: Vec<(Step, Option<Tab>)> = vec![
            (Step::Setup, None),
            (Step::Working, None),
            (Step::Review, Some(Tab::Conflicts)),
            (Step::Review, Some(Tab::Mods)),
            (Step::Review, Some(Tab::Files)),
            (Step::Review, Some(Tab::Summary)),
            (Step::Applying, None),
            (Step::Done, None),
        ];
        for (step, tab) in screens {
            if step == Step::Review {
                app.prepared = Some(sample_prepared());
            }
            app.step = step;
            if let Some(tab) = tab {
                app.tab = tab;
            }
            app.error = Some("a sample error banner".into());
            app.done_dir = Some(PathBuf::from("/tmp/GTNH-daily-690"));
            for _ in 0..2 {
                let mut out = ctx.run_ui(egui::RawInput::default(), |ui| app.render(ui));
                // Nothing is uploading textures in a headless run.
                out.textures_delta.clear();
            }
        }
    }

    /// Resolving a conflict changes what gets written for that file.
    #[test]
    fn conflict_choices_change_the_output() {
        let mut file = sample_conflict("config/GregTech.cfg");
        assert_eq!(file.merge.render(), "pollution=maybe\nnoise=1\n");
        file.merge.hunks[0].choice = merge::Choice::Ours;
        assert_eq!(file.merge.render(), "pollution=false\nnoise=1\n");
        file.merge.manual = Some("pollution=custom\n".into());
        assert_eq!(file.merge.render(), "pollution=custom\n");
    }
}
