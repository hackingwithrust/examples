//! rlog — a terminal UI that follows a log file and watches the process table.
//!
//! Step 4 of the track: the single box of step 3 becomes panes. One layout
//! splits the screen into a process table, a log list and a status line; the
//! layout is responsive — side by side on a wide terminal, stacked on a narrow
//! one — and the wheel scrolls whichever pane the pointer is over. Step 5
//! fills both panes with live data.

use std::io;
use std::time::Duration;

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

/// How long the loop waits for an event before redrawing anyway. One tick of
/// the program's clock; the refresh schedules of later steps are measured
/// against it.
const TICK: Duration = Duration::from_millis(250);

/// At or above this width the panes sit side by side; below it they stack.
/// 80 columns is the width a terminal opens at before anyone resizes it, so
/// the first thing a reader sees is the stacked layout until the window is
/// dragged wider.
const WIDE: u16 = 80;

/// How many lines one notch of the wheel scrolls. Three traverses a long log
/// without turning the wheel into a paging key.
const SCROLL_STEP: isize = 3;

/// The process table until step 5 reads the real one: pid, command, cpu
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

/// The log pane's contents until step 5 tails a real file: thirty lines of a
/// build, more than a normal pane can show at once. The one long line is on
/// purpose — a line wider than the pane is clipped, not wrapped.
const LOG_LINES: &[&str] = &[
    "2026-10-24T21:03:11Z INFO  rlog starting, following ./app.log",
    "2026-10-24T21:03:11Z INFO  tick 250ms, wheel scrolls 3 lines",
    "2026-10-24T21:04:02Z INFO  cargo build --release",
    "2026-10-24T21:04:02Z DEBUG   Compiling rlog v0.1.0",
    "2026-10-24T21:04:03Z DEBUG   Compiling ratatui v0.30.2",
    "2026-10-24T21:04:04Z DEBUG   Compiling crossterm v0.29.0",
    "2026-10-24T21:04:06Z WARN  unused import: `std::fmt`",
    "2026-10-24T21:04:06Z INFO    Finished dev profile in 4.11s",
    "2026-10-24T21:04:06Z INFO    Running target/debug/rlog",
    "2026-10-24T21:05:31Z INFO  terminal 100x30, panes side by side",
    "2026-10-24T21:05:44Z INFO  wheel over log at 60,10 -> offset 3",
    "2026-10-24T21:05:58Z INFO  resize 60x24, panes stacked",
    "2026-10-24T21:06:12Z WARN  log line wider than the pane is clipped",
    "2026-10-24T21:06:30Z ERROR failed to open ./app.log, retrying",
    "2026-10-24T21:06:31Z INFO  ./app.log opened at offset 0",
    "2026-10-24T21:07:02Z INFO  watching 3 log files, 0 errors",
    "2026-10-24T21:07:44Z DEBUG   process table refreshed, 14 rows",
    "2026-10-24T21:08:19Z INFO  cpu 12.7% cargo, 41.3% rustc",
    "2026-10-24T21:08:51Z WARN  rustc idle for 32s",
    "2026-10-24T21:09:26Z INFO  build finished, 0 warnings",
    "2026-10-24T21:10:03Z INFO  rlog idle, no new lines",
    "2026-10-24T21:10:40Z DEBUG   tick 250ms, 0 events",
    "2026-10-24T21:11:15Z INFO  terminal 100x30, panes side by side",
    "2026-10-24T21:11:52Z INFO  wheel over table at 5,5 -> offset 3",
    "2026-10-24T21:12:30Z WARN  table scrolled past the last row",
    "2026-10-24T21:13:07Z INFO  log line 30 of 30 shown",
    "2026-10-24T21:13:44Z DEBUG   frame 328, dirty false",
    "2026-10-24T21:14:21Z INFO  resize 46x18, panes stacked",
    "2026-10-24T21:14:58Z INFO  rlog stopping on q",
    "2026-10-24T21:14:58Z INFO  rlog stopped, terminal restored",
];

/// The whole state of the program. Steps 5 and 6 grow it — the log tail, the
/// real process table, the selection — and `draw` stays a pure function of
/// it.
struct App {
    frames: u64,
    /// The last key press, `None` until the first one arrives.
    last_key: Option<KeyEvent>,
    /// The last mouse event, `None` until the mouse does something.
    last_mouse: Option<MouseEvent>,
    /// Columns and rows, as of the last resize.
    size: (u16, u16),
    /// The first log line on screen. `ListState` also carries a selection,
    /// which stays `None` until step 6.
    log: ListState,
    /// The first process row on screen. `TableState` likewise has no
    /// selection yet — step 6 adds one.
    table: TableState,
}

fn main() -> io::Result<()> {
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
        frames: 0,
        last_key: None,
        last_mouse: None,
        size: crossterm::terminal::size()?,
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

/// The render loop. Every pass draws a frame, then waits one tick for an event.
/// `q`, `Esc` and `Ctrl-C` end the loop and give the terminal back; every other
/// event lands in the state, where the next frame reports it.
fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        app.frames += 1;
        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(TICK)? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        if is_quit(key) {
                            break;
                        }
                        app.last_key = Some(key);
                    }
                }
                Event::Mouse(mouse) => {
                    app.last_mouse = Some(mouse);
                    scroll_wheel(app, mouse);
                }
                Event::Resize(w, h) => app.size = (w, h),
                // Focus and paste events only arrive if the program asks for
                // them with their own commands; rlog asks for neither, so this
                // arm is unreachable in practice.
                _ => {}
            }
        }
    }
    Ok(())
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
        scroll_offset(app.log.offset_mut(), LOG_LINES.len(), visible, delta);
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

    let log = List::new(LOG_LINES.iter().map(|line| ListItem::new(*line))).block(
        Block::bordered().title(format!(
            " log [{}/{}] ",
            app.log.offset() + 1,
            LOG_LINES.len()
        )),
    );
    let mut log_state = app.log;
    frame.render_stateful_widget(log, log_area, &mut log_state);

    frame.render_widget(
        Paragraph::new(Line::from(format!(
            "frame {} · key {} · mouse {} · {}x{}",
            app.frames,
            describe_key(app.last_key.as_ref()),
            describe_mouse(app.last_mouse.as_ref()),
            app.size.0,
            app.size.1,
        ))),
        status_area,
    );
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
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn a_notch_moves_the_offset_by_the_step() {
        let mut offset = 0;
        scroll_offset(&mut offset, 30, 10, SCROLL_STEP);
        assert_eq!(offset, 3);
    }

    #[test]
    fn scrolling_up_stops_at_the_top() {
        let mut offset = 2;
        scroll_offset(&mut offset, 30, 10, -SCROLL_STEP * 2);
        assert_eq!(offset, 0);
    }

    #[test]
    fn scrolling_down_stops_with_the_last_line_on_screen() {
        let mut offset = 0;
        scroll_offset(&mut offset, 30, 10, SCROLL_STEP * 10);
        assert_eq!(offset, 20);
    }

    #[test]
    fn a_pane_taller_than_the_content_pulls_the_offset_back() {
        // A pane that just grew taller than its content has nothing to scroll:
        // the offset clamps to zero rather than hiding lines above the view.
        let mut offset = 5;
        scroll_offset(&mut offset, 6, 20, SCROLL_STEP);
        assert_eq!(offset, 0);
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
        // is allowed to draw nothing, not to panic.
        let app = App {
            frames: 1,
            last_key: None,
            last_mouse: None,
            size: (0, 0),
            log: ListState::default(),
            table: TableState::default(),
        };
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
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
        }
    }
}
