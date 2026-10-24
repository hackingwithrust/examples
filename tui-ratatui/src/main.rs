//! rlog — a terminal UI that follows a log file and watches the process table.
//!
//! Step 2 of the track: the project skeleton, raw mode, the alternate screen,
//! and one bordered box drawn by ratatui. The later steps fill the frame in.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Paragraph};

/// How long the loop waits for a key before redrawing anyway. One tick of the
/// program's clock; the refresh schedules of later steps are measured against
/// it.
const TICK: Duration = Duration::from_millis(250);

/// The whole state of the program at this step: how many frames it has drawn.
/// Every later step grows this struct — the log tail, the process table, the
/// selection — and `draw` stays a pure function of it.
struct App {
    frames: u64,
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
    execute!(io::stdout(), EnterAlternateScreen)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App { frames: 0 };
    let result = run(&mut terminal, &mut app);

    leave_full_screen()?;
    result
}

/// Undo what `main` set up: leave the alternate screen, then raw mode. The
/// order matters — raw mode is what makes the escape sequences arrive intact.
fn leave_full_screen() -> io::Result<()> {
    execute!(io::stdout(), LeaveAlternateScreen)?;
    disable_raw_mode()
}

/// The render loop. Every pass draws a frame, then waits one tick for a key.
/// `q`, `Esc` and `Ctrl-C` end the loop and give the terminal back.
fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        app.frames += 1;
        terminal.draw(|frame| draw(frame, app))?;

        if event::poll(TICK)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && is_quit(key) {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// The whole frame for this step: one bordered box filling the screen, with
/// the frame count inside it. The count is what makes the loop's redraws
/// visible — ratatui writes only the cells that changed.
fn draw(frame: &mut Frame, app: &App) {
    let block = Block::bordered().title(" rlog ");
    frame.render_widget(
        Paragraph::new(format!("frame {} — press q to quit", app.frames)),
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
