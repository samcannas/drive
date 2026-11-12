//! `drive` — the Drive background service and its command-line interface.
//!
//! `drive daemon` runs the service (the tray app starts it automatically). Every other
//! subcommand talks to the running service over its local socket.

mod config;
mod ipc_server;
mod platform;
mod service;

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use drive_core::ipc::{
    Activity, Call, ErrorCode, Issue, IssueAction, RecoveryKit, Settings, SettingsPatch,
    SetupRequest, SetupResult, Status, TransferList, TransferMode, TransferState,
};
use drive_core::ipc_client::{Client, ClientError};
use tracing::{error, info};

#[derive(Parser)]
#[command(
    name = "drive",
    version,
    about = "Encrypted cloud drive backed by uloz.to"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the background service (normally started by the tray app or systemd).
    Daemon {
        /// Also log to the terminal.
        #[arg(long)]
        foreground: bool,
    },
    /// Show sync status.
    Status,
    /// Connect a uloz.to account (asks for passwords interactively).
    Setup {
        /// uloz.to login (user name or email).
        #[arg(long)]
        login: String,
        /// Local folder (Windows) or mount point (Linux).
        #[arg(long)]
        folder: Option<String>,
        /// Ask for an existing encryption password instead of generating a new one.
        #[arg(long)]
        existing_vault: bool,
    },
    /// Pause syncing.
    Pause,
    /// Resume syncing.
    Resume,
    /// Check the cloud for changes now.
    Sync,
    /// Always keep a file or folder on this device.
    Pin { path: String },
    /// Stop keeping a file or folder on this device.
    Unpin { path: String },
    /// Remove downloaded copies of a file or folder (they stay in the cloud).
    Free { path: String },
    /// List items that need attention.
    Issues,
    /// Resolve an issue: retry, dismiss, confirm-delete, restore-deleted.
    Resolve { id: i64, action: String },
    /// Show recent activity.
    Activity {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Show or change settings, e.g. `drive settings cache-limit-gb=20`.
    Settings { changes: Vec<String> },
    /// Print an rclone configuration that opens the vault without Drive.
    Recovery,
    /// Disconnect the account from this device.
    SignOut {
        /// Don't ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Stop the background service.
    Stop,
    /// List uploads and downloads: what's moving, what's waiting and why.
    Transfers {
        /// Show every queued item, not just the first 20.
        #[arg(long)]
        all: bool,
    },
    /// Control one upload: pause, resume, tonight (wait for the overnight window) or now.
    Transfer {
        action: String,
        /// The file's path, or its id from `drive transfers`.
        target: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match cli.command {
        Command::Daemon { foreground } => rt.block_on(run_daemon(foreground)),
        other => rt.block_on(run_client(other)),
    }
}

async fn run_daemon(foreground: bool) -> Result<()> {
    let paths = config::Paths::detect()?;
    paths.ensure()?;
    let _log_guard = init_logging(&paths, foreground)?;
    info!("Drive {} starting", env!("CARGO_PKG_VERSION"));

    let listener = match ipc_server::bind().await {
        Ok(l) => l,
        Err(e)
            if e.downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::AddrInUse) =>
        {
            info!("another instance is already running");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let service = service::Service::new(paths)?;
    tokio::spawn(ipc_server::serve(listener, service.clone()));
    service.start().await;

    tokio::select! {
        _ = service.shutdown.notified() => info!("shutdown requested"),
        _ = tokio::signal::ctrl_c() => info!("interrupted"),
        _ = terminate() => info!("terminated"),
    }
    service.stop().await;
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(drive_core::ipc_client::unix_socket_path());
    }
    Ok(())
}

#[cfg(unix)]
async fn terminate() {
    if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        s.recv().await;
    } else {
        std::future::pending::<()>().await;
    }
}

#[cfg(not(unix))]
async fn terminate() {
    std::future::pending::<()>().await;
}

fn init_logging(
    paths: &config::Paths,
    foreground: bool,
) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    std::fs::create_dir_all(paths.log_dir())?;
    let file = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("drive")
        .filename_suffix("log")
        .max_log_files(7)
        .build(paths.log_dir())
        .context("opening the log file")?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = tracing_subscriber::EnvFilter::try_from_env("DRIVE_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,drive_core=info,drive=info"));
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false);
    let console = foreground.then(|| tracing_subscriber::fmt::layer().with_writer(std::io::stderr));
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(console)
        .init();
    Ok(guard)
}

// ---- client commands -----------------------------------------------------------------

async fn connect() -> Result<Client> {
    match Client::connect().await {
        Ok(c) => Ok(c),
        Err(ClientError::NotRunning(_)) => {
            bail!(
                "The Drive service isn't running. Start the Drive app, or run `drive daemon` in another terminal."
            )
        }
        Err(e) => Err(e.into()),
    }
}

fn human_duration(secs: u64) -> String {
    if secs < 60 {
        "less than a minute".into()
    } else if secs < 3600 {
        format!("about {} min", secs.div_ceil(60))
    } else {
        format!("about {} h {} min", secs / 3600, (secs % 3600) / 60)
    }
}

fn print_transfers(list: &TransferList, all: bool) {
    if list.items.is_empty() {
        println!("Nothing to transfer.");
        return;
    }
    let mut head = format!("{} transfer(s)", list.total_count);
    if list.upload_rate_bps > 0 {
        head.push_str(&format!(
            " · uploading at {}/s",
            human_bytes(list.upload_rate_bps)
        ));
    }
    if let Some(eta) = list.upload_eta_secs {
        head.push_str(&format!(" · {} left", human_duration(eta)));
    }
    println!("{head}");
    if list.auto_upload_checking {
        println!("Uploads paused for a few seconds: checking whether they slow your connection.");
    } else if let Some(l) = list.auto_upload_limit_bps {
        println!(
            "Uploads slowed to {}/s so your connection stays responsive (auto-upload-speed).",
            human_bytes(l)
        );
    }
    let shown = if all {
        list.items.len()
    } else {
        list.items.len().min(20)
    };
    for i in &list.items[..shown] {
        let state = match i.state {
            TransferState::Preparing => "preparing",
            TransferState::Active => "active",
            TransferState::Verifying => "verifying",
            TransferState::Queued => "queued",
            TransferState::Waiting => "waiting",
            TransferState::Paused => "paused",
            TransferState::Scheduled => "tonight",
        };
        let pct = if i.bytes_total > 0 {
            i.bytes_done * 100 / i.bytes_total
        } else {
            0
        };
        let mut line = format!(
            "{:>7}  {:<9} {:>3}%  {} of {}",
            i.id,
            state,
            pct,
            human_bytes(i.bytes_done),
            human_bytes(i.bytes_total)
        );
        if let Some(eta) = i.eta_secs {
            line.push_str(&format!(", {}", human_duration(eta)));
        }
        println!("{line}  {}", i.path);
        if let Some(r) = &i.reason {
            println!("         {r}");
        }
    }
    if shown < list.items.len() {
        println!(
            "…and {} more (drive transfers --all)",
            list.items.len() - shown
        );
    }
}

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

/// Reads a secret from the terminal without echo, or one line from stdin when there is no
/// terminal (scripts, headless servers: `echo "$PW" | drive setup ...`).
fn prompt_secret(label: &str) -> Result<String> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return rpassword::prompt_password(label).context("reading password");
    }
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading password from stdin")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

async fn run_client(cmd: Command) -> Result<()> {
    let client = connect().await?;
    match cmd {
        Command::Daemon { .. } => unreachable!(),
        Command::Status => {
            let s: Status = client.call(Call::Status).await?;
            println!("{}", s.headline);
            if let Some(a) = &s.account {
                println!("  Account:     {a}");
            }
            if let Some(f) = &s.folder {
                println!("  Folder:      {f}");
            }
            println!(
                "  In the cloud: {} files, {}",
                s.cloud_files,
                human_bytes(s.cloud_bytes)
            );
            println!(
                "  On this device: {} kept offline, {} of {} cache used",
                human_bytes(s.pinned_bytes),
                human_bytes(s.cache_bytes),
                human_bytes(s.cache_limit_bytes)
            );
            if s.pending > 0 || s.uploading + s.downloading > 0 {
                println!(
                    "  Pending: {} changes, {} uploading, {} downloading",
                    s.pending, s.uploading, s.downloading
                );
            }
            if s.issues > 0 {
                println!("  {} issue(s) — run `drive issues`", s.issues);
            }
        }
        Command::Setup {
            login,
            folder,
            existing_vault,
        } => {
            let password = prompt_secret("uloz.to password: ")?;
            let encryption_password = if existing_vault {
                Some(prompt_secret("Vault encryption password: ")?)
            } else {
                None
            };
            let req = SetupRequest {
                login,
                password,
                encryption_password,
                folder,
                app_token: None,
            };
            match client.call::<SetupResult>(Call::Setup(req)).await {
                Ok(r) => {
                    println!("Connected. Your Drive folder is {}", r.folder);
                    if let Some(p) = r.generated_encryption_password {
                        println!();
                        println!("Your encryption password is:\n\n    {p}\n");
                        println!(
                            "Write it down and keep it safe. You need it to set up Drive on another device,"
                        );
                        println!(
                            "and nobody — not even uloz.to — can recover your files without it."
                        );
                        println!(
                            "Run `drive recovery` for an rclone config that opens your vault without Drive."
                        );
                    }
                }
                Err(ClientError::Rpc(e))
                    if e.code == ErrorCode::WrongEncryptionPassword && !existing_vault =>
                {
                    bail!("{} Run the command again with --existing-vault.", e.message)
                }
                Err(e) => return Err(e.into()),
            }
        }
        Command::Pause => {
            client.call_value(Call::Pause).await?;
            println!("Syncing paused.");
        }
        Command::Resume => {
            client.call_value(Call::Resume).await?;
            println!("Syncing resumed.");
        }
        Command::Sync => {
            client.call_value(Call::SyncNow).await?;
            println!("Checking for changes…");
        }
        Command::Pin { path } => {
            let path = absolute(&path);
            client
                .call_value(Call::SetPinned { path, pinned: true })
                .await?;
            println!("Kept on this device.");
        }
        Command::Unpin { path } => {
            let path = absolute(&path);
            client
                .call_value(Call::SetPinned {
                    path,
                    pinned: false,
                })
                .await?;
            println!("No longer kept on this device.");
        }
        Command::Free { path } => {
            let path = absolute(&path);
            let v = client.call_value(Call::FreeUpSpace { path }).await?;
            println!(
                "Freed {}.",
                human_bytes(v.get("freed_bytes").and_then(|b| b.as_u64()).unwrap_or(0))
            );
        }
        Command::Issues => {
            let issues: Vec<Issue> = client.call(Call::ListIssues).await?;
            if issues.is_empty() {
                println!("Nothing needs attention.");
            }
            for i in issues {
                let actions: Vec<String> = i
                    .actions
                    .iter()
                    .map(|a| format!("{a:?}").to_lowercase())
                    .collect();
                println!(
                    "#{} {}\n    actions: {}",
                    i.id,
                    i.message,
                    actions.join(", ")
                );
            }
        }
        Command::Resolve { id, action } => {
            let action = match action.replace('_', "-").as_str() {
                "retry" => IssueAction::Retry,
                "dismiss" => IssueAction::Dismiss,
                "confirm-delete" | "confirmdelete" => IssueAction::ConfirmDelete,
                "restore-deleted" | "restoredeleted" => IssueAction::RestoreDeleted,
                other => bail!("unknown action {other}"),
            };
            client.call_value(Call::ResolveIssue { id, action }).await?;
            println!("Done.");
        }
        Command::Activity { limit } => {
            let items: Vec<Activity> = client.call(Call::RecentActivity { limit }).await?;
            for a in items {
                let when = chrono::DateTime::from_timestamp_millis(a.at_ms)
                    .map(|t| {
                        t.with_timezone(&chrono::Local)
                            .format("%Y-%m-%d %H:%M")
                            .to_string()
                    })
                    .unwrap_or_default();
                println!("{when}  {:<14} {}", format!("{:?}", a.kind), a.path);
            }
        }
        Command::Settings { changes } => {
            let mut patch = SettingsPatch::default();
            for c in &changes {
                let (k, v) = c
                    .split_once('=')
                    .with_context(|| format!("expected key=value, got {c}"))?;
                let num = || {
                    v.parse::<u64>()
                        .with_context(|| format!("{k} needs a number"))
                };
                match k {
                    "cache-limit-gb" => patch.cache_limit_bytes = Some(num()? * 1024 * 1024 * 1024),
                    "poll-interval" => patch.poll_interval_secs = Some(num()?),
                    "upload-limit-kbps" => patch.upload_limit_kbps = Some(num()?),
                    "download-limit-kbps" => patch.download_limit_kbps = Some(num()?),
                    "parallel" => patch.max_parallel_transfers = Some(num()? as u32),
                    "device-name" => patch.device_name = Some(v.to_string()),
                    "api-key" => patch.app_token = Some(v.to_string()),
                    "auto-upload-speed" => {
                        patch.adaptive_upload = Some(match v {
                            "on" | "true" | "yes" | "1" => true,
                            "off" | "false" | "no" | "0" => false,
                            _ => bail!("auto-upload-speed needs on or off"),
                        })
                    }
                    "auto-update" => {
                        patch.auto_update = Some(match v {
                            "on" | "true" | "yes" | "1" => true,
                            "off" | "false" | "no" | "0" => false,
                            _ => bail!("auto-update needs on or off"),
                        })
                    }
                    "night-uploads" => {
                        patch.night_uploads = Some(match v {
                            "on" | "true" | "yes" | "1" => true,
                            "off" | "false" | "no" | "0" => false,
                            _ => bail!("night-uploads needs on or off"),
                        })
                    }
                    "night-start" => patch.night_start = Some(v.to_string()),
                    "night-end" => patch.night_end = Some(v.to_string()),
                    "night-min-mb" => patch.night_min_bytes = Some(num()? * 1024 * 1024),
                    other => bail!("unknown setting {other}"),
                }
            }
            let s: Settings = if changes.is_empty() {
                client.call(Call::GetSettings).await?
            } else {
                client.call(Call::UpdateSettings(patch)).await?
            };
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
        Command::Recovery => {
            let kit: RecoveryKit = client.call(Call::ExportRecovery).await?;
            println!("{}", kit.rclone_config);
            eprintln!(
                "Encryption password: {}\nSalt: {}\n\n{}",
                kit.encryption_password, kit.encryption_salt, kit.instructions
            );
        }
        Command::SignOut { yes } => {
            if yes {
                client.call_value(Call::SignOut).await?;
                println!("Signed out.");
                return Ok(());
            }
            print!(
                "Disconnect this device from uloz.to? Files that only exist in the cloud disappear from this device (they stay in the cloud). [y/N] "
            );
            std::io::stdout().flush()?;
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer)?;
            if answer.trim().eq_ignore_ascii_case("y") {
                client.call_value(Call::SignOut).await?;
                println!("Signed out.");
            }
        }
        Command::Stop => {
            client.call_value(Call::Shutdown).await?;
            println!("Stopping the Drive service.");
        }
        Command::Transfers { all } => {
            let list: TransferList = client.call(Call::Transfers).await?;
            print_transfers(&list, all);
        }
        Command::Transfer { action, target } => {
            let mode = match action.as_str() {
                "pause" => TransferMode::Paused,
                "resume" | "auto" => TransferMode::Auto,
                "tonight" => TransferMode::Tonight,
                "now" => TransferMode::Now,
                other => bail!("unknown action {other}: use pause, resume, tonight or now"),
            };
            let list: TransferList = client.call(Call::Transfers).await?;
            let rel = target.replace('\\', "/");
            let id = match target.parse::<i64>() {
                Ok(id) => id,
                Err(_) => list
                    .items
                    .iter()
                    .find(|i| rel.ends_with(&i.path) || i.path == rel)
                    .map(|i| i.id)
                    .with_context(|| format!("{target} isn't waiting to upload"))?,
            };
            client
                .call_value(Call::SetTransferMode { id, mode })
                .await?;
            println!("Done.");
        }
    }
    Ok(())
}

fn absolute(path: &str) -> String {
    let p = std::path::Path::new(path);
    if p.is_absolute() {
        path.to_string()
    } else {
        std::env::current_dir()
            .map(|d| d.join(p).display().to_string())
            .unwrap_or_else(|_| path.to_string())
    }
}

#[allow(dead_code)]
fn log_error(e: &anyhow::Error) {
    error!("{e:#}");
}
