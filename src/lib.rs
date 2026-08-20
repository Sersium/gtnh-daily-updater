//! Daily GTNH modpack updater for Prism Launcher.
//!
//! The pieces are deliberately separable: `github` finds a build, `pack` reads it,
//! `plan` decides what happens to every file, `merge` does the three-way text
//! merge, and `apply` writes the result into a brand-new instance.

pub mod apply;
pub mod github;
pub mod httpzip;
pub mod merge;
pub mod mods;
pub mod pack;
pub mod plan;
pub mod prism;
pub mod state;
pub mod util;
pub mod worker;
