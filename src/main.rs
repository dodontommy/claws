use anyhow::Result;
use clap::{Parser, Subcommand};
use uuid::Uuid;

mod auth;
mod client;
mod config;
mod daemon;
mod git;
mod hook;
mod paths;
mod persist;
mod phone;
mod pidfile;
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
    KillServer {
        /// Force-kill the daemon process by PID, bypassing the auth-protected
        /// shutdown RPC. Use when the daemon is unresponsive or the auth
        /// token on disk has somehow drifted from what the daemon expects.
        #[arg(long)]
        force: bool,
    },
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
    /// Phone companion (PWA over HTTP/WS) — start, pair, manage devices.
    Phone {
        #[command(subcommand)]
        action: PhoneAction,
    },
}

#[derive(Subcommand)]
enum PhoneAction {
    /// Start the phone listener (loopback only; front with Tailscale serve
    /// or Cloudflared for HTTPS + cellular reach). Persists across restarts.
    Start {
        /// Bind address. Must be loopback in this version.
        #[arg(long, default_value = "127.0.0.1:9817")]
        bind: String,
        /// Override the embedded PWA assets at runtime. When set, the static
        /// handler reads HTML/JS/CSS from this directory first (falling back
        /// to embedded for missing files). Edit, refresh phone, see changes
        /// — no rebuild, no kill-server, no session disruption. Pass an
        /// empty string to clear a previously-set override.
        #[arg(long)]
        pwa_dir: Option<String>,
    },
    /// Stop the phone listener and clear the persisted-enabled flag.
    Stop,
    /// Print listener status, bind URL, and device count.
    Status,
    /// Mint a pair code, print it + a QR for the URL the phone should open.
    /// Pass `--url` once to tell claws what URL the phone should hit
    /// (typically the tailnet HTTPS hostname); it's persisted so future
    /// `claws phone pair` runs encode the right host into the QR.
    Pair {
        /// Public URL the phone should open, e.g.
        /// `https://my-machine.tail-abc.ts.net`. Saved to phone.json on
        /// success so subsequent pair runs reuse it.
        #[arg(long)]
        url: Option<String>,
    },
    /// List paired devices with id, label, paired-at, last-seen.
    Devices,
    /// Revoke a paired device by id (from `phone devices`).
    Revoke {
        device_id: String,
    },
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
            Some(Command::KillServer { force }) => {
                if force {
                    match pidfile::force_kill() {
                        Ok(true) => {
                            println!("daemon killed");
                            Ok(())
                        }
                        Ok(false) => {
                            println!("(no daemon found)");
                            Ok(())
                        }
                        Err(e) => Err(e),
                    }
                } else {
                    client::kill_server().await
                }
            }
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
            Some(Command::Phone { action }) => run_phone(action).await,
        }
    })
}

async fn run_phone(action: PhoneAction) -> Result<()> {
    use serde_json::json;
    match action {
        PhoneAction::Start { bind, pwa_dir } => {
            let mut params = json!({"bind": bind});
            if let Some(d) = pwa_dir.as_ref() {
                params["pwa_dir"] = json!(d);
            }
            let res = client::call("phone_start", params).await?;
            let bind = res.get("bind").and_then(|v| v.as_str()).unwrap_or("?");
            if res.get("already_running").and_then(|v| v.as_bool()).unwrap_or(false) {
                println!("phone listener already running on {bind}");
            } else {
                println!("phone listener up on http://{bind}");
            }
            if let Some(d) = pwa_dir.as_ref() {
                if d.is_empty() {
                    println!("PWA override cleared — serving embedded assets.");
                } else {
                    println!("PWA hot-reload: serving from {d}");
                    println!("(edit files there, refresh the phone — no restart needed)");
                }
            }
            println!("\nNext: front this with HTTPS for cellular reach. One of:");
            println!("  tailscale serve --bg --https=443 http://{bind}");
            println!("  cloudflared tunnel --url http://{bind}");
            println!("\nThen run `claws phone pair` to add your phone.");
            Ok(())
        }
        PhoneAction::Stop => {
            let res = client::call("phone_stop", json!({})).await?;
            if res.get("stopped").and_then(|v| v.as_bool()).unwrap_or(false) {
                println!("phone listener stopped");
            } else {
                println!("(phone listener was not running)");
            }
            Ok(())
        }
        PhoneAction::Status => {
            let res = client::call("phone_status", json!({})).await?;
            let running = res.get("running").and_then(|v| v.as_bool()).unwrap_or(false);
            let bind = res.get("bind").and_then(|v| v.as_str()).unwrap_or("-");
            let dev = res.get("device_count").and_then(|v| v.as_u64()).unwrap_or(0);
            println!("running:  {}", if running { "yes" } else { "no" });
            println!("bind:     {bind}");
            println!("devices:  {dev}");
            Ok(())
        }
        PhoneAction::Pair { url } => {
            let mut params = json!({});
            if let Some(u) = url.as_ref() {
                params["set_url"] = json!(u);
            }
            let res = client::call("phone_pair_code", params).await?;
            let code = res.get("code").and_then(|v| v.as_str()).unwrap_or("");
            let bind = res.get("bind").and_then(|v| v.as_str()).unwrap_or("");
            // public_url honors: --url override → saved phone.json → tailscale auto-detect → bind fallback
            let public_url = res
                .get("public_url")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(bind);
            let phone_url = if public_url.starts_with("http") {
                format!("{public_url}/#code={code}")
            } else {
                format!("http://{public_url}/#code={code}")
            };
            println!("Pair code: {code}");
            println!("URL:       {phone_url}");
            println!("\nTTL: 10 minutes. Single use.\n");
            print_qr(&phone_url);
            if !public_url.starts_with("http") || public_url.starts_with("http://127.")
                || public_url.starts_with("http://localhost")
            {
                println!();
                println!("⚠  This URL points at loopback ({public_url}). Your phone won't reach it.");
                println!("   Front the listener with `tailscale serve --bg --https=443 http://{bind}`,");
                println!("   then re-run `claws phone pair --url https://<machine>.<tailnet>.ts.net`");
                println!("   so the QR encodes the right host. We'll auto-detect Tailscale next time.");
            }
            Ok(())
        }
        PhoneAction::Devices => {
            let res = client::call("phone_devices", json!({})).await?;
            let arr = res.as_array().cloned().unwrap_or_default();
            if arr.is_empty() {
                println!("(no paired devices)");
                return Ok(());
            }
            println!("{:<38}  {:<24}  paired", "id", "label");
            for d in arr {
                let id = d.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                let label = d
                    .get("label")
                    .and_then(|v| v.as_str())
                    .unwrap_or("(unlabeled)");
                let paired = d.get("paired_at_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                println!("{id:<38}  {label:<24}  {}", format_ts(paired));
            }
            Ok(())
        }
        PhoneAction::Revoke { device_id } => {
            let res = client::call("phone_revoke", json!({"device_id": device_id})).await?;
            if res.get("removed").and_then(|v| v.as_bool()).unwrap_or(false) {
                println!("revoked {device_id}");
            } else {
                println!("no device with id {device_id}");
            }
            Ok(())
        }
    }
}

fn format_ts(ms: u64) -> String {
    if ms == 0 {
        return "-".to_string();
    }
    let secs = ms / 1000;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dt = now.saturating_sub(secs);
    if dt < 60 { format!("{dt}s ago") }
    else if dt < 3600 { format!("{}m ago", dt / 60) }
    else if dt < 86_400 { format!("{}h ago", dt / 3600) }
    else { format!("{}d ago", dt / 86_400) }
}

fn print_qr(url: &str) {
    use qrcode::{render::unicode, QrCode};
    match QrCode::new(url.as_bytes()) {
        Ok(q) => {
            let s = q
                .render::<unicode::Dense1x2>()
                .quiet_zone(true)
                .module_dimensions(1, 1)
                .build();
            println!("{s}");
        }
        Err(e) => {
            tracing::warn!(error = %e, "qr render failed; falling back to URL only");
        }
    }
}

async fn run_update() -> Result<()> {
    use axoupdater::AxoUpdater;
    let mut updater = AxoUpdater::new_for("claws");
    if let Err(e) = updater.load_receipt() {
        // Homebrew installs land under a `Cellar` directory and don't write
        // the dist install receipt axoupdater needs. Detect that case so we
        // can point the user at `brew upgrade` instead of falsely claiming
        // they used `cargo install`.
        let exe = std::env::current_exe().ok();
        let is_brew = exe
            .as_deref()
            .map(|p| p.components().any(|c| c.as_os_str() == "Cellar"))
            .unwrap_or(false);
        if is_brew {
            anyhow::bail!(
                "claws was installed via Homebrew — `claws update` only works for the \
                 shell/powershell installers. Run:\n  \
                   brew update && brew upgrade dodontommy/tap/claws"
            );
        }
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
