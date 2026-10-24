//! The main window starts hidden (`visible: false` in tauri.conf.json), so the
//! webview's white background never shows while the page loads. The page
//! shows it once it has painted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tauri::{AppHandle, Manager, Runtime};

#[derive(Default)]
pub struct FirstShow(AtomicBool);

/// How long the window waits for the page before it is shown anyway, so a
/// page that fails to load never leaves an invisible app.
const FIRST_SHOW_FALLBACK: Duration = Duration::from_secs(5);

fn show_first_time<R: Runtime>(app: &AppHandle<R>) {
    if app.state::<FirstShow>().0.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Shows the window after `FIRST_SHOW_FALLBACK` if the page has not.
pub fn start_fallback<R: Runtime>(app: &AppHandle<R>) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_SHOW_FALLBACK);
        show_first_time(&app);
    });
}

/// Called by the page once it has painted and whenever the theme changes: the
/// native window takes the page background (`#rrggbb`), so resizing never
/// uncovers white, and appears the first time.
#[tauri::command]
pub fn window_painted(background: String, app: AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        if let Ok(color) = background.parse::<tauri::window::Color>() {
            let _ = window.set_background_color(Some(color));
        }
    }
    show_first_time(&app);
}
