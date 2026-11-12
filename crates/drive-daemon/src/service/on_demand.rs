//! On-demand backed-up folders ("Save space"): the folder stays where it is, but becomes a
//! cloud folder like the Drive folder. Files not used lately live only in the cloud and download
//! again when opened.
//!
//! Each such folder gets its own two-way engine (linked to the main one, so they share the
//! bandwidth) and its own folder integration (on Windows, a Cloud Files sync root). Its cloud
//! copy is the folder's backup in `Backups/<device>/<name>`, so switching on needs no upload:
//! files already backed up are recognised, and only the ones not used lately are freed.
//!
//! Switching off downloads everything back first and only then turns the folder into an
//! ordinary one again, so no file ever exists only in the cloud afterwards.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use drive_core::db::{Db, ROOT_ID, name_key, now_ms};
use drive_core::engine::{Engine, EngineConfig, LocalFilter};
use drive_core::ipc::{BackupInfo, BackupState, ErrorCode, RpcError, SyncState};
use drive_core::remote::crypt::CryptRemote;
use drive_core::remote::subtree::Subtree;
use drive_core::remote::{Kind, Remote};
use drive_core::ulozto::UlozRemote;
use tracing::{info, warn};

use super::backups::RawBackend;
use super::{Service, internal, rpc};
use crate::backup::scan::{self, Filter};
use crate::backup::{BackupFolder, DEFAULT_KEEP_BYTES};
use crate::config::Config;
use crate::platform::{self, Platform, PlatformContext};

/// Free space left over after switching a folder back to a full copy.
const SPACE_MARGIN: u64 = 1 << 30;

/// A running on-demand folder.
pub(super) struct OnDemand {
    pub folder_id: String,
    pub name: String,
    pub path: PathBuf,
    /// Distinguishes this engine's ids from the others' (see `backups::encode_id`).
    pub slot: i64,
    /// What the folder integration was prepared with (its Windows sync root id).
    pub account: String,
    pub engine: Arc<Engine>,
    platform: Mutex<Option<Box<dyn Platform>>>,
    relay: tokio::task::JoinHandle<()>,
    /// Shown while switching back to a full copy.
    pub converting: Mutex<Option<String>>,
    pub last_complete_ms: Mutex<Option<i64>>,
}

impl OnDemand {
    pub fn stop(&self) {
        if let Some(p) = self.platform.lock().unwrap().take() {
            p.stop();
        }
        self.engine.shutdown();
        self.relay.abort();
    }
}

/// The account an on-demand folder's integration is prepared with (unique per folder).
fn on_demand_account(login: &str, folder_id: &str) -> String {
    format!("{login}~{folder_id}")
}

/// The on-demand engine's configuration, derived from the main one.
fn engine_config(cfg: &Config, main: &EngineConfig, folder: &BackupFolder) -> EngineConfig {
    EngineConfig {
        device_name: cfg.device_name.clone(),
        account: main.account.clone(),
        folder: folder.path.clone(),
        cache_limit_bytes: folder.keep_bytes.unwrap_or(DEFAULT_KEEP_BYTES),
        // Only this device changes its backups (others could, through their Drive folder).
        poll_interval: main.poll_interval.max(Duration::from_secs(5 * 60)),
        max_parallel_transfers: (main.max_parallel_transfers / 2).max(1),
        upload_limit_kbps: main.upload_limit_kbps,
        download_limit_kbps: main.download_limit_kbps,
        adaptive_upload: main.adaptive_upload,
        night: main.night,
        night_auto: main.night_auto,
        night_min_bytes: main.night_min_bytes,
        // Files already backed up are recognised, not uploaded again.
        scan_before_upload: true,
        ..Default::default()
    }
}

/// The folder's skip list as an engine filter: skipped files stay on this device and never
/// upload (the same rules as the plain backup: names, cache folders, Python environments).
fn skip_filter(folder: &BackupFolder) -> LocalFilter {
    let filter = Filter::new(&folder.excludes);
    let root = folder.path.clone();
    let tool_dirs: Mutex<HashMap<String, bool>> = Mutex::new(HashMap::new());
    Arc::new(move |rel: &str| {
        let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
        if parts.iter().any(|p| filter.skips_name(p)) {
            return true;
        }
        let mut dir = root.clone();
        let mut key = String::new();
        for p in &parts {
            dir.push(p);
            key.push('/');
            key.push_str(&name_key(p));
            if let Some(&tool) = tool_dirs.lock().unwrap().get(&key) {
                if tool {
                    return true;
                }
                continue;
            }
            if !dir.is_dir() {
                break;
            }
            let tool = scan::is_tool_folder(&dir);
            tool_dirs.lock().unwrap().insert(key.clone(), tool);
            if tool {
                return true;
            }
        }
        false
    })
}

impl Service {
    pub(super) fn on_demand_runtimes(&self) -> Vec<Arc<OnDemand>> {
        self.on_demand.lock().unwrap().clone()
    }

    pub(super) fn on_demand_for(&self, id: &str) -> Option<Arc<OnDemand>> {
        self.on_demand
            .lock()
            .unwrap()
            .iter()
            .find(|o| o.folder_id == id)
            .cloned()
    }

    pub(super) fn on_demand_by_slot(&self, slot: i64) -> Option<Arc<OnDemand>> {
        self.on_demand
            .lock()
            .unwrap()
            .iter()
            .find(|o| o.slot == slot)
            .cloned()
    }

    pub(super) fn stop_on_demand_all(&self) {
        let all: Vec<Arc<OnDemand>> = std::mem::take(&mut *self.on_demand.lock().unwrap());
        for o in all {
            o.stop();
        }
    }

    fn stop_on_demand(&self, id: &str) {
        let found = {
            let mut list = self.on_demand.lock().unwrap();
            let i = list.iter().position(|o| o.folder_id == id);
            i.map(|i| list.remove(i))
        };
        if let Some(o) = found {
            o.stop();
        }
    }

    fn next_slot(&self) -> i64 {
        let used: Vec<i64> = self
            .on_demand
            .lock()
            .unwrap()
            .iter()
            .map(|o| o.slot)
            .collect();
        (1..super::backups::SLOTS)
            .find(|s| !used.contains(s))
            .unwrap_or(super::backups::SLOTS - 1)
    }

    /// Starts every on-demand folder (after the backup engine started), and resumes switching
    /// back the ones that were on their way.
    pub(super) async fn start_on_demand_all(self: &Arc<Self>) {
        let Some(m) = self.backup_manager() else {
            return;
        };
        for f in m.folders().into_iter().filter(|f| f.on_demand && f.enabled) {
            let _ops = self.backup_ops.lock().await;
            if self.on_demand_for(&f.id).is_some() {
                continue;
            }
            match self.start_on_demand(&f).await {
                Ok(_) => {
                    self.on_demand_problems.lock().unwrap().remove(&f.id);
                    if f.leaving_on_demand {
                        tokio::spawn(self.clone().leave_on_demand(f.id.clone()));
                    }
                }
                Err(e) => {
                    warn!("starting on-demand {} failed: {e:#}", f.name);
                    self.on_demand_problems
                        .lock()
                        .unwrap()
                        .insert(f.id.clone(), format!("Couldn't connect the folder: {e:#}"));
                }
            }
        }
        self.backups_changed.notify_one();
    }

    /// Starts a folder's on-demand engine and folder integration. Call with `backup_ops` held.
    async fn start_on_demand(self: &Arc<Self>, folder: &BackupFolder) -> Result<Arc<OnDemand>> {
        let (main, raw, cfg) = {
            let inner = self.inner.lock().await;
            let r = inner.running.as_ref().context("not connected")?;
            (r.engine.clone(), r.raw.clone(), inner.config.clone())
        };
        let account = cfg.account.clone().context("no account")?;
        let cloud = cfg.backup_cloud.clone().context("backups aren't set up")?;
        let manager = self.backup_manager().context("backups aren't running")?;

        // The folder's cloud copy: Backups/<device>/<name>.
        let mremote = manager.engine().remote().clone();
        let root = manager
            .engine()
            .db()
            .node(ROOT_ID)?
            .and_then(|n| n.remote_id)
            .unwrap_or_else(|| cloud.device_slug.clone());
        let existing = mremote
            .list(&root)
            .await
            .context("reading the backups in the vault")?
            .into_iter()
            .find(|e| e.kind == Kind::Folder && name_key(&e.name) == name_key(&folder.name));
        let slug = match existing {
            Some(e) => e.id,
            None => mremote.create_folder(&root, &folder.name).await?.id,
        };
        let cipher = {
            let inner = self.inner.lock().await;
            inner
                .running
                .as_ref()
                .context("not connected")?
                .cipher
                .clone()
        };
        let remote: Arc<dyn Remote> = match raw {
            RawBackend::Uloz(client) => Arc::new(CryptRemote::new(
                Arc::new(UlozRemote::new(client, slug.clone())),
                cipher,
            )),
            RawBackend::Mem(mem) => Arc::new(CryptRemote::new(
                Arc::new(Subtree::new(mem, slug.clone())),
                cipher,
            )),
        };

        let db_path = self.on_demand_db(&folder.id);
        if db_path.exists()
            && let Ok(db) = Db::open(&db_path)
            && db
                .kv_get("folder_slug")
                .ok()
                .flatten()
                .is_some_and(|v| v != slug)
        {
            drop(db);
            warn!(
                "on-demand database of {} belongs to another cloud folder; starting over",
                folder.name
            );
            super::remove_db(&db_path);
        }
        let db = Arc::new(Db::open(&db_path).context("opening the folder's database")?);
        db.kv_set("folder_slug", &slug)?;

        let ec = engine_config(&cfg, &main.config(), folder);
        let engine = Engine::new_linked(db, remote, ec, &main)?;
        engine.set_local_filter(skip_filter(folder));
        let ctx = PlatformContext {
            engine: engine.clone(),
            folder: folder.path.clone(),
            cache_dir: self.paths.cache_dir.join(format!("ondemand-{}", folder.id)),
            display_name: format!("{} (Drive)", folder.name),
            account: on_demand_account(&account.login, &folder.id),
            // Reached through "Drive - Backups", not as another drive of its own.
            show_in_explorer: false,
        };
        // Preparing reads the whole folder (it may be large).
        let platform = tokio::task::spawn_blocking(move || platform::prepare(ctx))
            .await
            .context("preparing the folder")??;
        engine.set_local(platform.local_side());
        engine.start();
        if let Err(e) = platform.start() {
            engine.shutdown();
            return Err(e).context("connecting the folder");
        }
        let slot = self.next_slot();
        let relay = self.spawn_relay(engine.subscribe(), slot);
        let done = engine
            .db()
            .kv_get("complete_ms")
            .ok()
            .flatten()
            .and_then(|v| v.parse().ok());
        let od = Arc::new(OnDemand {
            folder_id: folder.id.clone(),
            name: folder.name.clone(),
            path: folder.path.clone(),
            slot,
            account: on_demand_account(&account.login, &folder.id),
            engine,
            platform: Mutex::new(Some(platform)),
            relay,
            converting: Mutex::new(None),
            last_complete_ms: Mutex::new(done),
        });
        self.on_demand.lock().unwrap().push(od.clone());
        info!("{} is on-demand now", folder.path.display());
        Ok(od)
    }

    fn on_demand_db(&self, id: &str) -> PathBuf {
        self.paths.data_dir.join(format!("ondemand-{id}.db"))
    }

    /// Fills in an on-demand folder's progress from its engine.
    pub(super) fn on_demand_info(&self, info: &mut BackupInfo) {
        let Some(od) = self.on_demand_for(&info.id) else {
            if !info.on_demand {
                return;
            }
            if let Some(p) = self.on_demand_problems.lock().unwrap().get(&info.id) {
                info.state = BackupState::Problem;
                info.detail = Some(p.clone());
            }
            return;
        };
        let st = od.engine.folder_stats().unwrap_or_default();
        info.files = st.all.files;
        info.bytes = st.all.bytes;
        info.done_files = st.all.done_files;
        info.done_bytes = st.all.done_bytes;
        info.pending = st.all.pending;
        info.local_bytes = st.all.bytes.saturating_sub(st.cloud_only_bytes);
        let status = od.engine.status();
        let waiting = matches!(
            status.state,
            SyncState::Paused | SyncState::Offline | SyncState::SignedOut
        );
        let (state, detail) = if let Some(c) = od.converting.lock().unwrap().clone() {
            (BackupState::Converting, Some(c))
        } else if !od.engine.mirror_linked() {
            (
                BackupState::Scanning,
                Some(if st.all.files > 0 {
                    format!("Checking {} files…", st.all.files)
                } else {
                    "Looking through the folder…".to_string()
                }),
            )
        } else if waiting && st.all.pending > 0 {
            (BackupState::Waiting, Some(status.headline.clone()))
        } else if st.all.pending > 0 || st.all.done_files < st.all.files {
            let left = st.all.files.saturating_sub(st.all.done_files);
            (BackupState::BackingUp, Some(format!("{left} files left")))
        } else {
            (BackupState::UpToDate, None)
        };
        let mut last = od.last_complete_ms.lock().unwrap();
        if state == BackupState::UpToDate && last.is_none_or(|t| now_ms() - t > 60_000) {
            let t = now_ms();
            *last = Some(t);
            let _ = od.engine.db().kv_set("complete_ms", &t.to_string());
        }
        info.last_complete_ms = *last;
        info.state = state;
        info.detail = detail;
    }

    /// Switches "Save space" on or off for a backed-up folder (and sets how much it keeps).
    pub(super) async fn set_backup_on_demand(
        self: &Arc<Self>,
        id: &str,
        on: bool,
        keep_bytes: Option<u64>,
    ) -> Result<BackupInfo, RpcError> {
        if on && !platform::ON_DEMAND_FOLDERS {
            return Err(rpc(
                ErrorCode::BadRequest,
                "Saving space in backed-up folders is only available on Windows for now.",
            ));
        }
        let keep_bytes = keep_bytes.map(|k| k.max(100 << 20));
        {
            let _ops = self.backup_ops.lock().await;
            let m = self.need_manager()?;
            let folder = m
                .folders()
                .into_iter()
                .find(|f| f.id == id)
                .ok_or_else(|| rpc(ErrorCode::NotFound, "No such backup"))?;
            if on && folder.on_demand {
                // Already on: a new size to keep, or cancelling a switch back.
                m.update_folder(id, |f| {
                    f.keep_bytes = keep_bytes.or(f.keep_bytes);
                    f.leaving_on_demand = false;
                });
                if let Some(od) = self.on_demand_for(id) {
                    if let Some(k) = keep_bytes {
                        od.engine.update_config(|c| c.cache_limit_bytes = k);
                    }
                    if folder.leaving_on_demand {
                        *od.converting.lock().unwrap() = None;
                        let _ = od.engine.set_pinned("", false);
                    }
                }
            } else if on {
                if !folder.enabled {
                    return Err(rpc(ErrorCode::BadRequest, "Turn this backup on first."));
                }
                if !folder.path.is_dir() {
                    return Err(rpc(
                        ErrorCode::BadRequest,
                        "The folder can't be found. Reconnect its drive first.",
                    ));
                }
                m.update_folder(id, |f| f.keep_bytes = keep_bytes.or(f.keep_bytes));
                m.set_on_demand(id, true).await.map_err(internal)?;
                self.save_backup_folders(&m).await?;
                let folder = m
                    .folders()
                    .into_iter()
                    .find(|f| f.id == id)
                    .ok_or_else(|| rpc(ErrorCode::NotFound, "No such backup"))?;
                if let Err(e) = self.start_on_demand(&folder).await {
                    warn!("making {} on-demand failed: {e:#}", folder.name);
                    // Nothing was freed yet: undo the folder integration and go back to a
                    // plain backup.
                    let path = folder.path.clone();
                    let acct = self.on_demand_account_of(id).await;
                    let _ =
                        tokio::task::spawn_blocking(move || platform::release(&path, &acct)).await;
                    m.set_on_demand(id, false).await.map_err(internal)?;
                    self.save_backup_folders(&m).await?;
                    return Err(rpc(
                        ErrorCode::Internal,
                        format!("Couldn't make {} save space: {e:#}", folder.name),
                    ));
                }
                self.on_demand_problems.lock().unwrap().remove(id);
            } else if folder.on_demand && !folder.leaving_on_demand {
                match self.on_demand_for(id) {
                    Some(od) => {
                        let st = od.engine.folder_stats().map_err(internal)?;
                        let free = fs4::available_space(&folder.path).unwrap_or(u64::MAX);
                        if st.cloud_only_bytes.saturating_add(SPACE_MARGIN) > free {
                            return Err(rpc(
                                ErrorCode::BadRequest,
                                format!(
                                    "{} has {} only in the cloud, and this disk has {} free. Free up some space first.",
                                    folder.name,
                                    crate::human_bytes(st.cloud_only_bytes),
                                    crate::human_bytes(free)
                                ),
                            ));
                        }
                        m.update_folder(id, |f| f.leaving_on_demand = true);
                        *od.converting.lock().unwrap() = Some("Downloading files back…".into());
                        tokio::spawn(self.clone().leave_on_demand(id.to_string()));
                    }
                    None => {
                        // Not running (it couldn't start): only an ordinary folder can be
                        // handed back safely.
                        let path = folder.path.clone();
                        let acct = self.on_demand_account_of(id).await;
                        let released = tokio::task::spawn_blocking(move || {
                            let (_, files) = platform::cloud_only(&path)?;
                            if files > 0 {
                                anyhow::bail!("{files} files are still only in the cloud");
                            }
                            platform::release(&path, &acct)
                        })
                        .await
                        .map_err(internal)?;
                        if let Err(e) = released {
                            return Err(rpc(
                                ErrorCode::BadRequest,
                                format!("{} can't switch back yet: {e:#}", folder.name),
                            ));
                        }
                        self.forget_on_demand_data(id);
                        m.set_on_demand(id, false).await.map_err(internal)?;
                    }
                }
            }
            self.save_backup_folders(&m).await?;
        }
        self.backups_changed.notify_one();
        self.backup_overview()
            .await
            .folders
            .into_iter()
            .find(|f| f.id == id)
            .ok_or_else(|| rpc(ErrorCode::NotFound, "No such backup"))
    }

    /// Removes data of on-demand folders that aren't on-demand anymore (left behind when it
    /// couldn't be removed right away).
    pub(super) fn forget_stale_on_demand_data(&self, cfg: &Config) {
        let Ok(entries) = std::fs::read_dir(&self.paths.data_dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name
                .strip_prefix("ondemand-")
                .and_then(|r| r.split('.').next())
            else {
                continue;
            };
            if !cfg.backups.iter().any(|f| f.id == id && f.on_demand) {
                self.forget_on_demand_data(id);
            }
        }
    }

    fn forget_on_demand_data(&self, id: &str) {
        super::remove_db(&self.on_demand_db(id));
        let _ = std::fs::remove_dir_all(self.paths.cache_dir.join(format!("ondemand-{id}")));
        self.on_demand_problems.lock().unwrap().remove(id);
    }

    /// Downloads everything back, then turns the folder into an ordinary one and hands it back
    /// to the plain backup. Survives restarts (it resumes when the folder starts again).
    async fn leave_on_demand(self: Arc<Self>, id: String) {
        let still_leaving = |svc: &Service| {
            svc.backup_manager().is_some_and(|m| {
                m.folders()
                    .iter()
                    .any(|f| f.id == id && f.on_demand && f.leaving_on_demand)
            })
        };
        loop {
            let Some(od) = self.on_demand_for(&id) else {
                return; // stopped (quit, restart): resumes when it starts again
            };
            if let Err(e) = od.engine.set_pinned("", true) {
                warn!("keeping {} on this device failed: {e}", od.name);
            }
            // Wait until every file is on this device.
            loop {
                if !still_leaving(&self) || self.on_demand_for(&id).is_none() {
                    return;
                }
                let st = od.engine.folder_stats().unwrap_or_default();
                if st.cloud_only_files == 0 {
                    break;
                }
                *od.converting.lock().unwrap() = Some(format!(
                    "Downloading {} back to this PC…",
                    crate::human_bytes(st.cloud_only_bytes)
                ));
                self.backups_changed.notify_one();
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            *od.converting.lock().unwrap() = Some("Finishing…".into());
            self.backups_changed.notify_one();

            let _ops = self.backup_ops.lock().await;
            if !still_leaving(&self) {
                return;
            }
            let Some(m) = self.backup_manager() else {
                return;
            };
            self.stop_on_demand(&id);
            let path = od.path.clone();
            let acct = od.account.clone();
            let released =
                tokio::task::spawn_blocking(move || platform::release(&path, &acct)).await;
            match released {
                Ok(Ok(())) => {}
                other => {
                    // Something new arrived from the cloud meanwhile: connect again and wait.
                    let e = match other {
                        Ok(Err(e)) => format!("{e:#}"),
                        Err(e) => e.to_string(),
                        Ok(Ok(())) => unreachable!(),
                    };
                    warn!("switching {} back isn't possible yet: {e}", od.name);
                    let folder = m.folders().into_iter().find(|f| f.id == id);
                    let restarted = match folder {
                        Some(f) => self.start_on_demand(&f).await,
                        None => return,
                    };
                    match restarted {
                        Ok(od) => {
                            *od.converting.lock().unwrap() = Some("Downloading files back…".into());
                        }
                        Err(e) => {
                            warn!("reconnecting {} failed: {e:#}", od.name);
                            self.on_demand_problems
                                .lock()
                                .unwrap()
                                .insert(id.clone(), format!("Couldn't connect the folder: {e:#}"));
                            self.backups_changed.notify_one();
                            return;
                        }
                    }
                    drop(_ops);
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }
            let (name, path) = (od.name.clone(), od.path.clone());
            drop(od); // the engine (and its database) closes once its tasks have ended
            if let Err(e) = m.set_on_demand(&id, false).await {
                warn!("handing {name} back to the backup failed: {e:#}");
            }
            if let Err(e) = self.save_backup_folders(&m).await {
                warn!("saving the settings failed: {}", e.message);
            }
            info!("{} is a full copy again", path.display());
            let me = self.clone();
            let id2 = id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                me.forget_on_demand_data(&id2);
            });
            self.backups_changed.notify_one();
            return;
        }
    }

    async fn on_demand_account_of(&self, folder_id: &str) -> String {
        let inner = self.inner.lock().await;
        let login = inner
            .config
            .account
            .as_ref()
            .map(|a| a.login.clone())
            .unwrap_or_default();
        on_demand_account(&login, folder_id)
    }

    /// The first on-demand folder, if any (signing out needs every file on this device).
    pub(super) async fn on_demand_folder_name(&self) -> Option<String> {
        let inner = self.inner.lock().await;
        inner
            .config
            .backups
            .iter()
            .find(|f| f.on_demand)
            .map(|f| f.name.clone())
    }
}
