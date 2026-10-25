//! rlog — a terminal UI that follows a log file and watches the process table.
//!
//! Step 7 of the track: the frame grows a bottom pane. Every process refresh
//! now records one CPU sample per process into a bounded ring buffer, and the
//! pane draws the selected process's window as a sparkline — the pane step 1
//! sketched. `?` overlays the key list, `t` cycles three colour themes, and
//! `space` pauses the tail, which step 1 also promised and steps 5 and 6 did
//! not build. The history, the overlay and the themes are plain state: `draw`
//! borrows them and changes nothing.

use std::collections::{HashMap, HashSet, VecDeque};
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
use nucleo::{Config, Matcher, Utf32Str};
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Sparkline, Table, TableState,
};
use sysinfo::{ProcessesToUpdate, System};
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

/// How long the loop waits for an event before going around again. One tick of
/// the program's clock: short enough that a keypress feels immediate, long
/// enough that an idle program wakes four times a second instead of spinning.
const TICK: Duration = Duration::from_millis(250);

/// How often the log file is re-read. A log is cheap to poll (one `stat`, one
/// short read) and a line that appears a quarter of a second late is still
/// live.
const LOG_POLL: Duration = Duration::from_millis(250);

/// How often the process table is re-read. This is the expensive data: every
/// process on the machine is walked, and CPU usage is a delta between two
/// refreshes, so a once-a-second schedule is both the floor `sysinfo` wants
/// and the fastest rate a reader can read a changing table at.
const PROCESS_REFRESH: Duration = Duration::from_secs(1);

/// The most bytes one poll reads out of the file. A log that grew by gigabytes
/// since the last look is drained over several polls rather than pulled into
/// memory in one go; the offset keeps the place.
const LOG_READ_LIMIT: u64 = 64 * 1024;

/// How many lines the tail keeps. A pane shows a couple of dozen at a time; a
/// thousand is more history than the screen can scroll back through, and the
/// bound is what stops a busy log from growing the program without end.
const LOG_CAPACITY: usize = 1000;

/// How many CPU samples the history keeps per process. One sample per refresh
/// and one refresh per second, so the window is a minute — long enough to see
/// a process start working, short enough that the pane is never stale.
const HISTORY_CAPACITY: usize = 60;

/// How many rows the CPU history pane takes: the border pair and the single
/// row the sparkline draws in. The nine-level bar glyphs give that one row
/// eight steps of resolution, which is what a terminal sparkline is.
const HISTORY_ROWS: u16 = 3;

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

/// One row of the process table. Everything the table shows about a process,
/// copied out of `sysinfo` at refresh time: the snapshot is owned data, so
/// `draw` borrows it and the tests can build one without touching the machine.
#[derive(Debug, Clone)]
struct ProcessRow {
    /// The process id. It is also the selection's identity: the row order
    /// changes every refresh, and the pid is what stays put.
    pid: u32,
    name: String,
    /// Percent of one CPU, as `sysinfo` computes it between two refreshes.
    /// A multi-threaded process can exceed 100.
    cpu: f32,
    /// Resident set size in bytes.
    rss: u64,
}

/// Which column the table is sorted by. `s` cycles through them in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortColumn {
    Pid,
    Cpu,
    Mem,
}

impl SortColumn {
    /// The next column in the cycle.
    fn next(self) -> Self {
        match self {
            SortColumn::Pid => SortColumn::Cpu,
            SortColumn::Cpu => SortColumn::Mem,
            SortColumn::Mem => SortColumn::Pid,
        }
    }

    /// The column's name, as the pane title spells it.
    fn label(self) -> &'static str {
        match self {
            SortColumn::Pid => "pid",
            SortColumn::Cpu => "cpu",
            SortColumn::Mem => "mem",
        }
    }
}

/// A palette. Everything the frame paints with a colour comes from here, and
/// `t` walks the three in order. The accent colour is the only difference
/// between them: the layout, the data and the keys do not move.
#[derive(Debug, Clone, Copy)]
struct Theme {
    /// The palette's name, as the status line spells it.
    name: &'static str,
    /// The header line.
    header: Style,
    /// Pane borders.
    border: Style,
    /// Pane titles.
    title: Style,
    /// The selected table row.
    highlight: Style,
    /// The sparkline bars.
    spark: Style,
    /// The filter line's hint while the editor is closed.
    dim: Style,
}

impl Theme {
    /// The next palette in the cycle.
    fn next(self) -> Self {
        let index = THEMES
            .iter()
            .position(|theme| theme.name == self.name)
            .unwrap_or(0);
        THEMES[(index + 1) % THEMES.len()]
    }
}

/// The three palettes, in the order `t` cycles through them. `Style` is a
/// plain value — a colour and a set of attributes — so the whole table is a
/// constant, built at compile time.
const THEMES: [Theme; 3] = [
    Theme {
        name: "slate",
        header: Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        border: Style::new().fg(Color::DarkGray),
        title: Style::new().fg(Color::Cyan),
        highlight: Style::new().fg(Color::Black).bg(Color::Cyan),
        spark: Style::new().fg(Color::Cyan),
        dim: Style::new().add_modifier(Modifier::DIM),
    },
    Theme {
        name: "ember",
        header: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        border: Style::new().fg(Color::DarkGray),
        title: Style::new().fg(Color::Yellow),
        highlight: Style::new().fg(Color::Black).bg(Color::Yellow),
        spark: Style::new().fg(Color::Yellow),
        dim: Style::new().add_modifier(Modifier::DIM),
    },
    Theme {
        name: "mono",
        header: Style::new().add_modifier(Modifier::BOLD),
        border: Style::new().add_modifier(Modifier::DIM),
        title: Style::new().add_modifier(Modifier::BOLD),
        highlight: Style::new().add_modifier(Modifier::REVERSED),
        spark: Style::new(),
        dim: Style::new().add_modifier(Modifier::DIM),
    },
];

/// The whole state of the program. `draw` stays a pure function of it: the
/// render borrows the state back and changes nothing.
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
    /// The tail is frozen: the file keeps growing and nothing is read from it
    /// until `space` is pressed again. The offset stays where it is, so the
    /// catch-up read picks up everything that accumulated.
    paused: bool,
    /// Which log lines the pane shows: indices into `log_lines`, in order.
    /// Every line when no filter is active, the matching ones when it is.
    log_view: Vec<usize>,
    /// The first log line on screen.
    log: ListState,
    /// The `sysinfo` handle. It owns no snapshot of its own — `processes`
    /// below is the snapshot — but it owns the state CPU usage is computed
    /// from, which is why it lives for the whole run.
    system: System,
    /// The last process snapshot, kept in the current sort order.
    processes: Vec<ProcessRow>,
    /// Which processes the pane shows: indices into `processes`, in order.
    /// All of them when no filter is active, the matching ones when it is.
    view: Vec<usize>,
    /// The selected process, by pid. The first refresh already selects the
    /// top row, so the highlight is on screen before the first keypress.
    selected: Option<u32>,
    /// The first process row on screen.
    table: TableState,
    /// Which column sorts the table, and which way.
    sort: SortColumn,
    sort_desc: bool,
    /// The filter editor, `Some` while `/` is open. Closing it keeps the
    /// value: the filter stays active until it is cleared.
    filter: Option<Input>,
    /// The editor is open: keys belong to it. This is not the same as the
    /// filter being active — `Enter` closes the editor and leaves the filter
    /// in place, which is what step 6 promised and what its code did not do:
    /// closing the editor used to drop the value with it, and the table
    /// unfiltered itself at the next refresh.
    editing: bool,
    /// The fuzzy matcher, and the two scratch buffers `Utf32Str` borrows.
    /// They live in the state because a matcher is expensive to build and the
    /// filter runs on every row of every refresh.
    matcher: Matcher,
    needle_buf: Vec<char>,
    haystack_buf: Vec<char>,
    /// One CPU sample per process per refresh, oldest first, bounded by
    /// `HISTORY_CAPACITY`. Keyed by pid, because the row order changes every
    /// refresh and the history has to follow the process, not the row.
    history: HashMap<u32, VecDeque<f32>>,
    /// The help overlay is up. Every key press closes it.
    help: bool,
    /// The palette the frame is drawn in.
    theme: Theme,
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
        paused: false,
        log_view: Vec::new(),
        log: ListState::default(),
        system: System::new(),
        processes: Vec::new(),
        view: Vec::new(),
        selected: None,
        table: TableState::default(),
        sort: SortColumn::Cpu,
        sort_desc: true,
        filter: None,
        editing: false,
        matcher: Matcher::new(Config::DEFAULT),
        needle_buf: Vec::new(),
        haystack_buf: Vec::new(),
        history: HashMap::new(),
        help: false,
        theme: THEMES[0],
    };

    // The baseline refresh. `sysinfo` computes CPU usage as a delta between
    // two refreshes, so the first frame shows 0.0% everywhere and the real
    // numbers arrive with the first scheduled refresh a second later.
    refresh_processes(&mut app);

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

/// The render loop. Each pass, in order: poll the log file if its interval has
/// come around and the tail is not frozen, refresh the process table if its
/// slower one has, draw if anything is dirty, then wait one tick for an event.
/// `q`, `Esc` and `Ctrl-C` end the loop and give the terminal back; every
/// other event lands in the state, where the next frame reports it.
fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    // Both due immediately: the first frame already shows the file and the
    // process table as they stand.
    let mut log_due = Instant::now();
    let mut process_due = Instant::now();

    loop {
        // A frozen tail keeps its deadline: when `space` is pressed again the
        // poll is already overdue, so the catch-up read happens on the next
        // pass rather than a quarter of a second later.
        if !app.paused && Instant::now() >= log_due {
            poll_log(app)?;
            log_due = Instant::now() + LOG_POLL;
        }
        if Instant::now() >= process_due {
            refresh_processes(app);
            process_due = Instant::now() + PROCESS_REFRESH;
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
/// last key and mouse event, a resize reflows every pane, a keypress moves the
/// selection or the sort, and a wheel notch moves an offset. An event that
/// changes nothing visible — a key release, say — leaves the flag alone.
fn apply(app: &mut App, event: Event) -> bool {
    // The help overlay swallows every key press: any key closes it, and the
    // key does nothing else, so a reader cannot move the selection by
    // accident through the overlay. `Ctrl-C` is the exception — it always
    // ends the program.
    if app.help {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return false;
                }
                app.help = false;
                app.dirty = true;
            }
        }
        return true;
    }

    // While the filter editor is open, keys belong to it: `q` is a letter, not
    // a quit. `Ctrl-C` still ends the program — it arrives as a key event in
    // raw mode, and a user who presses it means it.
    if app.editing {
        if let Event::Key(key) = &event {
            if key.kind == KeyEventKind::Press {
                if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                    return false;
                }
                match key.code {
                    // Enter closes the editor and keeps the filter active.
                    KeyCode::Enter => {
                        close_filter(app, false);
                        return true;
                    }
                    // Esc closes the editor and clears the filter.
                    KeyCode::Esc => {
                        close_filter(app, true);
                        return true;
                    }
                    _ => {}
                }
            }
        }
        if let Some(input) = app.filter.as_mut() {
            let changed = input.handle_event(&event).map(|s| s.value).unwrap_or(false);
            if changed {
                rebuild_view(app);
                rebuild_log_view(app);
                app.dirty = true;
            }
        }
        return true;
    }

    match event {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Press {
                if is_quit(key) {
                    return false;
                }
                match key.code {
                    KeyCode::Char('j') | KeyCode::Down => {
                        move_selection(app, 1);
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        move_selection(app, -1);
                    }
                    KeyCode::Char('s') => {
                        app.sort = app.sort.next();
                        sort_processes(&mut app.processes, app.sort, app.sort_desc);
                        rebuild_view(app);
                        app.dirty = true;
                    }
                    KeyCode::Char('r') => {
                        app.sort_desc = !app.sort_desc;
                        sort_processes(&mut app.processes, app.sort, app.sort_desc);
                        rebuild_view(app);
                        app.dirty = true;
                    }
                    KeyCode::Char('/') => {
                        open_filter(app);
                    }
                    // Freeze the tail. The file keeps growing; nothing is read
                    // until the key is pressed again, so a burst can be read
                    // at the reader's pace instead of scrolling past.
                    KeyCode::Char(' ') => {
                        app.paused = !app.paused;
                        app.dirty = true;
                    }
                    KeyCode::PageUp => {
                        scroll_log(app, -page_step(app));
                    }
                    KeyCode::PageDown => {
                        scroll_log(app, page_step(app));
                    }
                    KeyCode::Char('?') => {
                        app.help = true;
                        app.dirty = true;
                    }
                    KeyCode::Char('t') => {
                        app.theme = app.theme.next();
                        app.dirty = true;
                    }
                    _ => {
                        app.last_key = Some(key);
                        app.dirty = true;
                    }
                }
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

/// Read the whole process table into the snapshot, sort it, rebuild the
/// view, and record one CPU sample per process. This is the expensive read,
/// and it runs on the slow schedule.
fn refresh_processes(app: &mut App) {
    app.system.refresh_processes(ProcessesToUpdate::All, true);
    app.system.refresh_memory();
    app.processes = app
        .system
        .processes()
        .values()
        .map(|process| ProcessRow {
            pid: process.pid().as_u32(),
            name: process.name().to_string_lossy().into_owned(),
            cpu: process.cpu_usage(),
            rss: process.memory(),
        })
        .collect();
    sort_processes(&mut app.processes, app.sort, app.sort_desc);
    rebuild_view(app);
    record_history(app);

    // The selection is a pid, and a refresh is exactly when a pid can stop
    // existing. A process that exited leaves the selection pointing at
    // nothing, and the top row takes over. A process that is still alive but
    // hidden by the filter keeps its selection: it comes back when the filter
    // is cleared.
    let alive = app
        .selected
        .is_some_and(|pid| app.processes.iter().any(|row| row.pid == pid));
    if !alive {
        app.selected = app.view.first().map(|&i| app.processes[i].pid);
    }
    app.dirty = true;
}

/// Record one CPU sample per process, and forget the processes that are gone.
///
/// The window is bounded twice over. Each deque drops its oldest sample at
/// `HISTORY_CAPACITY`, and the map drops the pids the latest snapshot did not
/// see — a machine that churns short-lived processes would otherwise grow the
/// history without end, one dead pid at a time.
fn record_history(app: &mut App) {
    let alive: HashSet<u32> = app.processes.iter().map(|row| row.pid).collect();
    for row in &app.processes {
        let samples = app.history.entry(row.pid).or_default();
        samples.push_back(row.cpu);
        if samples.len() > HISTORY_CAPACITY {
            samples.pop_front();
        }
    }
    app.history.retain(|&pid, _| alive.contains(&pid));
}

/// Sort the snapshot in place. The primary key is the chosen column; the pid
/// breaks every tie, so two processes with the same CPU keep a stable,
/// reproducible order instead of one that depends on hash-map iteration.
fn sort_processes(rows: &mut [ProcessRow], column: SortColumn, descending: bool) {
    rows.sort_by(|a, b| {
        let primary = match column {
            SortColumn::Pid => a.pid.cmp(&b.pid),
            SortColumn::Cpu => a.cpu.total_cmp(&b.cpu),
            SortColumn::Mem => a.rss.cmp(&b.rss),
        };
        let primary = if descending { primary.reverse() } else { primary };
        primary.then(a.pid.cmp(&b.pid))
    });
}

/// Rebuild the process view: the indices of the rows that pass the filter, in
/// the order the table shows them. An empty filter keeps everything.
fn rebuild_view(app: &mut App) {
    let needle = filter_needle(app);
    let App {
        matcher,
        needle_buf,
        haystack_buf,
        processes,
        view,
        ..
    } = app;
    *view = (0..processes.len())
        .filter(|&i| {
            fuzzy_match(
                matcher,
                needle_buf,
                haystack_buf,
                &needle,
                &row_haystack(&processes[i]),
            )
        })
        .collect();
    clamp_table(app);
}

/// What a process row is matched against: the pid and the name, joined by a
/// space, so a needle can be either — `ngx` finds nginx, `1042` finds a pid.
fn row_haystack(row: &ProcessRow) -> String {
    format!("{} {}", row.pid, row.name)
}

/// The active needle, or the empty string when no filter is set. An empty
/// needle matches everything, which is what "no filter" means.
fn filter_needle(app: &App) -> String {
    app.filter
        .as_ref()
        .map(|input| input.value().to_string())
        .unwrap_or_default()
}

/// One fuzzy match. `needle` is the query, `haystack` is the row or line.
///
/// `nucleo`'s matcher works on UTF-32 strings it borrows, so the two buffers
/// are scratch space owned by the caller and reused across calls — building a
/// matcher allocates a large matrix, and running a filter over a thousand rows
/// should not mean building anything per row.
fn fuzzy_match(
    matcher: &mut Matcher,
    needle_buf: &mut Vec<char>,
    haystack_buf: &mut Vec<char>,
    needle: &str,
    haystack: &str,
) -> bool {
    if needle.is_empty() {
        return true;
    }
    let needle = Utf32Str::new(needle, needle_buf);
    let haystack = Utf32Str::new(haystack, haystack_buf);
    matcher.fuzzy_match(haystack, needle).is_some()
}

/// Open the filter editor. The value starts empty, so opening it shows
/// everything until the first character arrives.
fn open_filter(app: &mut App) {
    app.filter = Some(Input::default());
    app.editing = true;
    app.dirty = true;
}

/// Close the filter editor. `clear` also empties the value, which is what
/// `Esc` does; `Enter` keeps the filter active so a narrowed table stays
/// narrowed while the keyboard goes back to moving the selection — and stays
/// narrowed across refreshes, because the value outlives the editor.
fn close_filter(app: &mut App, clear: bool) {
    app.editing = false;
    if clear {
        app.filter = None;
        rebuild_view(app);
        rebuild_log_view(app);
    }
    app.dirty = true;
}

/// Move the selection by `delta` rows in the current view, clamped at both
/// ends. The selection is a pid, not an index: the table reorders itself under
/// the cursor every second, and a pid is what keeps the same process
/// highlighted while its CPU percentage moves.
fn move_selection(app: &mut App, delta: isize) {
    if app.view.is_empty() {
        app.selected = None;
        app.dirty = true;
        return;
    }
    // No selection yet: `j` starts at the top, `k` at the bottom.
    let start = selection_index(app)
        .map(|index| index as isize)
        .unwrap_or(if delta < 0 {
            app.view.len() as isize
        } else {
            -1
        });
    let target = (start + delta).clamp(0, app.view.len() as isize - 1) as usize;
    app.selected = Some(app.processes[app.view[target]].pid);
    keep_selection_visible(app);
    app.dirty = true;
}

/// Where the selected process sits in the current view, if it is in it.
fn selection_index(app: &App) -> Option<usize> {
    let pid = app.selected?;
    app.view.iter().position(|&i| app.processes[i].pid == pid)
}

/// Pull the scroll offset to the selection after it moved. A selection below
/// the window scrolls down by the fewest rows that bring it into view; one
/// above scrolls back up to it.
fn keep_selection_visible(app: &mut App) {
    let Some(index) = selection_index(app) else {
        return;
    };
    let visible = table_visible(app.size);
    if visible == 0 {
        return;
    }
    let offset = app.table.offset_mut();
    if index < *offset {
        *offset = index;
    } else if index >= *offset + visible {
        *offset = index + 1 - visible;
    }
}

/// Pull the table's scroll offset back into range after the view changed.
/// A filter that hides the rows below a scrolled pane would otherwise leave
/// the offset pointing past the end.
fn clamp_table(app: &mut App) {
    let visible = table_visible(app.size);
    let last = app.view.len().saturating_sub(visible);
    let offset = app.table.offset_mut();
    if *offset > last {
        *offset = last;
        app.dirty = true;
    }
    keep_selection_visible(app);
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
        // (or replaced by a shorter one). Start over from the beginning.
        app.log_offset = 0;
        app.log_lines.clear();
        app.log_truncations += 1;
        rebuild_log_view(app);
        app.dirty = true;
    }
    if len == app.log_offset {
        return Ok(());
    }

    file.seek(SeekFrom::Start(app.log_offset))?;
    let mut chunk = Vec::new();
    // `take` caps the read at `LOG_READ_LIMIT` bytes; a file that grew by more
    // than that since the last poll is finished on the next one. The cap is
    // what makes a frozen tail's catch-up read bounded too: a burst of a
    // gigabyte arrives over several polls, not in one frame.
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
    rebuild_log_view(app);
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

/// Rebuild the log view: the indices of the lines the pane shows, and pull the
/// scroll offset back into range. The filter runs over the whole line, because
/// a log line is one string and there is no field boundary to match against.
fn rebuild_log_view(app: &mut App) {
    let needle = filter_needle(app);
    let App {
        matcher,
        needle_buf,
        haystack_buf,
        log_lines,
        log_view,
        ..
    } = app;
    *log_view = (0..log_lines.len())
        .filter(|&i| fuzzy_match(matcher, needle_buf, haystack_buf, &needle, &log_lines[i]))
        .collect();
    let visible = log_visible(app.size);
    let last = log_view.len().saturating_sub(visible);
    let offset = app.log.offset_mut();
    if *offset > last {
        *offset = last;
        app.dirty = true;
    }
}

/// The series the sparkline draws: the newest `width` samples, oldest first,
/// padded at the front with absent values so the current sample always sits
/// at the right edge.
///
/// Two conversions happen here. Percent is scaled by a hundred — one decimal of
/// precision, kept as an integer because the widget counts in `u64` — and the
/// samples the pane has not got yet are `None`, which draws as a blank column.
/// A process that started ten seconds ago shows ten bars against the right
/// edge, not ten bars drifting in from the left.
fn sparkline_data(samples: &VecDeque<f32>, width: usize) -> Vec<Option<u64>> {
    let mut series = vec![None; width];
    let take = samples.len().min(width);
    let skip = samples.len() - take;
    for (slot, sample) in series[width - take..].iter_mut().zip(samples.iter().skip(skip)) {
        *slot = Some((*sample * 100.0).round() as u64);
    }
    series
}

/// Split the screen into the frame: a one-row header, the two panes, the CPU
/// history pane, the filter line and the status line. Wide terminals put the
/// panes side by side; narrow ones stack them, the table above the log.
///
/// The header, the history pane, the filter line and the status line are
/// `Length`: they take their rows first, and the panes share what is left, so
/// a squeezed terminal can never push them off the screen. On a terminal too
/// short for all of them the panes are what shrink — six rows is the floor
/// where the header, the history pane, the filter line and the status line
/// still fit.
fn layout(area: Rect) -> [Rect; 6] {
    let [header, main, history, filter, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(HISTORY_ROWS),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    if area.width >= WIDE {
        let [table, log] =
            Layout::horizontal([Constraint::Percentage(38), Constraint::Fill(1)]).areas(main);
        [header, table, log, history, filter, status]
    } else {
        // The table is `Length(11)` — header, eight processes and the border —
        // and the log takes the rest. A `Min(11)` floor would not do here: the
        // solver shares the slack beyond the minimum with the `Fill` pane, so
        // the table would keep growing as the terminal grows taller.
        let [table, log] =
            Layout::vertical([Constraint::Length(11), Constraint::Fill(1)]).areas(main);
        [header, table, log, history, filter, status]
    }
}

/// How many log lines the pane can show at the current size: the border takes
/// a row off the top and one off the bottom.
fn log_visible(size: (u16, u16)) -> usize {
    let [_, _, log_area, _, _, _] = layout(Rect::new(0, 0, size.0, size.1));
    log_area.height.saturating_sub(2) as usize
}

/// How many process rows the pane can show at the current size: the border
/// takes a row off the top and one off the bottom, and the header row takes
/// one more.
fn table_visible(size: (u16, u16)) -> usize {
    let [_, table_area, _, _, _, _] = layout(Rect::new(0, 0, size.0, size.1));
    table_area.height.saturating_sub(3) as usize
}

/// How many lines one `PgUp`/`PgDn` scrolls: a screenful, less the row that
/// stays put so the reader keeps their place.
fn page_step(app: &App) -> isize {
    log_visible(app.size).saturating_sub(1) as isize
}

/// Scroll the log pane by `delta` lines, clamped at both ends.
fn scroll_log(app: &mut App, delta: isize) {
    let visible = log_visible(app.size);
    scroll_offset(app.log.offset_mut(), app.log_view.len(), visible, delta);
    app.dirty = true;
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
    let [_, table_area, log_area, _, _, _] = layout(Rect::new(0, 0, app.size.0, app.size.1));
    let at = Position::new(mouse.column, mouse.row);
    if table_area.contains(at) {
        // The table spends its first inner row on the header, so the rows
        // that fit are three fewer than the pane is tall.
        let visible = table_area.height.saturating_sub(3) as usize;
        scroll_offset(app.table.offset_mut(), app.view.len(), visible, delta);
    } else if log_area.contains(at) {
        let visible = log_area.height.saturating_sub(2) as usize;
        scroll_offset(app.log.offset_mut(), app.log_view.len(), visible, delta);
    }
    // A notch over the history pane, the header, the filter line or the
    // status line scrolls nothing: those have no offset to move.
}

/// Move a scroll offset by `delta` lines, clamped at both ends. The upper
/// clamp keeps the last line on screen: the offset never passes
/// `len - visible`, and when the pane is taller than the content that is zero
/// — a pane that just grew taller pulls a stale offset back with it.
fn scroll_offset(offset: &mut usize, len: usize, visible: usize, delta: isize) {
    let last = len.saturating_sub(visible) as isize;
    *offset = (*offset as isize + delta).clamp(0, last) as usize;
}

/// The whole frame: header, two panes, the CPU history pane, the filter line
/// and the status line, laid out from the terminal's current size. Both states
/// are `Copy`, so `draw` takes the app by shared reference and renders from
/// copies — the offset and selection the widgets write back while rendering
/// land on the copies, and the state changes only when an event says so.
fn draw(frame: &mut Frame, app: &App) {
    let theme = app.theme;
    let [header_area, table_area, log_area, history_area, filter_area, status_area] =
        layout(frame.area());

    // The header answers "is this box busy?" before anything is selected:
    // how many processes there are, and how much of the memory they hold.
    let gib = |bytes: u64| bytes as f64 / (1024 * 1024 * 1024) as f64;
    let used = gib(app.system.used_memory());
    let total = gib(app.system.total_memory());
    frame.render_widget(
        Paragraph::new(Line::from(format!(
            " rlog · {} processes · {used:.1}/{total:.1} GB resident",
            app.processes.len()
        )))
        .style(theme.header),
        header_area,
    );

    let rows = app.view.iter().map(|&i| {
        let process = &app.processes[i];
        Row::new(vec![
            Cell::from(process.pid.to_string()),
            Cell::from(process.name.as_str()),
            Cell::from(Line::from(format!("{:.1}", process.cpu)).alignment(Alignment::Right)),
            Cell::from(
                Line::from(format!("{:.1}", percent(process.rss, app.system.total_memory())))
                    .alignment(Alignment::Right),
            ),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Min(8),
            Constraint::Length(5),
            Constraint::Length(5),
        ],
    )
    .header(
        Row::new(vec![
            Cell::from("PID"),
            Cell::from("COMMAND"),
            Cell::from(Line::from("CPU%").alignment(Alignment::Right)),
            Cell::from(Line::from("MEM%").alignment(Alignment::Right)),
        ])
        .style(theme.title),
    )
    .block(
        Block::bordered()
            .title(format!(" processes {} ", table_title(app)))
            .border_style(theme.border)
            .title_style(theme.title),
    )
    // The selection is drawn by the widget: a marker in front of the row and
    // the row reversed, so the eye finds it without reading the numbers.
    .highlight_symbol("▸")
    .row_highlight_style(theme.highlight);
    let mut table_state = app.table;
    table_state.select(selection_index(app));
    frame.render_stateful_widget(table, table_area, &mut table_state);

    let log = List::new(
        app.log_view
            .iter()
            .map(|&i| ListItem::new(app.log_lines[i].as_str())),
    )
    .block(
        Block::bordered()
            .title(format!(" log {} ", log_title(app)))
            .border_style(theme.border)
            .title_style(theme.title),
    );
    let mut log_state = app.log;
    frame.render_stateful_widget(log, log_area, &mut log_state);

    draw_history(frame, history_area, app, theme);

    draw_filter(frame, filter_area, app);

    frame.render_widget(
        Paragraph::new(Line::from(format!(
            "draw {} · {} · key {} · mouse {} · {}x{} · theme {}",
            app.draws,
            describe_log(app),
            describe_key(app.last_key.as_ref()),
            describe_mouse(app.last_mouse.as_ref()),
            app.size.0,
            app.size.1,
            theme.name,
        ))),
        status_area,
    );

    // The overlay goes on last: it covers whatever is underneath it, which is
    // the point of an overlay.
    if app.help {
        let overlay = help_overlay(frame.area());
        frame.render_widget(Clear, overlay);
        frame.render_widget(
            Paragraph::new(help_lines()).block(
                Block::bordered()
                    .title(" keys ")
                    .border_style(theme.border)
                    .title_style(theme.title),
            ),
            overlay,
        );
    }
}

/// The bottom pane: the selected process's CPU history, drawn as a sparkline.
/// The subject is a pid, so the pane follows the selection even while the
/// table reorders underneath it.
///
/// The title carries what the bars cannot: the name and pid of the subject,
/// its latest sample, and how much of the window is filled. That last part
/// matters because an all-zero history draws a blank line — zero is the bottom
/// of the scale, and a flat zero is indistinguishable from no data at all
/// without the count.
fn draw_history(frame: &mut Frame, area: Rect, app: &App, theme: Theme) {
    let subject = app.selected.and_then(|pid| {
        let row = app.processes.iter().find(|row| row.pid == pid)?;
        let samples = app.history.get(&pid)?;
        Some((row, samples))
    });
    let title = match subject {
        Some((row, samples)) => format!(
            " cpu history · {} (pid {}) · {:.1}% now · {}/{} samples ",
            row.name,
            row.pid,
            samples.back().copied().unwrap_or_default(),
            samples.len(),
            HISTORY_CAPACITY
        ),
        None => " cpu history · no process selected ".to_string(),
    };
    // The border takes a column from each side.
    let width = area.width.saturating_sub(2) as usize;
    let data = subject
        .map(|(_, samples)| sparkline_data(samples, width))
        .unwrap_or_default();
    let sparkline = Sparkline::default()
        .block(
            Block::bordered()
                .title(title)
                .border_style(theme.border)
                .title_style(theme.title),
        )
        .data(data)
        .style(theme.spark);
    frame.render_widget(sparkline, area);
}

/// Where the help overlay sits: a centred box, two thirds of the width and
/// half the height. On a squeezed terminal the box shrinks with it, and an
/// empty area draws nothing rather than complaining.
fn help_overlay(area: Rect) -> Rect {
    Rect {
        x: area.x + area.width / 6,
        y: area.y + area.height / 4,
        width: area.width * 2 / 3,
        height: area.height / 2,
    }
}

/// The overlay's contents: every key, and what it does. The list is the
/// overlay — there is no second source of truth for the keys.
fn help_lines() -> Vec<Line<'static>> {
    let rows = [
        ("j, k, arrows", "move the selection"),
        ("s", "cycle the sort column"),
        ("r", "flip the sort direction"),
        ("/", "filter (enter keeps, esc clears)"),
        ("space", "pause the tail"),
        ("pgup, pgdn", "scroll the log"),
        ("t", "next colour theme"),
        ("?", "this help"),
        ("q, ctrl-c", "quit"),
    ];
    rows.iter()
        .map(|(key, what)| Line::from(format!("  {key:<14} {what}")))
        .collect()
}

/// The filter line. Closed, it is a dim hint that says what `/` does; open, it
/// shows the value with the cursor after it, and the terminal cursor is placed
/// there so typing looks like typing.
fn draw_filter(frame: &mut Frame, area: Rect, app: &App) {
    let (text, style) = if app.editing {
        (
            format!("/ {}", app.filter.as_ref().map(|input| input.value()).unwrap_or_default()),
            Style::new().add_modifier(Modifier::BOLD),
        )
    } else if app.filter.is_some() {
        // The editor is closed but the filter is still active.
        (
            format!("filter: {}", app.filter.as_ref().unwrap().value()),
            app.theme.dim,
        )
    } else {
        ("press / to filter".to_string(), app.theme.dim)
    };
    frame.render_widget(Paragraph::new(Line::from(text)).style(style), area);

    if app.editing {
        if let Some(input) = &app.filter {
            // The row holds "/ " plus the value. `visual_scroll` says how much of
            // the value is scrolled off to the left at this width; the cursor is
            // drawn where it falls inside the visible part.
            let width = area.width.saturating_sub(3) as usize;
            let scroll = input.visual_scroll(width);
            let x = input.visual_cursor().max(scroll) - scroll + 2;
            frame.set_cursor_position((area.x + x as u16, area.y));
        }
    }
}

/// The process pane's title: where the top row stands, how many rows the view
/// holds, and which column sorts and which way.
fn table_title(app: &App) -> String {
    if app.view.is_empty() {
        return "empty".to_string();
    }
    format!(
        "[{}/{}] · {} {}",
        app.table.offset() + 1,
        app.view.len(),
        app.sort.label(),
        if app.sort_desc { "desc" } else { "asc" }
    )
}

/// The log pane's title: where the tail stands, how many times it has been
/// restarted, and whether it is frozen. An empty view says so rather than
/// claiming a first line — which includes the case of a filter that matches
/// nothing.
fn log_title(app: &App) -> String {
    if app.log_view.is_empty() {
        return "empty".to_string();
    }
    let mut title = format!("[{}/{}]", app.log.offset() + 1, app.log_view.len());
    if app.log_truncations > 0 {
        title.push_str(&format!(" trunc {}", app.log_truncations));
    }
    if app.paused {
        title.push_str(" paused");
    }
    title
}

/// Resident bytes as a percentage of the machine's total memory.
fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        return 0.0;
    }
    part as f64 / whole as f64 * 100.0
}

/// Name the log pane for the status line: where the tail stands, or that the
/// file is not there.
fn describe_log(app: &App) -> String {
    if app.log_missing {
        "log missing".to_string()
    } else {
        format!("log {}@{}B", app.log_view.len(), app.log_offset)
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
        // The space bar has no printable character, and a blank in the middle
        // of the status line reads as a glitch.
        KeyCode::Char(' ') => "space".to_string(),
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
    use tui_input::InputRequest;

    /// An app pointed at a scratch file, the state `main` builds at startup.
    fn app(log_path: PathBuf) -> App {
        let mut app = App {
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
            paused: false,
            log_view: Vec::new(),
            log: ListState::default(),
            system: System::new(),
            processes: Vec::new(),
            view: Vec::new(),
            selected: None,
            table: TableState::default(),
        sort: SortColumn::Cpu,
        sort_desc: true,
        filter: None,
        editing: false,
        matcher: Matcher::new(Config::DEFAULT),
        needle_buf: Vec::new(),
        haystack_buf: Vec::new(),
        history: HashMap::new(),
        help: false,
        theme: THEMES[0],
    };
    refresh_processes(&mut app);
        // The baseline refresh above reads the real machine; the tests that
        // install their own snapshot start from the startup state instead,
        // so nothing the test machine happens to be running leaks in.
        app.processes.clear();
        app.view.clear();
        app.selected = None;
        app.table = TableState::default();
        app.history.clear();
        app.dirty = false;
        app
    }

    /// A scratch file with its own name, so parallel tests do not share one.
    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("rlog-test-{name}.log"))
    }

    /// A fixed snapshot, so the sort and selection tests do not read the
    /// machine the test happens to run on.
    fn rows() -> Vec<ProcessRow> {
        vec![
            ProcessRow { pid: 1042, name: "rlog".into(), cpu: 8.1, rss: 40_000_000 },
            ProcessRow { pid: 988, name: "nvim".into(), cpu: 6.4, rss: 20_000_000 },
            ProcessRow { pid: 812, name: "node".into(), cpu: 3.0, rss: 60_000_000 },
            ProcessRow { pid: 381, name: "nginx".into(), cpu: 0.0, rss: 5_000_000 },
        ]
    }

    /// The same rows in a different order, as a refresh would leave them:
    /// node got busy, the rest kept their numbers.
    fn refreshed() -> Vec<ProcessRow> {
        vec![
            ProcessRow { pid: 812, name: "node".into(), cpu: 61.0, rss: 60_000_000 },
            ProcessRow { pid: 1042, name: "rlog".into(), cpu: 8.1, rss: 40_000_000 },
            ProcessRow { pid: 988, name: "nvim".into(), cpu: 6.4, rss: 20_000_000 },
            ProcessRow { pid: 381, name: "nginx".into(), cpu: 0.0, rss: 5_000_000 },
        ]
    }

    #[test]
    fn sorting_puts_the_busiest_process_first() {
        let mut table = rows();
        // The default: CPU, descending.
        sort_processes(&mut table, SortColumn::Cpu, true);
        let names: Vec<&str> = table.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["rlog", "nvim", "node", "nginx"]);

        // `r` flips it without touching the column.
        sort_processes(&mut table, SortColumn::Cpu, false);
        let names: Vec<&str> = table.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(names, ["nginx", "node", "nvim", "rlog"]);

        // `s` moves to the next column, and the direction survives the move.
        sort_processes(&mut table, SortColumn::Mem, true);
        let pids: Vec<u32> = table.iter().map(|row| row.pid).collect();
        assert_eq!(pids, [812, 1042, 988, 381]);

        // pid sorts ascending, and the pid breaks every tie, so the order
        // never depends on anything but the data.
        sort_processes(&mut table, SortColumn::Pid, false);
        let pids: Vec<u32> = table.iter().map(|row| row.pid).collect();
        assert_eq!(pids, [381, 812, 988, 1042]);
    }

    #[test]
    fn equal_keys_keep_the_pid_order() {
        let mut table = vec![
            ProcessRow { pid: 40, name: "b".into(), cpu: 1.0, rss: 1 },
            ProcessRow { pid: 10, name: "a".into(), cpu: 1.0, rss: 1 },
            ProcessRow { pid: 20, name: "c".into(), cpu: 1.0, rss: 1 },
        ];
        sort_processes(&mut table, SortColumn::Cpu, true);
        let pids: Vec<u32> = table.iter().map(|row| row.pid).collect();
        assert_eq!(pids, [10, 20, 40]);
    }

    #[test]
    fn the_selection_follows_the_pid_not_the_row() {
        let mut app = app(scratch("selection"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);

        // nvim is the second row: two presses from no selection reach it.
        move_selection(&mut app, 1);
        move_selection(&mut app, 1);
        assert_eq!(app.selected, Some(988));
        assert_eq!(selection_index(&app), Some(1));

        app.processes = refreshed();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        assert_eq!(app.selected, Some(988));
        // nvim dropped from second to third, and the index followed the pid.
        assert_eq!(selection_index(&app), Some(2));
    }

    #[test]
    fn a_process_that_exits_hands_the_selection_to_the_top_row() {
        let mut app = app(scratch("exited"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        move_selection(&mut app, 1);
        move_selection(&mut app, 1);
        move_selection(&mut app, 1);
        assert_eq!(app.selected, Some(812));

        // node (812) is gone from the next snapshot; the top row takes over.
        app.processes = rows().into_iter().filter(|row| row.pid != 812).collect();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        let alive = app
            .selected
            .is_some_and(|pid| app.processes.iter().any(|row| row.pid == pid));
        assert!(!alive);
        app.selected = app.view.first().map(|&i| app.processes[i].pid);
        assert_eq!(app.selected, Some(1042));
    }

    #[test]
    fn moving_the_selection_clamps_at_both_ends() {
        let mut app = app(scratch("clamp-move"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);

        // No selection yet: the first `j` lands on the first row, and `k`
        // from there has nowhere to go.
        move_selection(&mut app, 1);
        assert_eq!(app.selected, Some(1042));
        move_selection(&mut app, -1);
        assert_eq!(app.selected, Some(1042));

        // `j` walks down: rlog (8.1), nvim (6.4), node (3.0), nginx (0.0),
        // and one more press has nowhere to go either.
        move_selection(&mut app, 3);
        assert_eq!(app.selected, Some(381));
        move_selection(&mut app, 1);
        assert_eq!(app.selected, Some(381));

        // An empty view leaves nothing selected.
        app.view.clear();
        move_selection(&mut app, 1);
        assert_eq!(app.selected, None);
    }

    #[test]
    fn the_selection_stays_visible_when_the_table_scrolls() {
        let mut app = app(scratch("visible"));
        // Thirty rows, of which the pane shows 21 at 100x30: a 24-row pane
        // minus the border pair and the header.
        for i in 1..=30u32 {
            app.processes.push(ProcessRow {
                pid: 1000 + i,
                name: format!("worker{i}"),
                cpu: i as f32,
                rss: 1,
            });
        }
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        assert_eq!(table_visible(app.size), 21);

        // Select the last row — worker1, pid 1001, because CPU sorts
        // descending: the offset jumps so it is on screen.
        move_selection(&mut app, 100);
        assert_eq!(app.selected, Some(1001));
        assert_eq!(app.table.offset(), 30 - table_visible(app.size));
        assert!(selection_index(&app).unwrap() >= app.table.offset());

        // Selecting the first row scrolls back to the top.
        move_selection(&mut app, -100);
        assert_eq!(app.selected, Some(1030));
        assert_eq!(app.table.offset(), 0);
    }

    #[test]
    fn the_filter_matches_names_and_pids_fuzzily() {
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut needle_buf = Vec::new();
        let mut haystack_buf = Vec::new();

        // The step 1 example: `ngx` finds nginx.
        assert!(fuzzy_match(
            &mut matcher,
            &mut needle_buf,
            &mut haystack_buf,
            "ngx",
            "381 nginx"
        ));
        // A needle that is not in the row does not.
        assert!(!fuzzy_match(
            &mut matcher,
            &mut needle_buf,
            &mut haystack_buf,
            "nginx",
            "381 ngin"
        ));
        // The pid is part of the haystack, so a number works too.
        assert!(fuzzy_match(
            &mut matcher,
            &mut needle_buf,
            &mut haystack_buf,
            "1042",
            "1042 rlog"
        ));
        // No filter keeps everything.
        assert!(fuzzy_match(
            &mut matcher,
            &mut needle_buf,
            &mut haystack_buf,
            "",
            "anything at all"
        ));
    }

    #[test]
    fn the_filter_narrows_both_panes_at_once() {
        let mut app = app(scratch("filter"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        for i in 1..=6 {
            app.log_lines.push_back(format!("line {i}"));
        }
        rebuild_log_view(&mut app);

        // No filter: every row and every line is in view.
        assert_eq!(app.view.len(), 4);
        assert_eq!(app.log_view.len(), 6);

        app.filter = Some(Input::from("vim"));
        rebuild_view(&mut app);
        rebuild_log_view(&mut app);
        // nvim, and nothing else, in the table; no log line matches "vim".
        let names: Vec<&str> = app.view.iter().map(|&i| app.processes[i].name.as_str()).collect();
        assert_eq!(names, ["nvim"]);
        assert!(app.log_view.is_empty());

        // A needle that matches log lines: the pane narrows to them.
        app.filter = Some(Input::from("line 3"));
        rebuild_view(&mut app);
        rebuild_log_view(&mut app);
        assert!(app.view.is_empty());
        assert_eq!(app.log_view.len(), 1);
        assert_eq!(app.log_lines[app.log_view[0]], "line 3");
    }

    #[test]
    fn closing_the_filter_with_esc_clears_it_and_enter_keeps_it() {
        let mut app = app(scratch("close"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        open_filter(&mut app);
        if let Some(input) = app.filter.as_mut() {
            input.handle(InputRequest::InsertChar('v'));
            input.handle(InputRequest::InsertChar('i'));
            input.handle(InputRequest::InsertChar('m'));
        }
        rebuild_view(&mut app);
        assert_eq!(app.view.len(), 1);

        // Enter: the editor closes, the narrowed table stays narrowed — and
        // stays narrowed across the refresh that step 6's code used to lose
        // it to, because the value now outlives the editor.
        close_filter(&mut app, false);
        assert!(!app.editing);
        assert!(app.filter.is_some());
        assert_eq!(app.view.len(), 1);
        rebuild_view(&mut app);
        assert_eq!(app.view.len(), 1);

        // Esc: the value goes, and everything is back.
        open_filter(&mut app);
        if let Some(input) = app.filter.as_mut() {
            input.handle(InputRequest::InsertChar('n'));
        }
        close_filter(&mut app, true);
        assert!(app.filter.is_none());
        rebuild_view(&mut app);
        assert_eq!(app.view.len(), 4);
    }

    #[test]
    fn the_sort_keys_move_through_the_columns() {
        let mut app = app(scratch("sort-keys"));
        app.processes = rows();
        assert_eq!(app.sort, SortColumn::Cpu);
        assert!(app.sort_desc);
        // `s` cycles cpu → mem → pid → cpu, and `r` flips the direction.
        let event = |code: KeyCode| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(apply(&mut app, event(KeyCode::Char('s'))));
        assert_eq!(app.sort, SortColumn::Mem);
        assert!(apply(&mut app, event(KeyCode::Char('s'))));
        assert_eq!(app.sort, SortColumn::Pid);
        assert!(apply(&mut app, event(KeyCode::Char('r'))));
        assert!(!app.sort_desc);
        // And a keypress is still reported by the status line.
        assert!(apply(&mut app, event(KeyCode::Char('x'))));
        assert_eq!(app.last_key.unwrap().code, KeyCode::Char('x'));
    }

    #[test]
    fn the_history_is_a_bounded_ring() {
        let mut app = app(scratch("history"));
        app.processes = rows();
        // More refreshes than the window holds: the oldest samples fall off
        // the front, the newest is always last.
        for i in 0..HISTORY_CAPACITY + 5 {
            app.processes[0].cpu = i as f32;
            record_history(&mut app);
        }
        let samples = &app.history[&1042];
        assert_eq!(samples.len(), HISTORY_CAPACITY);
        assert_eq!(*samples.front().unwrap(), 5.0);
        assert_eq!(*samples.back().unwrap(), (HISTORY_CAPACITY + 4) as f32);
    }

    #[test]
    fn processes_that_exit_fall_out_of_the_history() {
        let mut app = app(scratch("history-prune"));
        app.processes = rows();
        record_history(&mut app);
        assert_eq!(app.history.len(), 4);

        // node is gone from the next snapshot.
        app.processes.retain(|row| row.pid != 812);
        record_history(&mut app);
        assert_eq!(app.history.len(), 3);
        assert!(!app.history.contains_key(&812));
        // The survivors kept their earlier samples.
        assert_eq!(app.history[&1042].len(), 2);
    }

    #[test]
    fn the_sparkline_series_puts_the_newest_sample_at_the_right_edge() {
        let mut samples = VecDeque::new();
        samples.push_back(0.0);
        samples.push_back(1.5);
        samples.push_back(3.0);

        // Three samples in an eight-column pane: five absent columns, then
        // the window against the right edge. Percent is scaled by a hundred.
        let series = sparkline_data(&samples, 8);
        assert_eq!(series.len(), 8);
        assert_eq!(series[..5], [None; 5]);
        assert_eq!(series[5], Some(0));
        assert_eq!(series[6], Some(150));
        assert_eq!(series[7], Some(300));

        // A window longer than the pane keeps the newest samples, with no
        // padding at all.
        let mut full = VecDeque::new();
        for i in 0..10 {
            full.push_back(i as f32);
        }
        assert_eq!(
            sparkline_data(&full, 4),
            vec![Some(600), Some(700), Some(800), Some(900)]
        );
    }

    #[test]
    fn t_cycles_the_themes() {
        let mut app = app(scratch("themes"));
        assert_eq!(app.theme.name, "slate");
        let event = |code: KeyCode| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(apply(&mut app, event(KeyCode::Char('t'))));
        assert_eq!(app.theme.name, "ember");
        assert!(apply(&mut app, event(KeyCode::Char('t'))));
        assert_eq!(app.theme.name, "mono");
        assert!(apply(&mut app, event(KeyCode::Char('t'))));
        assert_eq!(app.theme.name, "slate");
    }

    #[test]
    fn the_help_overlay_swallows_keys_until_a_key_closes_it() {
        let mut app = app(scratch("help"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        move_selection(&mut app, 1);
        let selected = app.selected;
        let event = |code: KeyCode| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));

        // `?` opens it.
        assert!(apply(&mut app, event(KeyCode::Char('?'))));
        assert!(app.help);

        // Any key closes it and does nothing else: `j` does not move the
        // selection.
        app.dirty = false;
        assert!(apply(&mut app, event(KeyCode::Char('j'))));
        assert!(!app.help);
        assert_eq!(app.selected, selected);
        assert!(app.dirty);

        // Ctrl-C still ends the program while the overlay is up.
        assert!(apply(&mut app, event(KeyCode::Char('?'))));
        assert!(!apply(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        ));
    }

    #[test]
    fn space_pauses_the_tail_and_the_title_says_so() {
        let mut app = app(scratch("pause"));
        for i in 1..=30 {
            app.log_lines.push_back(format!("line {i}"));
        }
        rebuild_log_view(&mut app);
        let event = |code: KeyCode| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        assert!(!app.paused);

        assert!(apply(&mut app, event(KeyCode::Char(' '))));
        assert!(app.paused);
        assert!(app.dirty);
        assert!(log_title(&app).contains("paused"));

        assert!(apply(&mut app, event(KeyCode::Char(' '))));
        assert!(!app.paused);
        assert!(!log_title(&app).contains("paused"));
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
        assert_eq!(app.log_view.len(), 1);
        assert_eq!(app.log_offset, 11);

        // The rest of the line arrives on the next poll and the halves join.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&app.log_path)
            .unwrap();
        file.write_all(b"ne\n").unwrap();
        poll_log(&mut app).unwrap();
        assert_eq!(app.log_view.len(), 2);
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
        assert_eq!(app.log_view.len(), 1);
        assert_eq!(app.log_offset, 6);

        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"second\nthird\n").unwrap();
        poll_log(&mut app).unwrap();
        assert_eq!(app.log_view.len(), 3);
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
        assert_eq!(app.log_view.len(), 2);

        // The file is emptied and refilled with something shorter: the tail
        // notices the length went backwards and reads from the top again.
        std::fs::write(&path, b"gamma\n").unwrap();
        poll_log(&mut app).unwrap();
        assert_eq!(app.log_view.len(), 1);
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
        assert_eq!(app.log_view.len(), 0);

        // The file appears with contents: the next poll picks them up.
        std::fs::write(&app.log_path, b"late line\n").unwrap();
        poll_log(&mut app).unwrap();
        assert!(!app.log_missing);
        assert_eq!(app.log_view.len(), 1);
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
            app.log_lines.push_back(format!("line {i}"));
        }
        // At 100x30 the pane shows 22 of the 30 lines, so an offset of 2 is
        // legal and stays put.
        *app.log.offset_mut() = 2;
        app.dirty = false;
        rebuild_log_view(&mut app);
        assert_eq!(app.log.offset(), 2);
        assert!(!app.dirty);

        // A truncation empties the tail: the offset comes back with it, and
        // that is a change the frame has to show.
        app.log_lines.clear();
        rebuild_log_view(&mut app);
        assert_eq!(app.log.offset(), 0);
        assert!(app.dirty);
    }

    #[test]
    fn a_keypress_leaves_the_frame_dirty_and_q_ends_the_loop() {
        let mut app = app(scratch("apply-key"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        app.dirty = false;
        // `j` moves the selection, and that is a change the frame shows.
        assert!(apply(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
        ));
        assert!(app.dirty);
        assert!(app.selected.is_some());

        // Any other keypress is reported by the status line.
        app.dirty = false;
        assert!(apply(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
        ));
        assert!(app.dirty);
        assert_eq!(app.last_key.unwrap().code, KeyCode::Char('x'));

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
    fn a_key_pressed_while_the_filter_is_open_types_instead_of_quitting() {
        let mut app = app(scratch("apply-filter"));
        open_filter(&mut app);
        // `q` is a letter now, not a quit.
        assert!(apply(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        ));
        assert_eq!(app.filter.as_ref().unwrap().value(), "q");
        // Ctrl-C still ends the program.
        assert!(!apply(
            &mut app,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL))
        ));
    }

    #[test]
    fn a_wheel_notch_scrolls_the_log_pane() {
        let mut app = app(scratch("apply-wheel"));
        for i in 1..=30 {
            app.log_lines.push_back(format!("line {i}"));
        }
        rebuild_log_view(&mut app);
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
    fn a_wheel_notch_over_the_history_pane_scrolls_nothing() {
        let mut app = app(scratch("apply-wheel-history"));
        for i in 1..=30 {
            app.log_lines.push_back(format!("line {i}"));
        }
        rebuild_log_view(&mut app);
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        app.dirty = false;
        // Row 26 is inside the history pane at 100x30 (rows 25-27), which has
        // no offset to move.
        let mouse = MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 40,
            row: 26,
            modifiers: KeyModifiers::NONE,
        };
        assert!(apply(&mut app, Event::Mouse(mouse)));
        assert_eq!(app.log.offset(), 0);
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
        let [header, table, log, history, filter, status] = layout(Rect::new(0, 0, 100, 30));
        assert_eq!(header, Rect::new(0, 0, 100, 1));
        assert_eq!(table, Rect::new(0, 1, 38, 24));
        assert_eq!(log, Rect::new(38, 1, 62, 24));
        assert_eq!(history, Rect::new(0, 25, 100, 3));
        assert_eq!(filter, Rect::new(0, 28, 100, 1));
        assert_eq!(status, Rect::new(0, 29, 100, 1));
    }

    #[test]
    fn a_narrow_terminal_stacks_the_panes() {
        let [header, table, log, history, filter, status] = layout(Rect::new(0, 0, 60, 24));
        assert_eq!(header, Rect::new(0, 0, 60, 1));
        assert_eq!(table, Rect::new(0, 1, 60, 11));
        assert_eq!(log, Rect::new(0, 12, 60, 7));
        assert_eq!(history, Rect::new(0, 19, 60, 3));
        assert_eq!(filter, Rect::new(0, 22, 60, 1));
        assert_eq!(status, Rect::new(0, 23, 60, 1));
    }

    #[test]
    fn a_terminal_too_short_for_the_panes_keeps_the_fixed_rows() {
        // Six rows is the floor: the header, the history pane, the filter line
        // and the status line take all of them, and the two panes get nothing.
        // Rendering an empty rect draws nothing rather than complaining.
        let [header, table, log, history, filter, status] = layout(Rect::new(0, 0, 40, 6));
        assert_eq!(header, Rect::new(0, 0, 40, 1));
        assert_eq!(table, Rect::new(0, 1, 40, 0));
        assert_eq!(log, Rect::new(0, 1, 40, 0));
        assert_eq!(history, Rect::new(0, 1, 40, 3));
        assert_eq!(filter, Rect::new(0, 4, 40, 1));
        assert_eq!(status, Rect::new(0, 5, 40, 1));
    }

    #[test]
    fn the_frame_renders_at_every_size() {
        // The widgets get zero-height areas at the small end; rendering one
        // is allowed to draw nothing, not to panic. The sweep runs twice, with
        // a full tail and with an empty one, because a pane with no lines yet
        // is the state the program starts in — and a third time with the help
        // overlay up, because an overlay over a squeezed frame is the state a
        // reader produces by pressing `?` on a small terminal.
        let mut app = app(scratch("render"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        move_selection(&mut app, 1);
        record_history(&mut app);
        for filled in [false, true] {
            app.log_lines.clear();
            if filled {
                for i in 1..=30 {
                    app.log_lines.push_back(format!("line {i}"));
                }
            }
            rebuild_log_view(&mut app);
            for (w, h) in [
                (100, 30),
                (80, 24),
                (79, 24),
                (46, 18),
                (40, 12),
                (40, 11),
                (40, 6),
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
        app.help = true;
        for (w, h) in [(100, 30), (60, 24), (40, 6), (0, 0)] {
            app.size = (w, h);
            let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
            terminal.draw(|frame| draw(frame, &app)).unwrap();
        }
    }

    #[test]
    fn the_log_pane_names_its_position() {
        let mut app = app(scratch("pane"));
        for i in 1..=30 {
            app.log_lines
                .push_back(format!("2026-10-24T21:00:{i:02}Z INFO  line {i}"));
        }
        rebuild_log_view(&mut app);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        // The log pane's top border is row 1; the pane titles sit on it.
        let top: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect();
        assert!(top.contains("log [1/30]"), "pane title was {top:?}");

        // An empty view says so instead of claiming a first line.
        app.log_lines.clear();
        rebuild_log_view(&mut app);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let top: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 1)].symbol())
            .collect();
        assert!(top.contains("log empty"), "pane title was {top:?}");
    }

    #[test]
    fn the_history_pane_draws_the_selected_process() {
        let mut app = app(scratch("history-pane"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        // rlog is the top row at the default sort, and the selection follows.
        move_selection(&mut app, 1);
        assert_eq!(app.selected, Some(1042));
        // Three refreshes with the same numbers: three identical samples.
        for _ in 0..3 {
            record_history(&mut app);
        }
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        // The pane's title is on its top border, row 25 at this size.
        let top: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 25)].symbol())
            .collect();
        assert!(
            top.contains("cpu history · rlog (pid 1042) · 8.1% now · 3/60 samples"),
            "pane title was {top:?}"
        );
        // The bar row is the pane's only inner row: three samples against the
        // right edge, every one of them the maximum, so every bar is full.
        let inner: String = (1..buffer.area.width - 1)
            .map(|x| buffer[(x, 26)].symbol())
            .collect();
        assert!(inner.ends_with("███"), "bar row was {inner:?}");
    }

    #[test]
    fn the_history_pane_says_so_when_nothing_is_selected() {
        let mut app = app(scratch("history-empty"));
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let top: String = (0..buffer.area.width)
            .map(|x| buffer[(x, 25)].symbol())
            .collect();
        assert!(
            top.contains("cpu history · no process selected"),
            "pane title was {top:?}"
        );
    }

    #[test]
    fn the_help_overlay_covers_the_frame() {
        let mut app = app(scratch("help-pane"));
        app.processes = rows();
        sort_processes(&mut app.processes, app.sort, app.sort_desc);
        rebuild_view(&mut app);
        app.help = true;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();

        // The overlay is centred: at 100x30 it starts a sixth in and a quarter
        // down, two thirds wide and half the height.
        let overlay = help_overlay(Rect::new(0, 0, 100, 30));
        assert_eq!(overlay, Rect::new(16, 7, 66, 15));

        // Its top border carries the title.
        let top: String = (0..buffer.area.width)
            .map(|x| buffer[(x, overlay.y)].symbol())
            .collect();
        assert!(top.contains("keys"), "overlay top was {top:?}");

        // Its fifth row is the `space` line, not the table underneath —
        // `Clear` wiped the table before the overlay drew.
        let row: String = (0..buffer.area.width)
            .map(|x| buffer[(x, overlay.y + 5)].symbol())
            .collect();
        assert!(row.contains("pause the tail"), "overlay row was {row:?}");

        // And a cell the table drew but the overlay covers is blank now.
        assert_eq!(buffer[(overlay.x + 2, overlay.y + 11)].symbol(), " ");
    }
}
