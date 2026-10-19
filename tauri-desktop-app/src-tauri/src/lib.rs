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
        .invoke_handler(tauri::generate_handler![
            add, divide, stats, create_note, list_notes, update_note, search_notes
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
