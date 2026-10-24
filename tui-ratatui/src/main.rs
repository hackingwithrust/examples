//! rlog — a terminal UI that follows a log file and watches the process table.
//!
//! Step 3 of the track: the event loop reads every kind of event the terminal
//! produces — keys, the mouse, and a resize — and the frame reports the last of
//! each. Step 4 turns the box into panes; the events read here are what those
//! panes answer.

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
use ratatui::widgets::{Block, Paragraph};

/// How long the loop waits for an event before redrawing anyway. One tick of
/// the program's clock; the refresh schedules of later steps are measured
/// against it.
const TICK: Duration = Duration::from_millis(250);

/// The whole state of the program at this step. Step 2 carried only the frame
/// counter; the event step adds the last event of each kind and the terminal
/// size as of the last resize. Every later step grows this struct — the log
/// tail, the process table, the selection — and `draw` stays a pure function
/// of it.
struct App {
    frames: u64,
    /// The last key press, `None` until the first one arrives.
    last_key: Option<KeyEvent>,
    /// The last mouse event, `None` until the mouse does something.
    last_mouse: Option<MouseEvent>,
    /// Columns and rows, as of the last resize.
    size: (u16, u16),
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
                Event::Mouse(mouse) => app.last_mouse = Some(mouse),
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

/// The whole frame for this step: one bordered box filling the screen, with
/// the frame count, the last key, the last mouse event and the terminal size
/// inside it. Each line is a claim this step can check by hand.
fn draw(frame: &mut Frame, app: &App) {
    let block = Block::bordered().title(" rlog ");
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(format!("frame {} — press q to quit", app.frames)),
            Line::from(format!("last key: {}", describe_key(app.last_key.as_ref()))),
            Line::from(format!(
                "last mouse: {}",
                describe_mouse(app.last_mouse.as_ref())
            )),
            Line::from(format!("terminal: {}x{}", app.size.0, app.size.1)),
        ]),
        block.inner(frame.area()),
    );
    frame.render_widget(block, frame.area());
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
