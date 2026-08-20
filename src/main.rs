mod cli;

use eframe::egui;
use gtnh_updater::app;

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
