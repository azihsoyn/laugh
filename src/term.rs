use std::io;

use anyhow::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

pub type Term = Terminal<CrosstermBackend<io::Stdout>>;

/// Hands the terminal back to the shell, for a command that needs it.
pub fn suspend(terminal: &mut Term) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Takes the terminal back after [`suspend`] and redraws from scratch.
pub fn resume(terminal: &mut Term) -> Result<()> {
    enable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableMouseCapture
    )?;
    terminal.clear()?;
    Ok(())
}

/// Turns mouse reporting on or off. Off, the terminal takes the mouse
/// back: dragging selects text to copy.
pub fn set_mouse(terminal: &mut Term, on: bool) -> Result<()> {
    if on {
        execute!(terminal.backend_mut(), EnableMouseCapture)?;
    } else {
        execute!(terminal.backend_mut(), DisableMouseCapture)?;
    }
    Ok(())
}

/// Runs `body` on the alternate screen in raw mode with mouse reporting on,
/// restoring the terminal afterwards whether or not `body` failed.
pub fn with_terminal(body: impl FnOnce(&mut Term) -> Result<()>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = body(&mut terminal);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}
