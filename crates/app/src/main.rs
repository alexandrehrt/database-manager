// Release builds on Windows are GUI apps: no console window next to the app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod persist;
mod ui;
mod worker;

fn main() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/cuia-256.png")).ok();
    let mut viewport = eframe::egui::ViewportBuilder::default()
        .with_title("Cuia")
        .with_app_id("cuia")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([640.0, 400.0]);
    if let Some(icon) = icon {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    // The storage name stays "database-manager" so saved window state carries over.
    eframe::run_native("database-manager", options, Box::new(|cc| Ok(Box::new(app::App::new(cc)))))
}
