use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Mutex;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager, State, WindowEvent};

#[derive(Serialize)]
struct DirEntry {
    name: String,
    path: String,
    is_dir: bool,
}

#[derive(Serialize)]
struct DirListing {
    dir: String,
    parent: Option<String>,
    entries: Vec<DirEntry>,
}

/// List sub-folders and Markdown files in a directory (for the file explorer).
/// If `path` is a file, its parent directory is listed.
#[tauri::command]
fn list_dir(path: String) -> Result<DirListing, String> {
    let p = std::path::PathBuf::from(&path);
    let dir = if p.is_dir() {
        p
    } else {
        p.parent().map(Path::to_path_buf).unwrap_or(p)
    };

    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // skip hidden
        }
        let ep = entry.path();
        let is_dir = ep.is_dir();
        let is_md = ep
            .extension()
            .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"))
            .unwrap_or(false);
        if is_dir || is_md {
            entries.push(DirEntry {
                name,
                path: ep.to_string_lossy().into_owned(),
                is_dir,
            });
        }
    }
    // Folders first, then files; alphabetical, case-insensitive.
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });

    Ok(DirListing {
        dir: dir.to_string_lossy().into_owned(),
        parent: dir.parent().map(|p| p.to_string_lossy().into_owned()),
        entries,
    })
}

/// Holds the file we are currently viewing plus the live filesystem watcher.
#[derive(Default)]
struct AppState {
    current: Mutex<Option<PathBuf>>,
    watcher: Mutex<Option<RecommendedWatcher>>,
    geometry: Mutex<GeometryStore>,
}

// ---------- Window geometry (size, position, maximized) ----------

/// Window placement remembered across launches, in physical pixels. The
/// position/size are always the *normal* (un-maximized) bounds, so a window
/// closed while maximized still un-maximizes back to its previous place.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
struct WindowGeometry {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    maximized: bool,
}

#[derive(Default)]
struct GeometryStore {
    /// Last geometry recorded (restored at startup or saved since).
    last: Option<WindowGeometry>,
    /// Channel to the background thread that writes the geometry file.
    writer: Option<Sender<WindowGeometry>>,
}

const GEOMETRY_FILE: &str = "window-state.json";

/// Reject degenerate sizes (a corrupt file, or 0×0 reported while minimized).
fn sane_size(width: u32, height: u32) -> bool {
    (200..=20_000).contains(&width) && (150..=20_000).contains(&height)
}

fn geometry_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|dir| dir.join(GEOMETRY_FILE))
}

fn load_geometry(path: &Path) -> Option<WindowGeometry> {
    let text = std::fs::read_to_string(path).ok()?;
    let geo: WindowGeometry = serde_json::from_str(&text).ok()?;
    sane_size(geo.width, geo.height).then_some(geo)
}

fn write_geometry(path: &Path, geo: &WindowGeometry) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string(geo) {
        let _ = std::fs::write(path, json);
    }
}

/// The window's current normal bounds (used when nothing was saved yet).
fn current_geometry(window: &tauri::WebviewWindow) -> Option<WindowGeometry> {
    let pos = window.outer_position().ok()?;
    let size = window.inner_size().ok()?;
    Some(WindowGeometry {
        x: pos.x,
        y: pos.y,
        width: size.width,
        height: size.height,
        maximized: false,
    })
}

/// True if the window's title-bar strip overlaps a connected monitor enough to
/// be grabbed — so a monitor that's been unplugged can't strand the window.
fn is_on_screen(window: &tauri::WebviewWindow, geo: &WindowGeometry) -> bool {
    let Ok(monitors) = window.available_monitors() else {
        return false;
    };
    let (x, y, w) = (i64::from(geo.x), i64::from(geo.y), i64::from(geo.width));
    monitors.iter().any(|m| {
        let (mx, my) = (i64::from(m.position().x), i64::from(m.position().y));
        let (mw, mh) = (i64::from(m.size().width), i64::from(m.size().height));
        let overlap_w = (x + w).min(mx + mw) - x.max(mx);
        let overlap_h = (y + 40).min(my + mh) - y.max(my);
        overlap_w >= 100 && overlap_h >= 20
    })
}

/// Apply the saved geometry to the (still hidden) main window, then show it.
/// The window starts hidden (`visible: false`) so it never flashes at the
/// default size/place first.
fn restore_geometry(window: &tauri::WebviewWindow, saved: Option<WindowGeometry>) {
    if let Some(geo) = saved {
        let pos = || tauri::Position::Physical(tauri::PhysicalPosition::new(geo.x, geo.y));
        let size = || tauri::Size::Physical(tauri::PhysicalSize::new(geo.width, geo.height));
        if is_on_screen(window, &geo) {
            // Move first so the size is applied on the target monitor (its DPI),
            // then move again in case the DPI change nudged the window.
            let _ = window.set_position(pos());
            let _ = window.set_size(size());
            let _ = window.set_position(pos());
        } else {
            let _ = window.set_size(size());
        }
        if geo.maximized {
            let _ = window.maximize();
        }
    }
    let _ = window.show();
    let _ = window.set_focus();
}

/// Record the window's geometry after every move / resize.
fn save_geometry(window: &tauri::Window) {
    // A minimized window reports a bogus off-screen position — keep the last state.
    if window.is_minimized().unwrap_or(true) {
        return;
    }
    let Ok(maximized) = window.is_maximized() else {
        return;
    };
    let state = window.state::<AppState>();
    let mut store = state.geometry.lock().unwrap();
    let geo = if maximized {
        // Keep the normal bounds from before maximizing; only flag the state.
        match store.last {
            Some(last) => WindowGeometry {
                maximized: true,
                ..last
            },
            None => return,
        }
    } else {
        let (Ok(pos), Ok(size)) = (window.outer_position(), window.inner_size()) else {
            return;
        };
        if !sane_size(size.width, size.height) {
            return;
        }
        WindowGeometry {
            x: pos.x,
            y: pos.y,
            width: size.width,
            height: size.height,
            maximized: false,
        }
    };
    if store.last == Some(geo) {
        return;
    }
    store.last = Some(geo);
    if let Some(writer) = &store.writer {
        let _ = writer.send(geo);
    }
}

/// Pull a markdown file path out of the process arguments (set when Windows
/// launches us for a `.md` file, e.g. via "Open with").
fn path_from_args() -> Option<PathBuf> {
    std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .find(|p| p.extension().map(|e| e.eq_ignore_ascii_case("md")).unwrap_or(false) && p.exists())
}

/// Whether the app was launched with `--edit` (open straight into edit mode).
#[tauri::command]
fn start_in_edit() -> bool {
    std::env::args().any(|a| a == "--edit")
}

/// Optional `--zoom=<factor>` launch flag (0.0 means "not set").
#[tauri::command]
fn start_zoom() -> f64 {
    std::env::args()
        .find_map(|a| a.strip_prefix("--zoom=").and_then(|v| v.parse::<f64>().ok()))
        .unwrap_or(0.0)
}

/// Returns the file path the app was opened with, if any.
#[tauri::command]
fn get_initial_path(state: State<AppState>) -> Option<String> {
    state
        .current
        .lock()
        .unwrap()
        .as_ref()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Read a markdown file's raw text.
#[tauri::command]
fn read_md(path: String) -> Result<String, String> {
    std::fs::read_to_string(&path).map_err(|e| format!("Failed to read {path}: {e}"))
}

/// Open a file in a brand-new application window (spawns another instance).
#[tauri::command]
fn open_new_window(path: String) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .arg(path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Write text back to a markdown file.
#[tauri::command]
fn write_md(path: String, content: String) -> Result<(), String> {
    std::fs::write(&path, content).map_err(|e| format!("Failed to write {path}: {e}"))
}

/// Begin watching `path`; emits `md-changed` whenever the file is modified.
/// We watch the *parent directory* (non-recursive) because many editors save
/// by replacing the file, which breaks a watch placed directly on the file.
#[tauri::command]
fn watch_file(path: String, app: tauri::AppHandle, state: State<AppState>) -> Result<(), String> {
    let target = PathBuf::from(&path);
    let parent = target
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "File has no parent directory".to_string())?;

    *state.current.lock().unwrap() = Some(target.clone());

    let watched = target.clone();
    let handle = app.clone();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            if matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Any
            ) && event.paths.iter().any(|p| p == &watched)
            {
                let _ = handle.emit("md-changed", watched.to_string_lossy().into_owned());
            }
        }
    })
    .map_err(|e| e.to_string())?;

    watcher
        .watch(&parent, RecursiveMode::NonRecursive)
        .map_err(|e| e.to_string())?;

    // Keep the watcher alive by stashing it in state (replacing any previous one).
    *state.watcher.lock().unwrap() = Some(watcher);
    Ok(())
}

pub fn run() {
    let state = AppState::default();
    if let Some(p) = path_from_args() {
        *state.current.lock().unwrap() = Some(p);
    }

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .setup(|app| {
            let path = geometry_path(app.handle());
            let saved = path.as_deref().and_then(load_geometry);

            // Background writer: a window drag fires many events, so coalesce
            // bursts and write only the newest geometry, off the UI thread.
            let (tx, rx) = std::sync::mpsc::channel::<WindowGeometry>();
            std::thread::spawn(move || {
                while let Ok(mut geo) = rx.recv() {
                    while let Ok(newer) = rx.try_recv() {
                        geo = newer;
                    }
                    if let Some(path) = &path {
                        write_geometry(path, &geo);
                    }
                }
            });

            if let Some(window) = app.get_webview_window("main") {
                {
                    let state = app.state::<AppState>();
                    let mut store = state.geometry.lock().unwrap();
                    store.last = saved.or_else(|| current_geometry(&window));
                    store.writer = Some(tx);
                }
                restore_geometry(&window, saved);
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_)) {
                save_geometry(window);
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_initial_path,
            start_in_edit,
            start_zoom,
            read_md,
            write_md,
            list_dir,
            open_new_window,
            watch_file
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
