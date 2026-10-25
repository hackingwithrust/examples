//! rlog — a terminal UI that follows a log file and watches the process table.
//!
//! Step 5 of the track: the fixed demo data is gone. The log pane tails a real
//! file — it remembers its offset, picks up whatever grew, and starts over when
//! the file is truncated underneath it — and the loop runs on schedules: one
//! tick for events, one for the file, and a dirty flag so the screen is redrawn
//! only when something actually changed. The process table is still the demo
//! rows of step 4; step 6 reads the real one.

use std::collections::VecDeque;
use std::env;
use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState};

/// How long the loop waits for an event before going around again. One tick of
/// the program's clock: short enough that a keypress feels immediate, long
/// enough that an idle program wakes four times a second instead of spinning.
const TICK: Duration = Duration::from_millis(250);

/// How often the log file is re-read. It matches the tick here, but it is its
/// own constant because it is its own decision: a log is cheap to poll (one
/// `stat`, one short read) and a line that appears a quarter of a second late
/// is still live. The process table of step 6 gets a slower schedule, because
/// reading it costs real work.
const LOG_POLL: Duration = Duration::from_millis(250);

/// The most bytes one poll reads out of the file. A log that grew by gigabytes
/// since the last look is drained over several polls rather than pulled into
/// memory in one go; the offset keeps the place.
const LOG_READ_LIMIT: u64 = 64 * 1024;

/// How many lines the tail keeps. A pane shows a couple of dozen at a time; a
/// thousand is more history than the screen can scroll back through, and the
/// bound is what stops a busy log from growing the program without end.
const LOG_CAPACITY: usize = 1000;

/// At or above this width the panes sit side by side; below it they stack.
/// 80 columns is the width a terminal opens at before anyone resizes it, so
/// the first thing a reader sees is the stacked layout until the window is
/// dragged wider.
const WIDE: u16 = 80;

/// How many lines one notch of the wheel scrolls. Three traverses a long log
/// without turning the wheel into a paging key.
const SCROLL_STEP: isize = 3;

/// The file `rlog` follows when no path is given.
const DEFAULT_LOG: &str = "./app.log";

/// The process table until step 6 reads the real one: pid, command, cpu
/// percent, resident memory percent. Fourteen rows on purpose — more than the
/// stacked layout can show at once, so the table has something to scroll.
const PROCESSES: &[(&str, &str, f64, f64)] = &[
    ("1", "systemd", 0.0, 0.3),
    ("381", "launchd", 0.2, 0.5),
    ("742", "WindowServer", 3.1, 2.4),
    ("1204", "rlog", 1.4, 0.9),
    ("1583", "cargo", 12.7, 1.1),
    ("1590", "rustc", 41.3, 3.8),
    ("1602", "rustc", 38.9, 3.6),
    ("1711", "firefox", 8.2, 12.4),
    ("1890", "Code Helper", 2.0, 6.1),
    ("2044", "node", 0.9, 4.2),
    ("2287", "tmux", 0.0, 0.2),
    ("2410", "zsh", 0.0, 0.1),
    ("2533", "ssh", 0.1, 0.2),
    ("2680", "top", 0.0, 0.1),
];

/// The whole state of the program. Steps 6 and 7 grow it — the real process
/// table, the selection, the history — and `draw` stays a pure function of it.
struct App {
    /// Frames actually sent to the terminal. Not the same as the number of
    /// loop passes: an idle program makes thousands of those and one of these.
    draws: u64,
    /// Set whenever anything visible changed; the loop clears it as it draws.
    dirty: bool,
    /// The last key press, `None` until the first one arrives.
    last_key: Option<KeyEvent>,
    /// The last mouse event, `None` until the mouse does something.
    last_mouse: Option<MouseEvent>,
    /// Columns and rows, as of the last resize.
    size: (u16, u16),
    /// The file being followed.
    log_path: PathBuf,
    /// The tail itself: the last lines read, oldest first, bounded by
    /// `LOG_CAPACITY`.
    log_lines: VecDeque<String>,
    /// How far into the file the tail has read. Everything before it has been
    /// shown; everything after it has not been read yet.
    log_offset: u64,
    /// How many times the file was truncated underneath the program.
    log_truncations: u64,
    /// The file could not be opened on the last poll. Not an error — a log
    /// that has not been created yet is a normal thing to wait for.
    log_missing: bool,
    /// The first log line on screen. `ListState` also carries a selection,
    /// which stays `None` until step 6.
    log: ListState,
    /// The first process row on screen. `TableState` likewise has no
    /// selection yet — step 6 adds one.
    table: TableState,
}

fn main() -> io::Result<()> {
    // The file to follow is the first argument; `./app.log` is the default so
    // a bare `cargo run` has something to tail.
    let log_path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_LOG));

    // A panic while the terminal is in raw mode leaves it unusable: every line
    // the shell prints afterwards lands wherever the cursor happens to sit.
    // Restore the terminal first, then let the default hook print the panic.
    let default_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = leave_full_screen();
        default_panic_hook(info);
    }));

    enable_raw_mode()?;
    // Mouse tracking is a terminal mode like the alternate screen: the program
    // asks for it, and it has to give it back on the way out.
    execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    // The size starts from the terminal itself; `Event::Resize` keeps it
    // current after that.
    let mut app = App {
        draws: 0,
        dirty: true,
        last_key: None,
        last_mouse: None,
        size: crossterm::terminal::size()?,
        log_path,
        log_lines: VecDeque::new(),
        log_offset: 0,
        log_truncations: 0,
        log_missing: false,
        log: ListState::default(),
        table: TableState::default(),
    };
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let result = run(&mut terminal, &mut app);

    leave_full_screen()?;
    result
}

/// Undo what `main` set up: mouse tracking off, the alternate screen away, then
/// raw mode. The order matters — raw mode is what makes the escape sequences
/// arrive intact, so it goes last.
fn leave_full_screen() -> io::Result<()> {
    execute!(
        io::stdout(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    disable_raw_mode()
}

/// The render loop. Three things happen each pass, in this order: the log file
/// is polled if its interval has come around, the frame is drawn if anything is
/// dirty, and then the loop waits one tick for an event. `q`, `Esc` and
/// `Ctrl-C` end the loop and give the terminal back; every other event lands
/// in the state, where the next frame reports it.
fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    // Due immediately, so the first frame already shows the file as it stands.
    let mut log_due = Instant::now();

    loop {
        if Instant::now() >= log_due {
            poll_log(app)?;
            log_due = Instant::now() + LOG_POLL;
        }

        if app.dirty {
            app.dirty = false;
            app.draws += 1;
            terminal.draw(|frame| draw(frame, app))?;
        }

        if event::poll(TICK)? {
            if !apply(app, event::read()?) {
                break;
            }
        }
    }
    Ok(())
}

/// Fold one event into the state. Returns `false` when the event is one that
/// ends the program, so the caller can break out of the loop.
///
/// Every arm that a reader can see sets `dirty`: the status line reports the
/// last key and mouse event, and a resize reflows every pane. An event that
/// changes nothing visible — a key release, say — leaves the flag alone.
fn apply(app: &mut App, event: Event) -> bool {
    match event {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Press {
                if is_quit(key) {
                    return false;
                }
                app.last_key = Some(key);
                app.dirty = true;
            }
        }
        Event::Mouse(mouse) => {
            app.last_mouse = Some(mouse);
            scroll_wheel(app, mouse);
            app.dirty = true;
        }
        Event::Resize(w, h) => {
            app.size = (w, h);
            app.dirty = true;
        }
        // Focus and paste events only arrive if the program asks for them with
        // their own commands; rlog asks for neither, so this arm is unreachable
        // in practice.
        _ => {}
    }
    true
}

/// Read whatever the file has grown by since the last poll, and notice the two
/// things that can happen to a file underneath a running program: it can
/// vanish, and it can be truncated.
///
/// The file is opened fresh each poll rather than held open. A held handle
/// would follow the old inode after the file is replaced, and the program
/// would tail a file nobody can see any more; re-opening is what makes a
/// replaced file show up as new contents at the same path.
fn poll_log(app: &mut App) -> io::Result<()> {
    let mut file = match File::open(&app.log_path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            // Waiting for a log that does not exist yet is not a failure. The
            // tail and the offset stay as they are, and the status line says
            // the file is missing until it appears.
            if !app.log_missing {
                app.log_missing = true;
                app.dirty = true;
            }
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    if app.log_missing {
        // It came back: whatever grew while it was away is read below.
        app.log_missing = false;
        app.dirty = true;
    }

    let len = file.metadata()?.len();
    if len < app.log_offset {
        // Shorter than where the tail stopped reading: the file was truncated
        // (or replaced by a shorter one). Start over from the beginning, and
        // pull the scroll offset back with the emptied tail.
        app.log_offset = 0;
        app.log_lines.clear();
        app.log_truncations += 1;
        clamp_log(app);
        app.dirty = true;
    }
    if len == app.log_offset {
        return Ok(());
    }

    file.seek(SeekFrom::Start(app.log_offset))?;
    let mut chunk = Vec::new();
    // `take` caps the read at `LOG_READ_LIMIT` bytes; a file that grew by more
    // than that since the last poll is finished on the next one.
    (&file).take(LOG_READ_LIMIT).read_to_end(&mut chunk)?;

    // Only the bytes up to the last newline are lines. A writer mid-line
    // leaves a partial line at the end of the chunk, and consuming it would
    // advance the offset past bytes that do not make a line yet — the rest
    // arrives on the next poll and the two halves are joined there.
    let usable = complete_len(&chunk);
    app.log_offset += usable as u64;
    for line in String::from_utf8_lossy(&chunk[..usable]).lines() {
        push_log(app, line.to_string());
    }
    if usable > 0 {
        app.dirty = true;
    }
    clamp_log(app);
    Ok(())
}

/// How many bytes at the front of `chunk` form whole lines: everything through
/// the last newline. Zero when there is no newline at all.
fn complete_len(chunk: &[u8]) -> usize {
    match chunk.iter().rposition(|byte| *byte == b'\n') {
        Some(last_newline) => last_newline + 1,
        None => 0,
    }
}

/// Append one line to the tail, dropping the oldest when the bound is reached.
/// Dropping the front moves the window: the same lines are one line earlier in
/// the buffer, so the offset follows them down.
fn push_log(app: &mut App, line: String) {
    if app.log_lines.len() == LOG_CAPACITY {
        app.log_lines.pop_front();
        *app.log.offset_mut() = app.log.offset().saturating_sub(1);
    }
    app.log_lines.push_back(line);
}

/// Pull the log's scroll offset back into range after its contents changed.
/// Truncating the file empties the tail, and a tail that shrank underneath a
/// scrolled pane would otherwise leave the offset pointing past the end.
fn clamp_log(app: &mut App) {
    let visible = log_visible(app.size);
    let last = app.log_lines.len().saturating_sub(visible);
    let offset = app.log.offset_mut();
    if *offset > last {
        *offset = last;
        app.dirty = true;
    }
}

/// Split the screen into the three panes: the process table, the log list and
/// the one-row status line. Wide terminals put the two panes side by side;
/// narrow ones stack them, the table above the log.
///
/// The status line is `Length(1)`: it takes its row first, and the panes share
/// what is left, so a squeezed pane can never push it off the screen.
fn layout(area: Rect) -> [Rect; 3] {
    let [main, status] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    if area.width >= WIDE {
        let [table, log] =
            Layout::horizontal([Constraint::Percentage(38), Constraint::Fill(1)]).areas(main);
        [table, log, status]
    } else {
        // The table is `Length(11)` — header, eight processes and the border —
        // and the log takes the rest. A `Min(11)` floor would not do here: the
        // solver shares the slack beyond the minimum with the `Fill` pane, so
        // the table would keep growing as the terminal grows taller.
        let [table, log] =
            Layout::vertical([Constraint::Length(11), Constraint::Fill(1)]).areas(main);
        [table, log, status]
    }
}

/// How many log lines the pane can show at the current size: the border takes
/// a row off the top and one off the bottom.
fn log_visible(size: (u16, u16)) -> usize {
    let [_, log_area, _] = layout(Rect::new(0, 0, size.0, size.1));
    log_area.height.saturating_sub(2) as usize
}

/// A wheel notch scrolls the pane the pointer is over. The layout is the hit
/// map: the same rects `draw` renders into decide which scroll offset moves,
/// so the two can never disagree about where a pane is.
fn scroll_wheel(app: &mut App, mouse: MouseEvent) {
    let delta = match mouse.kind {
        MouseEventKind::ScrollUp => -SCROLL_STEP,
        MouseEventKind::ScrollDown => SCROLL_STEP,
        _ => return,
    };
    let [table_area, log_area, _] = layout(Rect::new(0, 0, app.size.0, app.size.1));
    let at = Position::new(mouse.column, mouse.row);
    if table_area.contains(at) {
        // The table spends its first inner row on the header, so the rows
        // that fit are three fewer than the pane is tall.
        let visible = table_area.height.saturating_sub(3) as usize;
        scroll_offset(app.table.offset_mut(), PROCESSES.len(), visible, delta);
    } else {
        let visible = log_area.height.saturating_sub(2) as usize;
        scroll_offset(app.log.offset_mut(), app.log_lines.len(), visible, delta);
    }
}

/// Move a scroll offset by `delta` lines, clamped at both ends. The upper
/// clamp keeps the last line on screen: the offset never passes
/// `len - visible`, and when the pane is taller than the content that is zero
/// — a pane that just grew taller pulls a stale offset back with it.
fn scroll_offset(offset: &mut usize, len: usize, visible: usize, delta: isize) {
    let last = len.saturating_sub(visible) as isize;
    *offset = (*offset as isize + delta).clamp(0, last) as usize;
}

/// The whole frame: three panes and the status line, laid out from the
/// terminal's current size. Both states are `Copy`, so `draw` takes the app by
/// shared reference and renders from copies — the offset the widget writes
/// back while rendering lands on the copy, and the state changes only when an
/// event says so.
fn draw(frame: &mut Frame, app: &App) {
    let [table_area, log_area, status_area] = layout(frame.area());

    let rows = PROCESSES.iter().map(|(pid, command, cpu, mem)| {
        Row::new(vec![
            Cell::from(*pid),
            Cell::from(*command),
            Cell::from(Line::from(format!("{cpu:.1}")).alignment(Alignment::Right)),
            Cell::from(Line::from(format!("{mem:.1}")).alignment(Alignment::Right)),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(5),
            Constraint::Length(5),
        ],
    )
    .header(Row::new(vec![
        Cell::from("PID"),
        Cell::from("COMMAND"),
        Cell::from(Line::from("CPU%").alignment(Alignment::Right)),
        Cell::from(Line::from("MEM%").alignment(Alignment::Right)),
    ]))
    .block(Block::bordered().title(format!(
        " processes [{}/{}] ",
        app.table.offset() + 1,
        PROCESSES.len()
    )));
    let mut table_state = app.table;
    frame.render_stateful_widget(table, table_area, &mut table_state);

    let log = List::new(app.log_lines.iter().map(|line| ListItem::new(line.as_str()))).block(
        Block::bordered().title(format!(" log {} ", log_title(app))),
    );
    let mut log_state = app.log;
    frame.render_stateful_widget(log, log_area, &mut log_state);

    frame.render_widget(
        Paragraph::new(Line::from(format!(
            "draw {} · {} · key {} · mouse {} · {}x{}",
            app.draws,
            describe_log(app),
            describe_key(app.last_key.as_ref()),
            describe_mouse(app.last_mouse.as_ref()),
            app.size.0,
            app.size.1,
        ))),
        status_area,
    );
}

/// The log pane's title: where the tail stands, and how many times it has been
/// restarted. An empty tail says so rather than claiming a first line.
fn log_title(app: &App) -> String {
    if app.log_lines.is_empty() {
        return "empty".to_string();
    }
    let mut title = format!("[{}/{}]", app.log.offset() + 1, app.log_lines.len());
    if app.log_truncations > 0 {
        title.push_str(&format!(" trunc {}", app.log_truncations));
    }
    title
}

/// Name the log pane for the status line: where the tail stands, or that the
/// file is not there.
fn describe_log(app: &App) -> String {
    if app.log_missing {
        "log missing".to_string()
    } else {
        format!("log {}@{}B", app.log_lines.len(), app.log_offset)
    }
}

/// The keys that end the program. `Ctrl-C` arrives as a key event in raw mode,
/// not as a signal, so it has to be handled here or not at all.
fn is_quit(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
}

/// Name a key the way the status line shows it. Modifiers come first, the key
/// itself second; `None` before the first press.
fn describe_key(key: Option<&KeyEvent>) -> String {
    let Some(key) = key else {
        return "(none yet)".to_string();
    };
    let mut name = match key.code {
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "enter".to_string(),
        KeyCode::Esc => "esc".to_string(),
        KeyCode::Backspace => "backspace".to_string(),
        KeyCode::Tab => "tab".to_string(),
        KeyCode::Up => "up".to_string(),
        KeyCode::Down => "down".to_string(),
        KeyCode::Left => "left".to_string(),
        KeyCode::Right => "right".to_string(),
        KeyCode::PageUp => "pgup".to_string(),
        KeyCode::PageDown => "pgdn".to_string(),
        KeyCode::Home => "home".to_string(),
        KeyCode::End => "end".to_string(),
        other => format!("{other:?}").to_lowercase(),
    };
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        name = format!("ctrl-{name}");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        name = format!("alt-{name}");
    }
    name
}

/// Name a mouse event for the status line: what happened, then where. Both
/// coordinates are zero-based, counted from the top-left cell.
fn describe_mouse(mouse: Option<&MouseEvent>) -> String {
    let Some(mouse) = mouse else {
        return "(none yet)".to_string();
    };
    let kind = match mouse.kind {
        MouseEventKind::Down(button) => format!("press {}", button_name(button)),
        MouseEventKind::Up(button) => format!("release {}", button_name(button)),
        MouseEventKind::Drag(button) => format!("drag {}", button_name(button)),
        MouseEventKind::Moved => "move".to_string(),
        MouseEventKind::ScrollUp => "scroll up".to_string(),
        MouseEventKind::ScrollDown => "scroll down".to_string(),
        MouseEventKind::ScrollLeft => "scroll left".to_string(),
        MouseEventKind::ScrollRight => "scroll right".to_string(),
    };
    format!("{} at {},{}", kind, mouse.column, mouse.row)
}

fn button_name(button: MouseButton) -> &'static str {
    match button {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use ratatui::backend::TestBackend;

    use super::*;

    /// An app pointed at a scratch file, the state `main` builds at startup.
    fn app(log_path: PathBuf) -> App {
        App {
            draws: 0,
            dirty: true,
            last_key: None,
            last_mouse: None,
            size: (100, 30),
            log_path,
            log_lines: VecDeque::new(),
            log_offset: 0,
            log_truncations: 0,
            log_missing: false,
            log: ListState::default(),
            table: TableState::default(),
        }
    }

    /// A scratch file with its own name, so parallel tests do not share one.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rlog-test-{name}.log"))
    }

    #[test]
    fn a_chunk_ending_in_a_newline_is_consumed_whole() {
        assert_eq!(complete_len(b"one\ntwo\n"), 8);
        assert_eq!(complete_len(b"one\ntwo"), 4);
        assert_eq!(complete_len(b""), 0);
    }

    #[test]
    fn a_partial_line_waits_for_the_next_poll() {
        // A writer halfway through a line: the bytes are in the chunk, but
        // they are not a line yet, so the offset does not move past them.
        let mut app = app(scratch("partial"));
        std::fs::write(&app.log_path, b"whole line\nhalf a li").unwrap();
        poll_log(&mut app).unwrap();
        assert_eq!(app.log_lines.len(), 1);
        assert_eq!(app.log_offset, 11);

        // The rest of the line arrives on the next poll and the halves join.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&app.log_path)
            .unwrap();
        file.write_all(b"ne\n").unwrap();
        poll_log(&mut app).unwrap();
        assert_eq!(app.log_lines.len(), 2);
        assert_eq!(app.log_offset, 23);
        assert_eq!(app.log_lines[1], "half a line");
        std::fs::remove_file(&app.log_path).unwrap();
    }

    #[test]
    fn a_growing_file_is_read_from_where_the_tail_stopped() {
        let path = scratch("grow");
        std::fs::write(&path, b"first\n").unwrap();
        let mut app = app(path.clone());
        poll_log(&mut app).unwrap();
        assert_eq!(app.log_lines.len(), 1);
        assert_eq!(app.log_offset, 6);

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"second\nthird\n").unwrap();
        poll_log(&mut app).unwrap();
        assert_eq!(app.log_lines.len(), 3);
        assert_eq!(app.log_offset, 19);
        assert_eq!(app.log_lines[2], "third");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_truncated_file_starts_over() {
        let path = scratch("truncate");
        std::fs::write(&path, b"alpha\nbeta\n").unwrap();
        let mut app = app(path.clone());
        poll_log(&mut app).unwrap();
        assert_eq!(app.log_lines.len(), 2);

        // The file is emptied and refilled with something shorter: the tail
        // notices the length went backwards and reads from the top again.
        std::fs::write(&path, b"gamma\n").unwrap();
        poll_log(&mut app).unwrap();
        assert_eq!(app.log_lines.len(), 1);
        assert_eq!(app.log_lines[0], "gamma");
        assert_eq!(app.log_offset, 6);
        assert_eq!(app.log_truncations, 1);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_missing_file_is_waited_for_not_an_error() {
        let mut app = app(scratch("missing"));
        poll_log(&mut app).unwrap();
        assert!(app.log_missing);
        assert!(app.dirty);
        assert_eq!(app.log_lines.len(), 0);

        // The file appears with contents: the next poll picks them up.
        std::fs::write(&app.log_path, b"late line\n").unwrap();
        poll_log(&mut app).unwrap();
        assert!(!app.log_missing);
        assert_eq!(app.log_lines.len(), 1);
        std::fs::remove_file(&app.log_path).unwrap();
    }

    #[test]
    fn the_tail_is_bounded_and_the_window_follows() {
        let mut app = app(scratch("capacity"));
        for i in 1..=LOG_CAPACITY + 5 {
            push_log(&mut app, format!("line {i}"));
        }
        assert_eq!(app.log_lines.len(), LOG_CAPACITY);
        assert_eq!(app.log_lines.front().unwrap(), "line 6");

        // A pane scrolled five lines down keeps showing the same lines: the
        // front fell off, so the same window is one line earlier in the buffer.
        *app.log.offset_mut() = 5;
        push_log(&mut app, "one more".to_string());
        assert_eq!(app.log.offset(), 4);
        assert_eq!(app.log_lines[4], "line 11");
    }

    #[test]
    fn a_shrunk_tail_pulls_the_offset_back() {
        let mut app = app(scratch("clamp"));
        for i in 1..=30 {
            push_log(&mut app, format!("line {i}"));
        }
        // At 100x30 the pane shows 27 of the 30 lines, so an offset of 2 is
        // legal and stays put.
        *app.log.offset_mut() = 2;
        app.dirty = false;
        clamp_log(&mut app);
        assert_eq!(app.log.offset(), 2);
        assert!(!app.dirty);

        // A truncation empties the tail: the offset comes back with it, and
        // that is a change the frame has to show.
        app.log_lines.clear();
        clamp_log(&mut app);
        assert_eq!(app.log.offset(), 0);
        assert!(app.dirty);
    }

    #[test]
    fn a_keypress_leaves_the_frame_dirty_and_q_ends_the_loop() {
        let mut app = app(scratch("apply-key"));
        app.dirty = false;
        assert!(apply(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
        ));
        assert!(app.dirty);
        assert_eq!(app.last_key.unwrap().code, KeyCode::Char('j'));

        // Quitting returns false without needing another frame: everything
        // the final state has to say has already been drawn.
        app.dirty = false;
        assert!(!apply(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        ));
        assert!(!app.dirty);
    }

    #[test]
    fn a_wheel_notch_scrolls_the_log_pane() {
        let mut app = app(scratch("apply-wheel"));
        for i in 1..=30 {
            push_log(&mut app, format!("line {i}"));
        }
        app.dirty = false;
        // At 100x30 the log pane starts at column 38; a notch there moves the
        // log's offset, not the table's.
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 40,
            row: 4,
            modifiers: KeyModifiers::NONE,
        };
        assert!(apply(&mut app, Event::Mouse(mouse)));
        assert_eq!(app.log.offset(), SCROLL_STEP as usize);
        assert_eq!(app.table.offset(), 0);
        assert!(app.dirty);
    }

    #[test]
    fn a_resize_reflows_and_dirties_the_frame() {
        let mut app = app(scratch("apply-resize"));
        app.dirty = false;
        assert!(apply(&mut app, Event::Resize(60, 24)));
        assert_eq!(app.size, (60, 24));
        assert!(app.dirty);
    }

    #[test]
    fn a_wide_terminal_puts_the_panes_side_by_side() {
        let [table, log, status] = layout(Rect::new(0, 0, 100, 30));
        assert_eq!(table, Rect::new(0, 0, 38, 29));
        assert_eq!(log, Rect::new(38, 0, 62, 29));
        assert_eq!(status, Rect::new(0, 29, 100, 1));
    }

    #[test]
    fn a_narrow_terminal_stacks_the_panes() {
        let [table, log, status] = layout(Rect::new(0, 0, 60, 24));
        assert_eq!(table, Rect::new(0, 0, 60, 11));
        assert_eq!(log, Rect::new(0, 11, 60, 12));
        assert_eq!(status, Rect::new(0, 23, 60, 1));
    }

    #[test]
    fn a_pane_too_small_for_the_log_takes_nothing() {
        // Twelve rows leave the table its eleven after the status line takes
        // its one; the log's rect is empty, and rendering an empty rect draws
        // nothing rather than complaining.
        let [table, log, _] = layout(Rect::new(0, 0, 40, 12));
        assert_eq!(table, Rect::new(0, 0, 40, 11));
        assert_eq!(log, Rect::new(0, 11, 40, 0));
    }

    #[test]
    fn the_frame_renders_at_every_size() {
        // The widgets get zero-height areas at the small end; rendering one
        // is allowed to draw nothing, not to panic. The sweep runs twice, with
        // a full tail and with an empty one, because a pane with no lines yet
        // is the state the program starts in.
        let mut app = app(scratch("render"));
        for filled in [false, true] {
            app.log_lines.clear();
            if filled {
                for i in 1..=30 {
                    app.log_lines.push_back(format!("line {i}"));
                }
            }
            for (w, h) in [
                (100, 30),
                (80, 24),
                (79, 24),
                (46, 18),
                (40, 12),
                (40, 11),
                (20, 6),
                (20, 4),
                (10, 3),
                (0, 0),
            ] {
                app.size = (w, h);
                let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
                terminal.draw(|frame| draw(frame, &app)).unwrap();
            }
        }
    }

    #[test]
    fn the_log_pane_names_its_position() {
        let mut app = app(scratch("pane"));
        for i in 1..=30 {
            app.log_lines
                .push_back(format!("2026-10-24T21:00:{i:02}Z INFO  line {i}"));
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let top: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect();
        assert!(top.contains("log [1/30]"), "pane title was {top:?}");

        // An empty tail says so instead of claiming a first line.
        app.log_lines.clear();
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let top: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect();
        assert!(top.contains("log empty"), "pane title was {top:?}");
    }
}
