use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
        EnableFocusChange, EnableMouseCapture, EventStream, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::StreamExt;
use intercom_tui::{
    app::App,
    platform::{self, PlatformEvent},
    storage::Store,
    ui,
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(
    version,
    about = "Portable Intercom inbox for Linux and macOS",
    long_about = "An interactive Intercom inbox with Markdown replies, notes, scheduling, local archive search, and native desktop notifications. Runs only while this terminal app is open; no daemon, tray icon, or Omarchy dependency."
)]
struct Args {
    /// Read the access token from this file (INTERCOM_ACCESS_TOKEN takes precedence)
    #[arg(long)]
    token_file: Option<PathBuf>,
    /// Store config/ and data/ under this directory instead of platform defaults
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Start on a conversation, even if it is outside the saved inbox filter
    #[arg(long)]
    conversation: Option<String>,
    /// Explore the interface with sample conversations; no network or account writes
    #[arg(long)]
    demo: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "intercom-tui needs an interactive terminal. Use --help for command-line options."
        );
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    #[cfg(target_os = "macos")]
    {
        // Notification Center delivers click callbacks on the main NSRunLoop.
        // Keep it pumping while Tokio owns the terminal and network work.
        mac_usernotifications::block_on_main(async {
            runtime
                .spawn(run(args))
                .await
                .context("Application task failed")?
        })
    }
    #[cfg(not(target_os = "macos"))]
    runtime.block_on(run(args))
}

struct TerminalGuard {
    enhanced: bool,
}

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut guard = Self { enhanced: false };
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange
        )?;
        if crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false) {
            execute!(
                io::stdout(),
                PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
            )?;
            guard.enhanced = true;
        }
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.enhanced {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste,
            DisableFocusChange,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        let _ = disable_raw_mode();
    }
}

async fn run(args: Args) -> Result<()> {
    let store = Store::open(args.data_dir.as_deref())?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let (platform_tx, mut platform_rx) = mpsc::unbounded_channel();
    let mut app = App::new(
        store,
        args.token_file,
        args.conversation,
        tx,
        platform_tx,
        args.demo,
    )?;
    let _guard = TerminalGuard::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut input = EventStream::new();
    let mut ticks = tokio::time::interval(Duration::from_millis(100));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut redraw = true;
    let mut last_draw = Instant::now();
    let panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
        panic_hook(info);
    }));
    loop {
        if redraw || last_draw.elapsed() >= Duration::from_secs(1) {
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            redraw = false;
            last_draw = Instant::now();
        }
        tokio::select! {
            event = input.next() => {
                match event {
                    Some(Ok(event)) => if let Err(error) = ui::input(&mut app, event) { app.message = error.to_string(); },
                    Some(Err(error)) => { app.flush()?; return Err(error).context("Terminal input failed"); },
                    None => break,
                }
                redraw = true;
            }
            Some(event) = rx.recv() => {
                if let Err(error) = app.handle(event) { app.message = format!("Storage/application error: {error:#}"); }
                redraw = true;
            }
            Some(event) = platform_rx.recv() => {
                match event {
                    PlatformEvent::Open { id, generation } if generation == app.generation() => {
                        if app.sending {
                            app.message = "Notification clicked. Wait for the current send before switching conversations.".into();
                        } else {
                            if app.view.composer.is_some() { app.close_composer(false)?; }
                            app.cancel_search();
                            app.view.search_active = false;
                            app.view.mode = ui::Mode::Inbox;
                            app.view.pane = ui::Pane::Thread;
                            app.select_id(id, true);
                            platform::focus_terminal();
                        }
                    }
                    PlatformEvent::Warning { message, generation } if generation == app.generation() => app.message = message,
                    _ => (),
                }
                redraw = true;
            }
            _ = ticks.tick() => if let Err(error) = app.tick() { app.message = format!("Could not save or send: {error:#}"); redraw = true; },
            _ = terminate.recv() => { app.flush()?; break; },
            _ = tokio::signal::ctrl_c() => { app.flush()?; break; },
        }
        if app.quit {
            break;
        }
    }
    app.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn application_future_can_run_off_the_macos_main_thread() {
        fn assert_send<T: Send>(_: T) {}
        assert_send(run(Args {
            token_file: None,
            data_dir: None,
            conversation: None,
            demo: true,
        }));
    }
}
