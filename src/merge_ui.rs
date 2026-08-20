//! The conflict editor: three-way hunks with per-hunk resolution, plus a raw
//! whole-file escape hatch.

use crate::merge::{Choice, Segment};
use crate::plan::ConflictFile;
use eframe::egui;
use egui::{Color32, CornerRadius, RichText, Stroke};

pub struct Palette {
    pub ours_bg: Color32,
    pub ours_accent: Color32,
    pub theirs_bg: Color32,
    pub theirs_accent: Color32,
    pub base_bg: Color32,
    pub card_bg: Color32,
    pub context: Color32,
    pub muted: Color32,
    pub warn: Color32,
    pub ok: Color32,
}

impl Palette {
    pub fn dark() -> Self {
        Self {
            ours_bg: Color32::from_rgb(52, 34, 38),
            ours_accent: Color32::from_rgb(233, 116, 133),
            theirs_bg: Color32::from_rgb(28, 47, 38),
            theirs_accent: Color32::from_rgb(112, 201, 143),
            base_bg: Color32::from_rgb(38, 38, 44),
            card_bg: Color32::from_rgb(30, 31, 36),
            context: Color32::from_rgb(150, 154, 165),
            muted: Color32::from_rgb(120, 124, 134),
            warn: Color32::from_rgb(232, 178, 92),
            ok: Color32::from_rgb(112, 201, 143),
        }
    }
}

/// How much untouched text to show around each conflict before folding it.
const CONTEXT_LINES: usize = 3;
const FOLD_THRESHOLD: usize = 8;

pub fn editor(ui: &mut egui::Ui, file: &mut ConflictFile, pal: &Palette, pack_label: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(&file.rel).monospace().strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if file.merge.manual.is_some() {
                if ui.button("Back to hunks").clicked() {
                    file.merge.manual = None;
                }
            } else {
                if ui.button("Edit whole file").clicked() {
                    file.merge.manual = Some(file.merge.render());
                }
                if ui.button("All pack").clicked() {
                    for h in &mut file.merge.hunks {
                        h.choice = Choice::Theirs;
                    }
                    file.reviewed = true;
                }
                if ui.button("All yours").clicked() {
                    for h in &mut file.merge.hunks {
                        h.choice = Choice::Ours;
                    }
                    file.reviewed = true;
                }
            }
        });
    });

    let subtitle = if file.no_base {
        format!(
            "The original version of this file could not be recovered — pick one side. ({} vs yours)",
            pack_label
        )
    } else {
        format!(
            "{} conflict{} — everything else merged automatically.",
            file.merge.hunks.len(),
            if file.merge.hunks.len() == 1 { "" } else { "s" }
        )
    };
    ui.label(RichText::new(subtitle).color(pal.muted).size(12.0));
    ui.add_space(6.0);

    if let Some(manual) = &mut file.merge.manual {
        egui::ScrollArea::vertical()
            .id_salt("manual-edit")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(manual)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .desired_rows(30),
                );
            });
        file.reviewed = true;
        return;
    }

    egui::ScrollArea::vertical()
        .id_salt("hunks")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let mut hunk_i = 0usize;
            let segments = file.merge.segments.clone();
            let total = file.merge.hunks.len();
            for seg in &segments {
                match seg {
                    Segment::Stable(text) => {
                        if !file.no_base {
                            context_block(ui, text, pal);
                        }
                    }
                    Segment::Conflict { .. } => {
                        if hunk_i < file.merge.hunks.len() {
                            hunk_card(ui, file, hunk_i, total, pal, pack_label);
                        }
                        hunk_i += 1;
                    }
                }
            }
            ui.add_space(12.0);
        });
}

fn context_block(ui: &mut egui::Ui, text: &str, pal: &Palette) {
    if text.trim().is_empty() {
        return;
    }
    let lines: Vec<&str> = text.lines().collect();
    let shown = if lines.len() > FOLD_THRESHOLD {
        let head = lines[..CONTEXT_LINES].join("\n");
        let tail = lines[lines.len() - CONTEXT_LINES..].join("\n");
        format!(
            "{head}\n    … {} unchanged lines …\n{tail}",
            lines.len() - CONTEXT_LINES * 2
        )
    } else {
        lines.join("\n")
    };
    ui.add_space(2.0);
    ui.label(
        RichText::new(shown)
            .monospace()
            .size(12.0)
            .color(pal.context),
    );
    ui.add_space(2.0);
}

fn hunk_card(
    ui: &mut egui::Ui,
    file: &mut ConflictFile,
    idx: usize,
    total: usize,
    pal: &Palette,
    pack_label: &str,
) {
    let choice = file.merge.hunks[idx].choice;
    egui::Frame::default()
        .fill(pal.card_bg)
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .outer_margin(egui::Margin::symmetric(0, 4))
        .stroke(Stroke::new(1.0, Color32::from_rgb(58, 60, 68)))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("Conflict {} of {}", idx + 1, total))
                        .size(12.0)
                        .color(pal.muted),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    for (label, value) in [
                        ("Edit", Choice::Custom),
                        ("Both (pack first)", Choice::BothReversed),
                        ("Both", Choice::Both),
                        ("Pack", Choice::Theirs),
                        ("Yours", Choice::Ours),
                    ] {
                        let selected = choice == value;
                        if ui.selectable_label(selected, label).clicked() {
                            if value == Choice::Custom && file.merge.hunks[idx].custom.is_empty() {
                                file.merge.hunks[idx].custom = file.merge.hunks[idx].resolved();
                            }
                            file.merge.hunks[idx].choice = value;
                            file.reviewed = true;
                        }
                    }
                });
            });
            ui.add_space(4.0);

            let hunk = &file.merge.hunks[idx];
            let (ours, theirs, base) = (hunk.ours.clone(), hunk.theirs.clone(), hunk.base.clone());
            let take_ours = matches!(choice, Choice::Ours | Choice::Both | Choice::BothReversed);
            let take_theirs =
                matches!(choice, Choice::Theirs | Choice::Both | Choice::BothReversed);

            ui.columns(2, |cols| {
                side(
                    &mut cols[0],
                    "Yours",
                    &ours,
                    pal.ours_bg,
                    pal.ours_accent,
                    take_ours,
                    pal,
                );
                side(
                    &mut cols[1],
                    pack_label,
                    &theirs,
                    pal.theirs_bg,
                    pal.theirs_accent,
                    take_theirs,
                    pal,
                );
            });

            if choice == Choice::Custom {
                ui.add_space(6.0);
                ui.label(RichText::new("Result").size(12.0).color(pal.muted));
                ui.add(
                    egui::TextEdit::multiline(&mut file.merge.hunks[idx].custom)
                        .code_editor()
                        .desired_width(f32::INFINITY)
                        .desired_rows(6),
                );
            }

            if !base.trim().is_empty() {
                ui.add_space(4.0);
                egui::CollapsingHeader::new(
                    RichText::new("Original (before either change)")
                        .size(12.0)
                        .color(pal.muted),
                )
                .id_salt(format!("orig-{}-{}", file.rel, idx))
                .default_open(false)
                .show(ui, |ui| {
                    code_block(ui, &base, pal.base_bg, pal.context);
                });
            }
        });
}

fn side(
    ui: &mut egui::Ui,
    title: &str,
    text: &str,
    bg: Color32,
    accent: Color32,
    active: bool,
    pal: &Palette,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(title).size(12.0).strong().color(if active {
            accent
        } else {
            pal.muted
        }));
        if active {
            ui.label(RichText::new("in result").size(11.0).color(pal.muted));
        }
    });
    let fill = if active { bg } else { pal.base_bg };
    let fg = if active {
        Color32::from_rgb(226, 228, 234)
    } else {
        pal.muted
    };
    code_block(ui, text, fill, fg);
}

fn code_block(ui: &mut egui::Ui, text: &str, bg: Color32, fg: Color32) {
    let shown = if text.is_empty() {
        "(nothing)".to_string()
    } else {
        text.trim_end_matches('\n').to_string()
    };
    egui::Frame::default()
        .fill(bg)
        .corner_radius(CornerRadius::same(4))
        .inner_margin(egui::Margin::same(6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(shown).monospace().size(12.0).color(fg));
        });
}
