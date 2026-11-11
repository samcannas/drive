//! The background service: owns the configuration, the account connection, the engine and
//! the platform integration, and answers IPC calls.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use drive_core::crypt::{Cipher, obscure};
use drive_core::db::Db;
use drive_core::engine::plan::{VAULT_MARKER, VAULT_MARKER_CONTENT};
use drive_core::engine::{Engine, EngineConfig};
use drive_core::ipc::{
    AccountCheck, AccountCredentials, Call, ErrorCode, Event, Hello, PROTOCOL_VERSION, RecoveryKit,
    RpcError, SettingsPatch, SetupRequest, SetupResult, Status, SyncState,
};
use drive_core::remote::crypt::CryptRemote;
use drive_core::remote::mem::{MemRemote, ROOT as MEM_ROOT};
use drive_core::remote::{Remote, RemoteError, UploadBody};
use drive_core::ulozto::{DEFAULT_APP_TOKEN, UlozClient, UlozConfig, UlozRemote};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, broadcast};
use tracing::{error, info, warn};

use crate::config::{Account, Config, Paths, Secrets};
use crate::platform::{self, Platform, PlatformContext};

/// Name of the vault folder in the uloz.to account root.
pub const VAULT_FOLDER: &str = "Drive vault";

struct Running {
    engine: Arc<Engine>,
    platform: Box<dyn Platform>,
    relay: tokio::task::JoinHandle<()>,
}

struct Inner {
    config: Config,
    running: Option<Running>,
    /// Why the engine couldn't start, if it couldn't.
    start_error: Option<String>,
}

pub struct Service {
    paths: Paths,
    inner: Mutex<Inner>,
    events: broadcast::Sender<Event>,
    pub shutdown: Notify,
}

fn rpc(code: ErrorCode, msg: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: msg.into(),
    }
}

fn remote_rpc(e: RemoteError) -> RpcError {
    match e {
        RemoteError::Auth(m) => rpc(
            ErrorCode::AuthFailed,
            format!("uloz.to didn't accept that login: {m}"),
        ),
        RemoteError::Network(m) => rpc(ErrorCode::Network, format!("Couldn't reach uloz.to: {m}")),
        RemoteError::RateLimited => rpc(
            ErrorCode::Network,
            "uloz.to is busy. Try again in a minute.",
        ),
        other => rpc(ErrorCode::Internal, other.to_string()),
    }
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    rpc(ErrorCode::Internal, e.to_string())
}

/// A strong, readable random password: 6 groups of 4 characters (≈120 bits).
pub fn generate_password() -> String {
    const ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789"; // no 0/o, 1/l/i
    let mut rng = rand::thread_rng();
    use rand::Rng;
    (0..6)
        .map(|_| {
            (0..4)
                .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// Development backend: `DRIVE_DEV_REMOTE=<dir>` replaces uloz.to with a local directory.
pub fn dev_remote_dir() -> Option<PathBuf> {
    std::env::var_os("DRIVE_DEV_REMOTE").map(PathBuf::from)
}

fn dev_remote(dir: &Path) -> Result<Arc<MemRemote>, RpcError> {
    MemRemote::persistent(dir).map(Arc::new).map_err(internal)
}

pub fn salt_for(login: &str) -> String {
    format!("drive-vault:{}", login.to_lowercase())
}

impl Service {
    pub fn new(paths: Paths) -> Result<Arc<Self>> {
        paths.ensure()?;
        let config = Config::load(&paths)?;
        let (events, _) = broadcast::channel(512);
        Ok(Arc::new(Self {
            paths,
            inner: Mutex::new(Inner {
                config,
                running: None,
                start_error: None,
            }),
            events,
            shutdown: Notify::new(),
        }))
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Starts syncing if an account is configured.
    pub async fn start(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        if inner.config.account.is_none() {
            info!("no account connected yet");
            return;
        }
        match Secrets::load(&self.paths) {
            Ok(Some(secrets)) => {
                if let Err(e) = self.start_running(&mut inner, &secrets) {
                    error!("starting sync failed: {e:#}");
                    inner.start_error = Some(format!("{e:#}"));
                }
            }
            Ok(None) => {
                warn!("account configured but credentials are missing");
                inner.start_error = Some("Credentials are missing. Sign in again.".into());
            }
            Err(e) => {
                error!("reading credentials failed: {e:#}");
                inner.start_error = Some("Saved credentials can't be read. Sign in again.".into());
            }
        }
        drop(inner);
        self.emit_status().await;
    }

    pub async fn stop(&self) {
        let mut inner = self.inner.lock().await;
        if let Some(r) = inner.running.take() {
            stop_running(r);
        }
    }

    fn client_for(
        &self,
        cfg: &Config,
        login: &str,
        password: &str,
        app_token: Option<&str>,
    ) -> Result<Arc<UlozClient>, RpcError> {
        let mut uc = UlozConfig::new(login, password, &cfg.device_id);
        if let Some(t) = app_token.filter(|t| !t.is_empty()) {
            uc.app_token = t.to_string();
        }
        UlozClient::new(uc).map(Arc::new).map_err(remote_rpc)
    }

    fn engine_config(&self, cfg: &Config, account: &Account) -> EngineConfig {
        EngineConfig {
            device_name: cfg.device_name.clone(),
            account: account.login.clone(),
            folder: cfg.folder_or_default(),
            cache_limit_bytes: cfg.cache_limit_bytes,
            poll_interval: Duration::from_secs(cfg.poll_interval_secs.max(30)),
            max_parallel_transfers: cfg.max_parallel_transfers.clamp(1, 16) as usize,
            upload_limit_kbps: cfg.upload_limit_kbps,
            download_limit_kbps: cfg.download_limit_kbps,
            ..Default::default()
        }
    }

    fn start_running(self: &Arc<Self>, inner: &mut Inner, secrets: &Secrets) -> Result<()> {
        let cfg = inner.config.clone();
        let account = cfg.account.clone().context("no account")?;
        let cipher = Cipher::new(&secrets.encryption_password, &account.salt)
            .context("deriving encryption keys")?;
        let remote: Arc<dyn Remote> = if let Some(dir) = dev_remote_dir() {
            warn!(
                "development mode: using {} instead of uloz.to",
                dir.display()
            );
            Arc::new(CryptRemote::new(
                dev_remote(&dir).map_err(|e| anyhow::anyhow!(e.message))?,
                cipher,
            ))
        } else {
            let client = self
                .client_for(
                    &cfg,
                    &account.login,
                    &secrets.uloz_password,
                    cfg.app_token.as_deref(),
                )
                .map_err(|e| anyhow::anyhow!(e.message))?;
            Arc::new(CryptRemote::new(
                Arc::new(UlozRemote::new(client, account.vault_slug.clone())),
                cipher,
            ))
        };

        // A database from another vault would be meaningless (and dangerous): start fresh.
        let db_path = self.paths.db_file();
        if db_path.exists()
            && let Ok(db) = Db::open(&db_path)
            && db
                .kv_get("vault_slug")
                .ok()
                .flatten()
                .is_some_and(|v| v != account.vault_slug)
        {
            drop(db);
            warn!("state database belongs to another vault; starting over");
            remove_db(&db_path);
        }
        let db = Arc::new(Db::open(&db_path).context("opening the state database")?);
        db.kv_set("vault_slug", &account.vault_slug)?;

        let folder = cfg.folder_or_default();
        // A FUSE mount left behind by a crash fails with "File exists"; the platform layer
        // cleans it up.
        if let Err(e) = std::fs::create_dir_all(&folder)
            && e.kind() != std::io::ErrorKind::AlreadyExists
        {
            return Err(e).with_context(|| format!("creating {}", folder.display()));
        }
        let engine = Engine::new(db, remote, self.engine_config(&cfg, &account))?;
        let platform = platform::prepare(PlatformContext {
            engine: engine.clone(),
            folder: folder.clone(),
            cache_dir: self.paths.cache_dir.join("content"),
            display_name: "Drive".into(),
            account: account.login.clone(),
        })
        .context("setting up the Drive folder")?;
        engine.set_local(platform.local_side());
        engine.start();
        if let Err(e) = platform.start() {
            engine.shutdown();
            return Err(e).context("connecting the Drive folder");
        }

        // Forward engine events to IPC subscribers.
        let mut rx = engine.subscribe();
        let tx = self.events.clone();
        let relay = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let _ = tx.send(ev);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        info!(
            "syncing {} with uloz.to account {}",
            folder.display(),
            account.login
        );
        inner.running = Some(Running {
            engine,
            platform,
            relay,
        });
        inner.start_error = None;
        Ok(())
    }

    async fn status(&self) -> Status {
        let inner = self.inner.lock().await;
        if let Some(r) = &inner.running {
            return r.engine.status();
        }
        let account = inner.config.account.as_ref().map(|a| a.login.clone());
        match (&account, &inner.start_error) {
            (Some(_), Some(err)) => Status {
                state: SyncState::Error,
                headline: err.clone(),
                account,
                folder: Some(inner.config.folder_or_default().display().to_string()),
                ..Default::default()
            },
            (Some(_), None) => Status {
                state: SyncState::SignedOut,
                headline: "Sign in to keep syncing".into(),
                account,
                ..Default::default()
            },
            (None, _) => Status {
                state: SyncState::NotSetUp,
                headline: "Set up Drive to get started".into(),
                ..Default::default()
            },
        }
    }

    async fn emit_status(&self) {
        let st = self.status().await;
        let _ = self.events.send(Event::Status(st));
    }

    async fn engine(&self) -> Result<Arc<Engine>, RpcError> {
        self.inner
            .lock()
            .await
            .running
            .as_ref()
            .map(|r| r.engine.clone())
            .ok_or_else(|| {
                rpc(
                    ErrorCode::NotSetUp,
                    "Drive isn't connected to an account yet",
                )
            })
    }

    /// Converts a path from the UI (absolute, or relative to the Drive folder) to a vault path.
    async fn vault_path(&self, path: &str) -> Result<String, RpcError> {
        let folder = self.inner.lock().await.config.folder_or_default();
        relative_vault_path(&folder, path).ok_or_else(|| {
            rpc(
                ErrorCode::BadRequest,
                format!("{path} is not inside the Drive folder"),
            )
        })
    }

    pub async fn handle(self: &Arc<Self>, call: Call) -> Result<Value, RpcError> {
        let ok = |v: Value| Ok(v);
        match call {
            Call::Hello => ok(serde_json::to_value(Hello {
                protocol: PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").into(),
                platform: std::env::consts::OS.into(),
            })
            .map_err(internal)?),
            Call::Status => ok(serde_json::to_value(self.status().await).map_err(internal)?),
            Call::Subscribe => ok(Value::Null),
            Call::CheckAccount(creds) => self
                .check_account(creds)
                .await
                .map(|c| serde_json::to_value(c).unwrap_or_default()),
            Call::Setup(req) => self
                .setup(req)
                .await
                .map(|r| serde_json::to_value(r).unwrap_or_default()),
            Call::Pause => {
                self.engine().await?.pause();
                ok(Value::Null)
            }
            Call::Resume => {
                self.engine().await?.resume();
                ok(Value::Null)
            }
            Call::SyncNow => {
                self.engine().await?.sync_now();
                ok(Value::Null)
            }
            Call::GetSettings => ok(serde_json::to_value(
                self.inner.lock().await.config.settings(),
            )
            .map_err(internal)?),
            Call::UpdateSettings(patch) => self.update_settings(patch).await,
            Call::RecentActivity { limit } => {
                let e = self.engine().await?;
                ok(serde_json::to_value(
                    e.db()
                        .recent_activity(limit.clamp(1, 200), None)
                        .map_err(internal)?,
                )
                .map_err(internal)?)
            }
            Call::ListIssues => match self.engine().await {
                Ok(e) => ok(serde_json::to_value(e.issues().map_err(internal)?).map_err(internal)?),
                Err(_) => ok(Value::Array(vec![])),
            },
            Call::ResolveIssue { id, action } => {
                self.engine()
                    .await?
                    .resolve_issue(id, action)
                    .map_err(|e| rpc(ErrorCode::NotFound, e.to_string()))?;
                ok(Value::Null)
            }
            Call::SetPinned { path, pinned } => {
                let p = self.vault_path(&path).await?;
                self.engine()
                    .await?
                    .set_pinned(&p, pinned)
                    .map_err(|e| rpc(ErrorCode::NotFound, e.to_string()))?;
                ok(Value::Null)
            }
            Call::FreeUpSpace { path } => {
                let p = self.vault_path(&path).await?;
                let freed = self
                    .engine()
                    .await?
                    .free_up_space(&p)
                    .await
                    .map_err(|e| rpc(ErrorCode::NotFound, e.to_string()))?;
                ok(serde_json::json!({ "freed_bytes": freed }))
            }
            Call::ExportRecovery => self
                .recovery_kit()
                .await
                .map(|k| serde_json::to_value(k).unwrap_or_default()),
            Call::SignOut => {
                self.sign_out().await.map_err(internal)?;
                ok(Value::Null)
            }
            Call::Shutdown => {
                self.shutdown.notify_waiters();
                self.shutdown.notify_one();
                ok(Value::Null)
            }
        }
    }

    async fn check_account(&self, creds: AccountCredentials) -> Result<AccountCheck, RpcError> {
        if let Some(dir) = dev_remote_dir() {
            let mem = dev_remote(&dir)?;
            let has = !mem.scan().await.map_err(remote_rpc)?.entries.is_empty();
            return Ok(AccountCheck {
                login: creds.login,
                vault_exists: has,
                vault_has_files: has,
            });
        }
        let cfg = self.inner.lock().await.config.clone();
        let client = self.client_for(
            &cfg,
            &creds.login,
            &creds.password,
            cfg.app_token.as_deref(),
        )?;
        let session = client.login().await.map_err(remote_rpc)?;
        let vault = UlozRemote::find_or_create_vault(&client, VAULT_FOLDER, false)
            .await
            .map_err(remote_rpc)?;
        let vault_has_files = match &vault {
            Some(slug) => vault_has_content(&client, slug).await.map_err(remote_rpc)?,
            None => false,
        };
        Ok(AccountCheck {
            login: session.login,
            vault_exists: vault.is_some(),
            vault_has_files,
        })
    }

    async fn setup(self: &Arc<Self>, req: SetupRequest) -> Result<SetupResult, RpcError> {
        let mut inner = self.inner.lock().await;
        let reauth = inner
            .config
            .account
            .as_ref()
            .is_some_and(|a| a.login.eq_ignore_ascii_case(&req.login));
        if inner.running.is_some() && !reauth {
            return Err(rpc(
                ErrorCode::AlreadySetUp,
                "Drive is already connected. Sign out first to use another account.",
            ));
        }
        let cfg = inner.config.clone();
        let app_token = req
            .app_token
            .clone()
            .filter(|t| !t.trim().is_empty())
            .or(cfg.app_token.clone());
        let stored = Secrets::load(&self.paths).ok().flatten();
        let dev = dev_remote_dir();

        // Sign in and find the vault.
        let (login, vault, client, mem) = match &dev {
            Some(dir) => {
                let mem = dev_remote(dir)?;
                (
                    req.login.clone(),
                    Some(MEM_ROOT.to_string()),
                    None,
                    Some(mem),
                )
            }
            None => {
                let client =
                    self.client_for(&cfg, &req.login, &req.password, app_token.as_deref())?;
                let session = client.login().await.map_err(remote_rpc)?;
                let vault = UlozRemote::find_or_create_vault(&client, VAULT_FOLDER, false)
                    .await
                    .map_err(remote_rpc)?;
                (session.login.clone(), vault, Some(client), None)
            }
        };
        let salt = cfg
            .account
            .as_ref()
            .filter(|_| reauth)
            .map(|a| a.salt.clone())
            .unwrap_or_else(|| salt_for(&login));
        let backend = Backend {
            client: client.clone(),
            mem: mem.clone(),
        };

        let mut generated = None;
        let (vault_slug, password, created) = match vault {
            Some(slug) => {
                let names = backend.top_level_names(&slug).await.map_err(remote_rpc)?;
                let has_content = !names.is_empty();
                let password = match (
                    req.encryption_password.clone().filter(|p| !p.is_empty()),
                    reauth,
                    &stored,
                ) {
                    (Some(p), _, _) => p,
                    (None, true, Some(s)) => s.encryption_password.clone(),
                    (None, _, _) if !has_content => {
                        let p = generate_password();
                        generated = Some(p.clone());
                        p
                    }
                    (None, _, _) => {
                        return Err(rpc(
                            ErrorCode::WrongEncryptionPassword,
                            "This account already has a Drive vault. Enter its encryption password.",
                        ));
                    }
                };
                let cipher = Cipher::new(&password, &salt).map_err(internal)?;
                if has_content && !names.iter().any(|n| cipher.decrypt_name(n).is_ok()) {
                    return Err(rpc(
                        ErrorCode::WrongEncryptionPassword,
                        "That encryption password doesn't open the Drive vault in this account.",
                    ));
                }
                if !has_content {
                    backend
                        .write_marker(&slug, cipher)
                        .await
                        .map_err(remote_rpc)?;
                }
                (slug, password, false)
            }
            None => {
                let client = client.clone().ok_or_else(|| internal("no client"))?;
                let slug = UlozRemote::find_or_create_vault(&client, VAULT_FOLDER, true)
                    .await
                    .map_err(remote_rpc)?
                    .ok_or_else(|| internal("creating the vault folder failed"))?;
                let password = match req.encryption_password.clone().filter(|p| !p.is_empty()) {
                    Some(p) => p,
                    None => {
                        let p = generate_password();
                        generated = Some(p.clone());
                        p
                    }
                };
                backend
                    .write_marker(&slug, Cipher::new(&password, &salt).map_err(internal)?)
                    .await
                    .map_err(remote_rpc)?;
                (slug, password, true)
            }
        };

        // Stop the current session (re-authentication) before switching credentials.
        if let Some(r) = inner.running.take() {
            stop_running(r);
        }
        let folder = match req.folder.clone().filter(|f| !f.trim().is_empty()) {
            Some(f) => PathBuf::from(f),
            None => cfg.folder_or_default(),
        };
        inner.config.account = Some(Account {
            login: login.clone(),
            vault_slug,
            salt,
        });
        inner.config.folder = Some(folder.clone());
        inner.config.app_token = app_token.filter(|t| t != DEFAULT_APP_TOKEN);
        inner.config.save(&self.paths).map_err(internal)?;
        let secrets = Secrets {
            uloz_password: req.password.clone(),
            encryption_password: password,
        };
        secrets.save(&self.paths).map_err(internal)?;
        if let Err(e) = self.start_running(&mut inner, &secrets) {
            inner.start_error = Some(format!("{e:#}"));
            drop(inner);
            self.emit_status().await;
            return Err(internal(format!(
                "Connected, but the Drive folder couldn't be set up: {e:#}"
            )));
        }
        drop(inner);
        self.emit_status().await;
        info!(
            "account {login} connected (vault {})",
            if created { "created" } else { "opened" }
        );
        Ok(SetupResult {
            folder: folder.display().to_string(),
            generated_encryption_password: generated,
            created_vault: created,
        })
    }

    async fn update_settings(self: &Arc<Self>, patch: SettingsPatch) -> Result<Value, RpcError> {
        let mut inner = self.inner.lock().await;
        let c = &mut inner.config;
        if let Some(v) = patch.cache_limit_bytes {
            c.cache_limit_bytes = v.max(256 * 1024 * 1024);
        }
        if let Some(v) = patch.poll_interval_secs {
            c.poll_interval_secs = v.clamp(30, 24 * 3600);
        }
        if let Some(v) = patch.upload_limit_kbps {
            c.upload_limit_kbps = v;
        }
        if let Some(v) = patch.download_limit_kbps {
            c.download_limit_kbps = v;
        }
        if let Some(v) = patch.max_parallel_transfers {
            c.max_parallel_transfers = v.clamp(1, 16);
        }
        if let Some(v) = patch.launch_at_login {
            c.launch_at_login = v;
        }
        if let Some(v) = patch.close_action {
            c.close_action = v;
        }
        if let Some(v) = patch.notifications {
            c.notifications = v;
        }
        if let Some(v) = patch.device_name.filter(|v| !v.trim().is_empty()) {
            c.device_name = v.trim().to_string();
        }
        let token_changed = match patch.app_token {
            Some(t) => {
                let new =
                    Some(t.trim().to_string()).filter(|t| !t.is_empty() && t != DEFAULT_APP_TOKEN);
                let changed = new != c.app_token;
                c.app_token = new;
                changed
            }
            None => false,
        };
        c.save(&self.paths).map_err(internal)?;
        let cfg = inner.config.clone();
        if token_changed && inner.running.is_some() {
            // The API key is baked into the client: reconnect.
            if let Some(r) = inner.running.take() {
                stop_running(r);
            }
            if let Ok(Some(secrets)) = Secrets::load(&self.paths)
                && let Err(e) = self.start_running(&mut inner, &secrets)
            {
                inner.start_error = Some(format!("{e:#}"));
            }
        } else if let (Some(r), Some(account)) = (&inner.running, &cfg.account) {
            let ec = self.engine_config(&cfg, account);
            r.engine.update_config(|e| *e = ec);
        }
        serde_json::to_value(cfg.settings()).map_err(internal)
    }

    async fn recovery_kit(&self) -> Result<RecoveryKit, RpcError> {
        let inner = self.inner.lock().await;
        let account = inner
            .config
            .account
            .clone()
            .ok_or_else(|| rpc(ErrorCode::NotSetUp, "No account connected"))?;
        let secrets = Secrets::load(&self.paths)
            .map_err(internal)?
            .ok_or_else(|| rpc(ErrorCode::NotSetUp, "No saved credentials"))?;
        Ok(recovery_kit(&account, &secrets.encryption_password))
    }

    async fn sign_out(&self) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let folder = inner.config.folder_or_default();
        if let Some(r) = inner.running.take() {
            stop_running(r);
        }
        if let Err(e) = platform::teardown(&folder, &self.paths.cache_dir.join("content")) {
            warn!("cleaning up the Drive folder failed: {e:#}");
        }
        remove_db(&self.paths.db_file());
        Secrets::delete(&self.paths);
        inner.config.account = None;
        inner.start_error = None;
        inner.config.save(&self.paths)?;
        drop(inner);
        self.emit_status().await;
        info!("signed out");
        Ok(())
    }
}

fn stop_running(r: Running) {
    r.platform.stop();
    r.engine.shutdown();
    r.relay.abort();
}

fn remove_db(path: &Path) {
    for p in [
        path.to_path_buf(),
        path.with_extension("db-wal"),
        path.with_extension("db-shm"),
    ] {
        let _ = std::fs::remove_file(p);
    }
}

pub fn relative_vault_path(folder: &Path, path: &str) -> Option<String> {
    let p = Path::new(path);
    let rel = if p.is_absolute() {
        let folder_s = folder.to_string_lossy().replace('\\', "/").to_lowercase();
        let path_s = path.replace('\\', "/");
        if !path_s.to_lowercase().starts_with(&folder_s) {
            return None;
        }
        path_s[folder_s.len()..].to_string()
    } else {
        path.replace('\\', "/")
    };
    let parts: Vec<&str> = rel
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    if parts.contains(&"..") {
        return None;
    }
    Some(parts.join("/"))
}

/// The account backend during setup: uloz.to, or the development directory.
struct Backend {
    client: Option<Arc<UlozClient>>,
    mem: Option<Arc<MemRemote>>,
}

impl Backend {
    /// Raw (encrypted) names directly inside the vault.
    async fn top_level_names(&self, slug: &str) -> Result<Vec<String>, RemoteError> {
        if let Some(mem) = &self.mem {
            let tree = mem.scan().await?;
            return Ok(tree
                .entries
                .into_iter()
                .filter(|e| e.parent_id == slug)
                .map(|e| e.name)
                .collect());
        }
        let client = self.client.as_ref().expect("client or mem");
        let mut names: Vec<String> = client
            .list_files(slug, false)
            .await?
            .into_iter()
            .map(|f| f.name)
            .collect();
        names.extend(
            client
                .list_folders(slug, false)
                .await?
                .into_iter()
                .map(|f| f.name),
        );
        Ok(names)
    }

    async fn write_marker(&self, slug: &str, cipher: Cipher) -> Result<(), RemoteError> {
        let body = UploadBody::from_bytes(VAULT_MARKER_CONTENT.to_vec(), drive_core::db::now_ms());
        if let Some(mem) = &self.mem {
            CryptRemote::new(mem.clone(), cipher)
                .upload(slug, VAULT_MARKER, body)
                .await?;
            return Ok(());
        }
        let client = self.client.clone().expect("client or mem");
        let raw = Arc::new(UlozRemote::new(client, slug.to_string()));
        CryptRemote::new(raw, cipher)
            .upload(slug, VAULT_MARKER, body)
            .await?;
        Ok(())
    }
}

async fn vault_has_content(client: &UlozClient, slug: &str) -> Result<bool, RemoteError> {
    Ok(!client.list_folders(slug, false).await?.is_empty()
        || !client.list_files(slug, false).await?.is_empty())
}

pub fn recovery_kit(account: &Account, encryption_password: &str) -> RecoveryKit {
    let rclone_config = format!(
        "# Drive recovery kit — opens your encrypted uloz.to vault with plain rclone (https://rclone.org).\n\
         # Keep this file private: with it, anyone who can sign in to the uloz.to account can read your files.\n\
         \n\
         [uloz]\n\
         type = ulozto\n\
         username = {login}\n\
         # Set your uloz.to password with:  rclone config update uloz password YOUR_PASSWORD --obscure\n\
         \n\
         [drive]\n\
         type = crypt\n\
         remote = uloz:{vault}\n\
         filename_encryption = standard\n\
         directory_name_encryption = true\n\
         password = {pw}\n\
         password2 = {salt}\n",
        login = account.login,
        vault = VAULT_FOLDER,
        pw = obscure::obscure(encryption_password),
        salt = obscure::obscure(&account.salt),
    );
    RecoveryKit {
        rclone_config,
        encryption_password: encryption_password.to_string(),
        encryption_salt: account.salt.clone(),
        instructions: "To get your files without Drive: install rclone, save the configuration above as rclone.conf, \
            set your uloz.to password as shown in the file, then run `rclone --config rclone.conf copy drive: ./restored`. \
            To browse instead, run `rclone --config rclone.conf lsf -R drive:`."
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vault_paths() {
        let folder = Path::new(if cfg!(windows) {
            r"C:\Users\me\Drive"
        } else {
            "/home/me/Drive"
        });
        let abs = if cfg!(windows) {
            r"c:\users\me\Drive\Docs\a.txt"
        } else {
            "/home/me/Drive/Docs/a.txt"
        };
        assert_eq!(
            relative_vault_path(folder, abs).as_deref(),
            Some("Docs/a.txt")
        );
        assert_eq!(
            relative_vault_path(folder, "Docs\\a.txt").as_deref(),
            Some("Docs/a.txt")
        );
        assert_eq!(relative_vault_path(folder, "../etc").as_deref(), None);
        let outside = if cfg!(windows) { r"C:\Windows" } else { "/etc" };
        assert_eq!(relative_vault_path(folder, outside), None);
    }

    #[test]
    fn generated_passwords() {
        let p = generate_password();
        assert_eq!(p.len(), 29);
        assert_ne!(p, generate_password());
    }

    #[test]
    fn recovery_config_reveals_to_same_secrets() {
        let acct = Account {
            login: "me".into(),
            vault_slug: "x".into(),
            salt: salt_for("Me"),
        };
        let kit = recovery_kit(&acct, "pw-123");
        let line = kit
            .rclone_config
            .lines()
            .find(|l| l.starts_with("password = "))
            .unwrap();
        assert_eq!(
            obscure::reveal(line.trim_start_matches("password = ")).unwrap(),
            "pw-123"
        );
        assert_eq!(kit.encryption_salt, "drive-vault:me");
    }
}
