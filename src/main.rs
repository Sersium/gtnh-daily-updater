// No console window when the exe is double-clicked on Windows. The CLI modes
// attach to whatever console launched them, see `attach_console`.
#![cfg_attr(all(not(debug_assertions), windows), windows_subsystem = "windows")]

mod cli;

use eframe::egui;
use gtnh_updater::app;

/// Reconnect stdout/stderr to the console that started us, if there was one.
/// Without this a GUI-subsystem Windows binary prints into the void.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
    // Failure just means we were not launched from a console; nothing to do.
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

#[cfg(not(windows))]
fn attach_console() {}

fn main() {
    // Harmless when there is no parent console (a double-clicked exe), and it
    // has to happen before anything prints.
    attach_console();

    let args = match cli::parse() {
        Ok(Some(args)) => args,
        Ok(None) => return,
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(2);
        }
    };
    let result = match args.mode {
        cli::Mode::Gui(ref preselect) => gui(preselect.clone()).map_err(|e| anyhow::anyhow!("{e}")),
        cli::Mode::Preview(ref screen) => {
            gui_preview(screen.clone()).map_err(|e| anyhow::anyhow!("{e}"))
        }
        _ => cli::run(args),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn gui_preview(screen: String) -> eframe::Result {
    run_gui(Box::new(move |_cc| {
        let mut app = app::App::new(None);
        app.preview_screen(&screen);
        Ok(Box::new(app) as Box<dyn eframe::App>)
    }))
}

fn gui(preselect: Option<std::path::PathBuf>) -> eframe::Result {
    run_gui(Box::new(move |_cc| {
        Ok(Box::new(app::App::new(preselect.clone())) as Box<dyn eframe::App>)
    }))
}

fn run_gui(creator: eframe::AppCreator<'static>) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("GTNH Daily Updater"),
        ..Default::default()
    };
    eframe::run_native("gtnh-updater", options, creator)
}
