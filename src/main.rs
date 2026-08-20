mod app;
mod cli;
mod merge_ui;

use eframe::egui;

fn main() {
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
        _ => cli::run(args),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn gui(preselect: Option<std::path::PathBuf>) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 780.0])
            .with_min_inner_size([900.0, 600.0])
            .with_title("GTNH Daily Updater"),
        ..Default::default()
    };
    eframe::run_native(
        "gtnh-updater",
        options,
        Box::new(move |_cc| Ok(Box::new(app::App::new(preselect)))),
    )
}
