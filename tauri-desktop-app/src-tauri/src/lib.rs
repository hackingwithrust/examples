use serde::Serialize;

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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![add, divide, stats])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
