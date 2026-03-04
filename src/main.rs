/// Todizzy — minimal macOS menu-bar notes app.
///
/// Entry point.  Resolves the data directory and hands off to `app::run_app`.
/// All AppKit work happens on the main thread via the NSApplication run loop.
mod app;
mod editor;
mod gestures;
mod settings;
mod storage;

use std::path::PathBuf;

fn main() {
    // Data directory: ~/Library/Application Support/todizzy/
    let data_dir = data_directory();
    std::fs::create_dir_all(&data_dir).expect("create data dir");

    app::run_app(data_dir);
}

fn data_directory() -> PathBuf {
    // Prefer override for development
    if let Ok(base) = std::env::var("TODIZZY_DATA_DIR") {
        return PathBuf::from(base);
    }

    // macOS canonical location
    let home = std::env::var("HOME").expect("$HOME not set");
    PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("todizzy")
}
