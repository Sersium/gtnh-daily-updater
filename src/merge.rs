//! Three-way text merge and the structured conflict model the UI edits.
//!
//! `diffy` produces git-style diff3 output; that gets parsed straight back into
//! segments so each conflict keeps all three sides (yours / original / pack) and
//! can be resolved independently.

use serde::{Deserialize, Serialize};

const OURS_MARK: &str = "<<<<<<<";
const BASE_MARK: &str = "|||||||";
const SPLIT_MARK: &str = "=======";
const THEIRS_MARK: &str = ">>>>>>>";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Segment {
    /// Text both sides agree on (or that only one side touched).
    Stable(String),
    Conflict {
        ours: String,
        base: String,
        theirs: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Choice {
    /// Keep the version from the instance being upgraded.
    Ours,
    /// Take the version the new pack ships.
    Theirs,
    /// Yours first, then the pack's.
    Both,
    /// The pack's first, then yours.
    BothReversed,
    /// Hand-edited text held in `Hunk::custom`.
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hunk {
    pub ours: String,
    pub base: String,
    pub theirs: String,
    pub choice: Choice,
    pub custom: String,
}

impl Hunk {
    pub fn resolved(&self) -> String {
        match self.choice {
            Choice::Ours => self.ours.clone(),
            Choice::Theirs => self.theirs.clone(),
            Choice::Both => join(&self.ours, &self.theirs),
            Choice::BothReversed => join(&self.theirs, &self.ours),
            Choice::Custom => self.custom.clone(),
        }
    }
}

fn join(a: &str, b: &str) -> String {
    if a.is_empty() {
        return b.to_string();
    }
    if b.is_empty() {
        return a.to_string();
    }
    if a.ends_with('\n') {
        format!("{a}{b}")
    } else {
        format!("{a}\n{b}")
    }
}

/// A file's merge state: the stable text around each conflict plus the conflicts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextMerge {
    /// Alternating layout is not assumed; segments are stored in order.
    pub segments: Vec<Segment>,
    pub hunks: Vec<Hunk>,
    /// Set when the user chose to hand-edit the whole file.
    pub manual: Option<String>,
}

impl TextMerge {
    /// The merged file as it stands with the current choices applied.
    pub fn render(&self) -> String {
        if let Some(manual) = &self.manual {
            return manual.clone();
        }
        let mut out = String::new();
        let mut hunk_i = 0;
        for seg in &self.segments {
            match seg {
                Segment::Stable(text) => out.push_str(text),
                Segment::Conflict { .. } => {
                    if let Some(h) = self.hunks.get(hunk_i) {
                        let piece = h.resolved();
                        if !out.is_empty() && !out.ends_with('\n') && !piece.is_empty() {
                            out.push('\n');
                        }
                        out.push_str(&piece);
                    }
                    hunk_i += 1;
                }
            }
        }
        out
    }
}

/// Result of merging one text file.
pub enum MergeOutcome {
    /// Merged cleanly; no user input needed.
    Clean(String),
    /// Needs the conflict editor.
    Conflicted(TextMerge),
}

pub fn three_way(base: &str, ours: &str, theirs: &str) -> MergeOutcome {
    let mut opts = diffy::MergeOptions::new();
    opts.set_conflict_style(diffy::ConflictStyle::Diff3);
    match opts.merge(base, ours, theirs) {
        Ok(merged) => MergeOutcome::Clean(merged),
        Err(with_markers) => MergeOutcome::Conflicted(parse_markers(&with_markers)),
    }
}

/// Split diff3 output back into stable text and three-sided conflicts.
fn parse_markers(text: &str) -> TextMerge {
    let mut segments = Vec::new();
    let mut hunks = Vec::new();
    let mut stable = String::new();

    #[derive(PartialEq)]
    enum Side {
        None,
        Ours,
        Base,
        Theirs,
    }
    let mut side = Side::None;
    let (mut ours, mut base, mut theirs) = (String::new(), String::new(), String::new());

    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.starts_with(OURS_MARK) && side == Side::None {
            if !stable.is_empty() {
                segments.push(Segment::Stable(std::mem::take(&mut stable)));
            }
            side = Side::Ours;
            continue;
        }
        if trimmed.starts_with(BASE_MARK) && side == Side::Ours {
            side = Side::Base;
            continue;
        }
        if trimmed.starts_with(SPLIT_MARK) && (side == Side::Ours || side == Side::Base) {
            side = Side::Theirs;
            continue;
        }
        if trimmed.starts_with(THEIRS_MARK) && side == Side::Theirs {
            segments.push(Segment::Conflict {
                ours: ours.clone(),
                base: base.clone(),
                theirs: theirs.clone(),
            });
            hunks.push(Hunk {
                ours: std::mem::take(&mut ours),
                base: std::mem::take(&mut base),
                theirs: std::mem::take(&mut theirs),
                // Defaulting to the pack keeps an untouched instance tracking upstream.
                choice: Choice::Theirs,
                custom: String::new(),
            });
            side = Side::None;
            continue;
        }
        match side {
            Side::None => stable.push_str(line),
            Side::Ours => ours.push_str(line),
            Side::Base => base.push_str(line),
            Side::Theirs => theirs.push_str(line),
        }
    }
    if !stable.is_empty() {
        segments.push(Segment::Stable(stable));
    }
    TextMerge {
        segments,
        hunks,
        manual: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common case: you tweaked one setting, the pack tweaked a different
    /// one. Both changes end up in the result with nothing to review.
    #[test]
    fn edits_in_different_places_merge_without_conflict() {
        let base = "alpha=1\nbeta=2\ngamma=3\ndelta=4\nepsilon=5\n";
        let ours = "alpha=1\nbeta=2\ngamma=3\ndelta=4\nepsilon=99\n";
        let theirs = "alpha=42\nbeta=2\ngamma=3\ndelta=4\nepsilon=5\n";
        match three_way(base, ours, theirs) {
            MergeOutcome::Clean(text) => {
                assert_eq!(text, "alpha=42\nbeta=2\ngamma=3\ndelta=4\nepsilon=99\n");
            }
            MergeOutcome::Conflicted(_) => panic!("should merge cleanly"),
        }
    }

    /// A line you appended right after a line the pack changed counts as a
    /// collision, exactly as `git merge-file` reports it.
    #[test]
    fn adjacent_edits_conflict_like_git_does() {
        let base = "a=1\nb=2\n";
        let ours = "a=1\nb=2\nc=3\n";
        let theirs = "a=1\nb=99\n";
        let MergeOutcome::Conflicted(m) = three_way(base, ours, theirs) else {
            panic!("expected a conflict");
        };
        assert_eq!(m.hunks.len(), 1);
        assert_eq!(m.hunks[0].ours, "b=2\nc=3\n");
        assert_eq!(m.hunks[0].theirs, "b=99\n");
    }

    #[test]
    fn same_line_edits_conflict_and_keep_all_sides() {
        let base = "x=1\n";
        let ours = "x=2\n";
        let theirs = "x=3\n";
        let MergeOutcome::Conflicted(mut m) = three_way(base, ours, theirs) else {
            panic!("expected a conflict");
        };
        assert_eq!(m.hunks.len(), 1);
        assert_eq!(m.hunks[0].ours, "x=2\n");
        assert_eq!(m.hunks[0].base, "x=1\n");
        assert_eq!(m.hunks[0].theirs, "x=3\n");
        assert_eq!(m.render(), "x=3\n");
        m.hunks[0].choice = Choice::Ours;
        assert_eq!(m.render(), "x=2\n");
        m.hunks[0].choice = Choice::Both;
        assert_eq!(m.render(), "x=2\nx=3\n");
    }

    #[test]
    fn stable_text_around_conflicts_survives() {
        let base = "head\nmid\ntail\n";
        let ours = "head\nMINE\ntail\n";
        let theirs = "head\nPACK\ntail\n";
        let MergeOutcome::Conflicted(m) = three_way(base, ours, theirs) else {
            panic!("expected a conflict");
        };
        assert_eq!(m.render(), "head\nPACK\ntail\n");
    }
}
