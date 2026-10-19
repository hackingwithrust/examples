use std::sync::Mutex;

use rusqlite::Connection;
use serde::Serialize;
use tauri::{Manager, State};

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

fn run_migrations(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < 1 {
        conn.execute_batch(MIGRATION_1)?;
        conn.pragma_update(None, "user_version", 1)?;
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let dir = app.path().app_data_dir().expect("no app data directory");
            std::fs::create_dir_all(&dir).expect("cannot create the app data directory");
            let conn = Connection::open(dir.join("rustnote.db")).expect("cannot open rustnote.db");
            run_migrations(&conn).expect("migrations failed");
            app.manage(Mutex::new(conn));
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![add, divide, stats, create_note, list_notes])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
