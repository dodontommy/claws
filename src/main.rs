use anyhow::Result;
use clap::{Parser, Subcommand};
use uuid::Uuid;

mod auth;
mod client;
mod daemon;
mod git;
mod hook;
mod paths;
mod persist;
mod protocol;
mod registry;
mod ring;
mod session;
mod spawn;
mod theme;
mod tui;

#[derive(Parser)]
#[command(name = "claws", version, about = "TUI multiplexer for Claude Code sessions")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground (debugging)
    Daemon,
    /// Send a ping to the daemon (auto-spawns one if needed)
    Ping,
    /// Stop the daemon and all sessions
    KillServer,
    /// Print the log file path
    Logs,
    /// Spawn a new claude session in the daemon
    New {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// List all sessions in the daemon
    List,
    /// Send raw bytes to a session's stdin (escapes \n, \r, \t)
    Send {
        session_id: String,
        data: String,
    },
    /// Read PTY output from a session, starting at `since`
    Read {
        session_id: String,
        #[arg(long, default_value_t = 0)]
        since: u64,
    },
    /// Close (kill) a session
    Close { session_id: String },
    /// Internal: invoked by Claude Code hooks. Reads payload from stdin.
    #[command(hide = true)]
    HookEmit {
        #[arg(long)]
        session: Uuid,
        #[arg(long)]
        event: String,
    },
    /// Check for and install the latest release.
    Update,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Daemon (foreground) logs to stderr; everyone else logs to file so TUI
    // and hook-emit don't trample the user's terminal.
    let to_stderr = matches!(cli.command, Some(Command::Daemon))
        || std::env::var("CLAWS_LOG_STDERR").is_ok();
    init_tracing(to_stderr);
    install_panic_hook();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async {
        match cli.command {
            Some(Command::Daemon) => daemon::run().await,
            None => {
                theme::load();
                tui::run().await
            },
            Some(Command::Ping) => client::ping().await,
            Some(Command::KillServer) => client::kill_server().await,
            Some(Command::Logs) => {
                println!("{}", paths::log_file()?.display());
                Ok(())
            }
            Some(Command::New { cwd, name, model }) => {
                let cwd = cwd
                    .map(Ok)
                    .unwrap_or_else(|| {
                        std::env::current_dir().map(|p| p.to_string_lossy().into_owned())
                    })?;
                client::create_session(cwd, name, model).await
            }
            Some(Command::List) => client::list_sessions().await,
            Some(Command::Send { session_id, data }) => client::send_input(session_id, data).await,
            Some(Command::Read { session_id, since }) => client::read_output(session_id, since).await,
            Some(Command::Close { session_id }) => client::close_session(session_id).await,
            Some(Command::HookEmit { session, event }) => hook::run_hook_emit(session, event).await,
            Some(Command::Update) => run_update().await,
        }
    })
}

async fn run_update() -> Result<()> {
    use axoupdater::AxoUpdater;
    let mut updater = AxoUpdater::new_for("claws");
    if let Err(e) = updater.load_receipt() {
        anyhow::bail!(
            "install receipt missing — `claws` was probably installed via `cargo install` \
             rather than the official installer.\n\n\
             To enable in-place updates, reinstall via one of:\n  \
               curl --proto '=https' --tlsv1.2 -LsSf https://github.com/dodontommy/claws/releases/latest/download/claws-installer.sh | sh\n  \
               powershell -c \"irm https://github.com/dodontommy/claws/releases/latest/download/claws-installer.ps1 | iex\"\n  \
               brew install dodontommy/tap/claws\n\n\
             Underlying error: {e}"
        );
    }
    match updater.run().await {
        Ok(Some(_)) => {
            println!("updated claws — relaunch to use the new version.");
            Ok(())
        }
        Ok(None) => {
            println!("claws is already up to date.");
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!("update failed: {e}")),
    }
}

fn init_tracing(to_stderr: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    // Silence vt100's per-frame "unhandled" debug spam — those are about
    // Claude's terminal features (kitty keyboard, focus events, synchronized
    // updates) that we don't need to handle and the screen renders fine without.
    let default = "info,vt100=warn";
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));

    if to_stderr {
        fmt().with_env_filter(filter).with_target(false).init();
        return;
    }
    if let Ok(path) = paths::log_file() {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let writer = std::sync::Mutex::new(file);
            fmt()
                .with_env_filter(filter)
                .with_target(false)
                .with_ansi(false)
                .with_writer(writer)
                .init();
            return;
        }
    }
    // Final fallback if file open fails: stderr.
    fmt().with_env_filter(filter).with_target(false).init();
}

fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort: leave alt screen and disable raw mode before the
        // panic message hits stderr, so the user's terminal isn't wedged.
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        original(info);
    }));
}
