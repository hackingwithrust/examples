use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

#[derive(Serialize)]
pub struct Stats {
    count: usize,
    total: i32,
    mean: f64,
}

#[tauri::command]
fn add(first_number: i32, second_number: i32) -> i32 {
    first_number + second_number
}

#[tauri::command]
fn divide(dividend: i32, divisor: i32) -> Result<i32, String> {
    if divisor == 0 {
        Err("cannot divide by zero".into())
    } else {
        Ok(dividend / divisor)
    }
}

#[tauri::command]
fn stats(numbers: Vec<i32>) -> Stats {
    let count = numbers.len();
    let total: i32 = numbers.iter().sum();
    let mean = if count > 0 {
        total as f64 / count as f64
    } else {
        0.0
    };
    Stats { count, total, mean }
}

#[derive(Serialize)]
pub struct Note {
    id: i64,
    title: String,
    body: String,
    created_at: i64,
}

const MIGRATION_1: &str = "
CREATE TABLE notes (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL
);";

const MIGRATION_2: &str = "
CREATE VIRTUAL TABLE notes_fts USING fts5(title, body);
INSERT INTO notes_fts(rowid, title, body) SELECT id, title, body FROM notes;
CREATE TRIGGER notes_ai AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;
CREATE TRIGGER notes_ad AFTER DELETE ON notes BEGIN
    DELETE FROM notes_fts WHERE rowid = old.id;
END;
CREATE TRIGGER notes_au AFTER UPDATE ON notes BEGIN
    DELETE FROM notes_fts WHERE rowid = old.id;
    INSERT INTO notes_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;";

fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        conn.execute_batch(MIGRATION_1)?;
        conn.pragma_update(None, "user_version", 1)?;
    }
    if version < 2 {
        conn.execute_batch(MIGRATION_2)?;
        conn.pragma_update(None, "user_version", 2)?;
    }
    Ok(())
}

#[tauri::command]
async fn create_note(state: State<'_, Mutex<Connection>>, title: String) -> Result<i64, String> {
    let conn = state.lock().map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO notes (title, body, created_at) VALUES (?1, '', ?2)",
        rusqlite::params![title, now],
    )
    .map_err(|e| e.to_string())?;
    Ok(conn.last_insert_rowid())
}

#[tauri::command]
async fn list_notes(state: State<'_, Mutex<Connection>>) -> Result<Vec<Note>, String> {
    let conn = state.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare("SELECT id, title, body, created_at FROM notes ORDER BY id DESC")
        .map_err(|e| e.to_string())?;
    let notes = stmt
        .query_map([], |row| {
            Ok(Note {
                id: row.get(0)?,
                title: row.get(1)?,
                body: row.get(2)?,
                created_at: row.get(3)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<Note>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(notes)
}

#[tauri::command]
async fn update_note(
    state: State<'_, Mutex<Connection>>,
    id: i64,
    title: String,
    body: String,
) -> Result<(), String> {
    let conn = state.lock().map_err(|e| e.to_string())?;
    let updated = conn
        .execute(
            "UPDATE notes SET title = ?1, body = ?2 WHERE id = ?3",
            rusqlite::params![title, body, id],
        )
        .map_err(|e| e.to_string())?;
    if updated == 0 {
        return Err(format!("no note #{id}"));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct SearchHit {
    id: i64,
    title: String,
    snippet: String,
}

fn quote_tokens(query: &str) -> String {
    query
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[tauri::command]
async fn search_notes(
    state: State<'_, Mutex<Connection>>,
    query: String,
) -> Result<Vec<SearchHit>, String> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let match_query = quote_tokens(trimmed);
    let conn = state.lock().map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT notes.id,
                    highlight(notes_fts, 0, '«', '»') AS title,
                    snippet(notes_fts, 1, '«', '»', '…', 12) AS snippet
             FROM notes_fts JOIN notes ON notes.id = notes_fts.rowid
             WHERE notes_fts MATCH ?1
             ORDER BY rank",
        )
        .map_err(|e| e.to_string())?;
    let hits = stmt
        .query_map([&match_query], |row| {
            Ok(SearchHit {
                id: row.get(0)?,
                title: row.get(1)?,
                snippet: row.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<SearchHit>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(hits)
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn open_new_note(app: &AppHandle) {
    show_main(app);
    let _ = app.emit("new-note", ());
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            let dir = app.path().app_data_dir().expect("no app data directory");
            std::fs::create_dir_all(&dir).expect("cannot create the app data directory");
            let conn = Connection::open(dir.join("rustnote.db")).expect("cannot open rustnote.db");
            run_migrations(&conn).expect("migrations failed");
            app.manage(Mutex::new(conn));

            let app_menu = Submenu::with_items(
                app,
                "rustnote",
                true,
                &[
                    &PredefinedMenuItem::about(app, None, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::hide(app, None)?,
                    &PredefinedMenuItem::hide_others(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::quit(app, None)?,
                ],
            )?;
            let new_note_item =
                MenuItem::with_id(app, "new-note", "New Note", true, Some("CmdOrCtrl+N"))?;
            let file_menu = Submenu::with_items(
                app,
                "File",
                true,
                &[
                    &new_note_item,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::close_window(app, None)?,
                ],
            )?;
            let edit_menu = Submenu::with_items(
                app,
                "Edit",
                true,
                &[
                    &PredefinedMenuItem::undo(app, None)?,
                    &PredefinedMenuItem::redo(app, None)?,
                    &PredefinedMenuItem::separator(app)?,
                    &PredefinedMenuItem::cut(app, None)?,
                    &PredefinedMenuItem::copy(app, None)?,
                    &PredefinedMenuItem::paste(app, None)?,
                    &PredefinedMenuItem::select_all(app, None)?,
                ],
            )?;
            let menu = Menu::with_items(app, &[&app_menu, &file_menu, &edit_menu])?;
            app.set_menu(menu)?;

            app.on_menu_event(|app, event| match event.id().as_ref() {
                "new-note" => open_new_note(app),
                "open" => show_main(app),
                "quit" => app.exit(0),
                _ => {}
            });

            let open_item =
                MenuItem::with_id(app, "open", "Open rustnote", true, None::<&str>)?;
            let tray_new_item =
                MenuItem::with_id(app, "new-note", "New Note", true, None::<&str>)?;
            let quit_item =
                MenuItem::with_id(app, "quit", "Quit rustnote", true, None::<&str>)?;
            let tray_menu = Menu::with_items(
                app,
                &[
                    &open_item,
                    &tray_new_item,
                    &PredefinedMenuItem::separator(app)?,
                    &quit_item,
                ],
            )?;
            TrayIconBuilder::with_id("main-tray")
                .icon(
                    app.default_window_icon()
                        .expect("no default window icon")
                        .clone(),
                )
                .tooltip("rustnote")
                .menu(&tray_menu)
                .show_menu_on_left_click(false)
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                })
                .build(app)?;

            app.global_shortcut()
                .on_shortcut("CmdOrCtrl+Shift+N", |app, _shortcut, event| {
                    if event.state == ShortcutState::Pressed {
                        open_new_note(app);
                    }
                })?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add, divide, stats, create_note, list_notes, update_note, search_notes
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
