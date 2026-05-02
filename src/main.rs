use anyhow::Result;
use clap::{Parser, Subcommand};
use uuid::Uuid;

mod client;
mod daemon;
mod hook;
mod paths;
mod protocol;
mod registry;
mod ring;
mod session;
mod spawn;
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
}

fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async {
        match cli.command {
            Some(Command::Daemon) => daemon::run().await,
            None => tui::run().await,
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
        }
    })
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}
