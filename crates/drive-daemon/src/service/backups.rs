//! The service side of folder backups: starting the mirror engine next to the main one,
//! answering the backup calls, and merging every engine's status, activity, issues and
//! transfers (main, mirror, on-demand folders) into one view for the app.
//!
//! Ids of items from backup engines are negative: `-(id * SLOTS + slot)`, where slot 0 is the
//! mirror engine and 1.. are on-demand folders.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, Weak};
use std::time::Duration;

use anyhow::{Context, Result};
use drive_core::db::{Db, now_ms};
use drive_core::engine::{Engine, EngineConfig};
use drive_core::ipc::{
    Activity, BackupInfo, BackupOverview, BackupState, ErrorCode, Event, Issue, RpcError, Status,
    SyncState, TransferItem, TransferList,
};
use drive_core::remote::crypt::CryptRemote;
use drive_core::remote::mem::MemRemote;
use drive_core::remote::subtree::Subtree;
use drive_core::remote::{Kind, Remote};
use drive_core::ulozto::{UlozClient, UlozRemote};
use tokio::sync::broadcast;
use tracing::{info, warn};

use super::{Service, internal, remote_rpc, rpc};
use crate::backup::local::BackupLocal;
use crate::backup::{self, BACKUPS_FOLDER, BackupCloud, BackupFolder, Manager};
use crate::config::Config;

/// What the backup engine's remote is built from (the main engine's backend).
#[derive(Clone)]
pub(super) enum RawBackend {
    Uloz(Arc<UlozClient>),
    Mem(Arc<MemRemote>),
}

/// Number of id slots (backup engines) in a negative id.
pub(super) const SLOTS: i64 = 1024;
/// The source of events from the main engine (backup engines use their slot).
pub(super) const MAIN_SOURCE: i64 = -1;

/// A backup engine's item id as the app sees it.
pub(super) fn encode_id(id: i64, slot: i64) -> i64 {
    -(id.saturating_mul(SLOTS) + slot)
}

/// `(slot, id)` of a backup item id; `None` for main engine ids.
pub(super) fn decode_id(id: i64) -> Option<(i64, i64)> {
    (id < 0).then(|| (-id % SLOTS, -id / SLOTS))
}

/// Where an engine path belongs: `(backup id, path inside the folder, folder name)`.
type Located = (String, String, String);

/// A running backup engine.
pub(super) struct BackupRuntime {
    pub manager: Arc<Manager>,
    relay: tokio::task::JoinHandle<()>,
}

impl BackupRuntime {
    fn stop(self) {
        self.manager.stop();
        self.relay.abort();
    }
}

/// The backup engine's configuration, derived from the main one.
pub(super) fn backup_engine_config(cfg: &Config, main: &EngineConfig) -> EngineConfig {
    EngineConfig {
        mirror: true,
        device_name: cfg.device_name.clone(),
        account: main.account.clone(),
        folder: dirs::home_dir().unwrap_or_default(),
        // The cloud copy only changes when someone tampers with it: look rarely.
        poll_interval: Duration::from_secs(30 * 60),
        // Backups share the bandwidth with the Drive folder; leave it room.
        max_parallel_transfers: (main.max_parallel_transfers / 2).max(1),
        adaptive_upload: main.adaptive_upload,
        night: main.night,
        night_min_bytes: main.night_min_bytes,
        debounce: Duration::from_secs(5),
        ..Default::default()
    }
}

/// Exclusions for the main engine: this device's own backups appear only through the backup
/// engine, never as cloud files in its Drive folder.
pub(super) fn main_exclusions(cfg: &Config, ec: &mut EngineConfig) {
    let Some(c) = &cfg.backup_cloud else {
        return;
    };
    match &c.backups_slug {
        // Only this device backs up: the Drive folder doesn't show `Backups` at all (they're
        // in "Drive - Backups").
        Some(b) if !c.other_devices => {
            ec.exclude_remote = vec![b.clone()];
            ec.exclude_local = vec![BACKUPS_FOLDER.to_string()];
        }
        _ => {
            ec.exclude_remote = vec![c.device_slug.clone()];
            ec.exclude_local = vec![c.vault_path()];
        }
    }
}

fn rank(s: SyncState) -> u8 {
    match s {
        SyncState::NotSetUp => 0,
        SyncState::UpToDate => 1,
        SyncState::Syncing => 2,
        SyncState::Paused => 3,
        SyncState::Offline => 4,
        SyncState::Error => 5,
        SyncState::SignedOut => 6,
    }
}

fn files(n: u32) -> String {
    format!("{n} {}", if n == 1 { "file" } else { "files" })
}

/// One status for both engines.
pub(super) fn merge_status(mut m: Status, b: &Status) -> Status {
    let main_uploading = m.uploading;
    m.uploading += b.uploading;
    m.downloading += b.downloading;
    m.pending += b.pending;
    m.transfer_bytes_total += b.transfer_bytes_total;
    m.transfer_bytes_done += b.transfer_bytes_done;
    m.issues += b.issues;
    m.cloud_bytes += b.cloud_bytes;
    m.cloud_files += b.cloud_files;
    if m.pending_delete_confirmation.is_none() {
        m.pending_delete_confirmation = b.pending_delete_confirmation;
    }
    if rank(b.state) > rank(m.state) {
        m.state = b.state;
        m.headline = match b.state {
            SyncState::Syncing if b.uploading > 0 => format!("Backing up {}", files(b.uploading)),
            // On-demand backed-up folders: opened files, or switching back to a full copy.
            SyncState::Syncing if b.downloading > 0 => {
                format!("Downloading {}", files(b.downloading))
            }
            SyncState::Syncing => "Backing up your folders".to_string(),
            _ => b.headline.clone(),
        };
    } else if m.state == SyncState::Syncing
        && main_uploading > 0
        && b.uploading > 0
        && m.downloading == 0
    {
        m.headline = format!("Uploading {}", files(m.uploading));
    }
    // Issue counts in the headline cover both engines.
    if m.state == SyncState::Error
        && m.pending_delete_confirmation.is_none()
        && m.headline.contains("attention")
    {
        m.headline = format!(
            "{} {} attention",
            m.issues,
            if m.issues == 1 {
                "item needs"
            } else {
                "items need"
            }
        );
    }
    m
}

/// One transfer list for every engine. Backup items get negative ids and readable paths.
pub(super) fn merge_transfers(
    main: &TransferList,
    others: &[(i64, &TransferList)],
    locate: &dyn Fn(i64, &str) -> Option<Located>,
) -> TransferList {
    let mut out = main.clone();
    for (slot, b) in others {
        out.items
            .extend(b.items.iter().cloned().map(|mut t: TransferItem| {
                t.id = encode_id(t.id, *slot);
                if let Some((_, rest, name)) = locate(*slot, &t.path) {
                    t.path = if rest.is_empty() {
                        name
                    } else {
                        format!("{name} › {rest}")
                    };
                }
                t
            }));
        out.total_count += b.total_count;
        out.upload_rate_bps += b.upload_rate_bps;
        out.download_rate_bps += b.download_rate_bps;
        out.upload_remaining_bytes += b.upload_remaining_bytes;
    }
    out.upload_eta_secs = (out.upload_rate_bps > 0)
        .then(|| out.upload_remaining_bytes / out.upload_rate_bps.max(1))
        .or(out.upload_eta_secs);
    out
}

fn map_activity(mut a: Activity, slot: i64, located: Option<Located>) -> Activity {
    a.id = encode_id(a.id, slot);
    if let Some((id, rest, _)) = located {
        a.path = rest;
        a.backup = Some(id);
    }
    a
}

fn map_issue(mut i: Issue, slot: i64, located: Option<Located>) -> Issue {
    i.id = encode_id(i.id, slot);
    match located {
        Some((id, rest, _)) => {
            i.path = Some(rest);
            i.backup = Some(id);
        }
        // Engine-wide issues (sign-in, mass deletes) are about backups as a whole.
        None => i.backup = Some(String::new()),
    }
    i
}

impl Service {
    pub(super) fn backup_manager(&self) -> Option<Arc<Manager>> {
        self.backups
            .lock()
            .unwrap()
            .as_ref()
            .map(|r| r.manager.clone())
    }

    pub(super) fn stop_backups(&self) {
        self.stop_on_demand_all();
        if let Some(r) = self.backups.lock().unwrap().take() {
            r.stop();
        }
    }

    /// Every running backup engine with its slot (mirror first).
    pub(super) fn backup_engines(&self) -> Vec<(i64, Arc<Engine>)> {
        let mut out = Vec::new();
        if let Some(m) = self.backup_manager() {
            out.push((0, m.engine().clone()));
        }
        out.extend(
            self.on_demand_runtimes()
                .into_iter()
                .map(|o| (o.slot, o.engine.clone())),
        );
        out
    }

    /// Where a backup engine's path belongs (see [`Located`]).
    pub(super) fn locate(&self, slot: i64, path: &str) -> Option<Located> {
        if slot == 0 {
            let (f, rest) = self.backup_manager()?.split_engine_path(path)?;
            Some((f.id, rest, f.name))
        } else {
            let o = self.on_demand_by_slot(slot)?;
            Some((o.folder_id.clone(), path.to_string(), o.name.clone()))
        }
    }

    /// Starts the backup engine next to the running main engine (after a start or restart).
    pub(super) async fn start_backups(self: &Arc<Self>) -> Result<()> {
        let _ops = self.backup_ops.lock().await;
        self.stop_backups();
        let (main, raw, cipher, vault_root, cfg) = {
            let inner = self.inner.lock().await;
            let Some(r) = &inner.running else {
                return Ok(());
            };
            (
                r.engine.clone(),
                r.raw.clone(),
                r.cipher.clone(),
                r.vault_root.clone(),
                inner.config.clone(),
            )
        };
        self.forget_stale_on_demand_data(&cfg);
        if cfg.backups.is_empty() {
            return Ok(());
        }
        let cloud = self.ensure_cloud(&main, &vault_root, &cfg).await?;

        let remote: Arc<dyn Remote> = match raw {
            RawBackend::Uloz(client) => Arc::new(CryptRemote::new(
                Arc::new(UlozRemote::new(client, cloud.device_slug.clone())),
                cipher,
            )),
            RawBackend::Mem(mem) => Arc::new(CryptRemote::new(
                Arc::new(Subtree::new(mem, cloud.device_slug.clone())),
                cipher,
            )),
        };
        let db_path = self.paths.data_dir.join("backups.db");
        if db_path.exists()
            && let Ok(db) = Db::open(&db_path)
            && db
                .kv_get("device_slug")
                .ok()
                .flatten()
                .is_some_and(|v| v != cloud.device_slug)
        {
            drop(db);
            warn!("backup database belongs to another cloud folder; starting over");
            super::remove_db(&db_path);
        }
        let db = Arc::new(Db::open(&db_path).context("opening the backup database")?);
        db.kv_set("device_slug", &cloud.device_slug)?;

        let ec = backup_engine_config(&cfg, &main.config());
        let engine = Engine::new_linked(db, remote, ec, &main)?;
        let sources = Arc::new(RwLock::new(HashMap::new()));
        engine.set_local(Arc::new(BackupLocal::new(sources.clone())));
        let weak = Arc::downgrade(self);
        let on_change: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(svc) = weak.upgrade() {
                svc.backups_changed.notify_one();
            }
        });
        let manager = Manager::new(engine.clone(), sources, cfg.backups.clone(), on_change);
        engine.start();
        manager.start();
        let relay = self.spawn_relay(engine.subscribe(), 0);
        *self.backups.lock().unwrap() = Some(BackupRuntime { manager, relay });
        if cfg.backups.iter().any(|f| f.on_demand) {
            tokio::spawn(self.clone().start_on_demand_after_backups());
        }
        info!(
            "backing up {} folders to {}",
            cfg.backups.len(),
            cloud.vault_path()
        );
        self.backups_changed.notify_one();
        Ok(())
    }

    async fn start_on_demand_after_backups(self: Arc<Self>) {
        self.start_on_demand_all().await;
    }

    /// Finds or creates `Backups/<device>` in the vault and records it (and the main engine's
    /// exclusion of it).
    async fn ensure_cloud(
        &self,
        main: &Arc<Engine>,
        vault_root: &str,
        cfg: &Config,
    ) -> Result<BackupCloud> {
        let remote = main.remote().clone();
        let top = remote.list(vault_root).await.context("reading the vault")?;
        let backups_id = match top
            .iter()
            .find(|e| e.kind == Kind::Folder && e.name == BACKUPS_FOLDER)
        {
            Some(e) => e.id.clone(),
            None => remote.create_folder(vault_root, BACKUPS_FOLDER).await?.id,
        };
        let devices = remote.list(&backups_id).await?;
        let known = cfg
            .backup_cloud
            .clone()
            .filter(|c| devices.iter().any(|e| e.id == c.device_slug));
        let cloud = match known {
            Some(c) => c,
            None => {
                // Same device name already there: it's this device's earlier backup (a
                // reinstall or a new sign-in); files that didn't change are recognised instead
                // of uploaded again.
                let name = backup::unique_name(&cfg.device_name, &[]);
                let slug = match devices.iter().find(|e| {
                    e.kind == Kind::Folder
                        && drive_core::db::name_key(&e.name) == drive_core::db::name_key(&name)
                }) {
                    Some(e) => e.id.clone(),
                    None => remote.create_folder(&backups_id, &name).await?.id,
                };
                BackupCloud {
                    device_folder: name,
                    device_slug: slug,
                    backups_slug: None,
                    other_devices: false,
                }
            }
        };
        let other_devices = devices
            .iter()
            .any(|e| e.kind == Kind::Folder && e.id != cloud.device_slug);
        let cloud = BackupCloud {
            backups_slug: Some(backups_id.clone()),
            other_devices,
            ..cloud
        };
        if cfg.backup_cloud.as_ref() == Some(&cloud) {
            return Ok(cloud);
        }
        let mut inner = self.inner.lock().await;
        inner.config.backup_cloud = Some(cloud.clone());
        inner.config.save(&self.paths)?;
        let cfg = inner.config.clone();
        if let (Some(r), Some(account)) = (&inner.running, &cfg.account) {
            let ec = self.engine_config(&cfg, account);
            r.engine.update_config(|e| *e = ec);
        }
        Ok(cloud)
    }

    /// Forwards one engine's events, merged with the other engine where needed.
    pub(super) fn spawn_relay(
        self: &Arc<Self>,
        mut rx: broadcast::Receiver<Event>,
        source: i64,
    ) -> tokio::task::JoinHandle<()> {
        let weak: Weak<Service> = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                let ev = match rx.recv().await {
                    Ok(ev) => ev,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let Some(svc) = weak.upgrade() else { break };
                let out = match ev {
                    Event::Status(_) => Event::Status(svc.status().await),
                    Event::IssuesChanged { .. } => Event::IssuesChanged {
                        count: svc.status().await.issues,
                    },
                    Event::Transfers(list) => {
                        let mut t = svc.last_transfers.lock().unwrap();
                        t.insert(source, list);
                        // Forget engines that stopped.
                        let live: Vec<i64> =
                            svc.backup_engines().into_iter().map(|(s, _)| s).collect();
                        t.retain(|s, _| *s == MAIN_SOURCE || live.contains(s));
                        let main = t.get(&MAIN_SOURCE).cloned().unwrap_or_default();
                        let others: Vec<(i64, &TransferList)> = t
                            .iter()
                            .filter(|(s, _)| **s != MAIN_SOURCE)
                            .map(|(s, l)| (*s, l))
                            .collect();
                        Event::Transfers(merge_transfers(&main, &others, &|s, p| svc.locate(s, p)))
                    }
                    Event::Activity(a) if source != MAIN_SOURCE => {
                        let located = svc.locate(source, &a.path);
                        Event::Activity(map_activity(a, source, located))
                    }
                    other => other,
                };
                let _ = svc.events.send(out);
            }
        })
    }

    /// Emits the backup overview when it changes (debounced).
    pub(super) async fn backups_loop(self: Arc<Self>) {
        loop {
            self.backups_changed.notified().await;
            tokio::time::sleep(Duration::from_millis(300)).await;
            let ov = self.backup_overview().await;
            let _ = self.events.send(Event::Backups(ov));
            let cfg = self.inner.lock().await.config.clone();
            self.refresh_backups_folder(&cfg, None);
        }
    }

    /// Keeps "Drive - Backups" in step with the backed-up folders: one link per folder that's
    /// backed up, and its entry in the file manager's sidebar while there are any. `login`
    /// names the account when it's no longer in `cfg` (signing out).
    pub(super) fn refresh_backups_folder(&self, cfg: &Config, login: Option<&str>) {
        let Some(login) = login.or(cfg.account.as_ref().map(|a| a.login.as_str())) else {
            return;
        };
        let wanted: Vec<(String, PathBuf)> = if cfg.account.is_some() {
            cfg.backups
                .iter()
                .filter(|f| f.enabled)
                .map(|f| (f.name.clone(), f.path.clone()))
                .collect()
        } else {
            Vec::new()
        };
        let dir = backup::links::folder_for(&cfg.folder_or_default());
        {
            let state = (dir.clone(), wanted.clone());
            let mut last = self.backups_folder.lock().unwrap();
            if last.as_ref() == Some(&state) {
                return;
            }
            *last = Some(state);
        }
        if let Err(e) = backup::links::sync(&dir, &wanted) {
            warn!("updating {} failed: {e:#}", dir.display());
        }
        let key = format!("{login}|{}", self.paths.data_dir.display());
        let show = !wanted.is_empty() && dir.is_dir();
        crate::platform::backups_sidebar_entry(&key, show.then_some(dir.as_path()));
    }

    pub(super) async fn backup_overview(&self) -> BackupOverview {
        let cfg = self.inner.lock().await.config.clone();
        let manager = self.backup_manager();
        let mut folders: Vec<BackupInfo> = match &manager {
            Some(m) => m.infos(),
            None => cfg
                .backups
                .iter()
                .map(|f| BackupInfo {
                    id: f.id.clone(),
                    name: f.name.clone(),
                    path: f.path.display().to_string(),
                    known: f.known,
                    enabled: f.enabled,
                    state: if f.enabled {
                        BackupState::Waiting
                    } else {
                        BackupState::Off
                    },
                    detail: f.enabled.then(|| "Starting…".to_string()),
                    excludes: f.excludes.clone(),
                    files: 0,
                    bytes: 0,
                    done_files: 0,
                    done_bytes: 0,
                    pending: 0,
                    skipped_online_only: 0,
                    added_ms: f.added_ms,
                    last_complete_ms: None,
                    on_demand: f.on_demand,
                    on_demand_available: crate::platform::ON_DEMAND_FOLDERS,
                    local_bytes: 0,
                    keep_bytes: f.keep_bytes.unwrap_or(backup::DEFAULT_KEEP_BYTES),
                })
                .collect(),
        };
        for f in &mut folders {
            self.on_demand_info(f);
        }
        let suggestions = if cfg.account.is_some() {
            let backed: Vec<PathBuf> = cfg.backups.iter().map(|f| f.path.clone()).collect();
            let links_dir = backup::links::folder_for(&cfg.folder_or_default());
            let own = [
                self.paths.data_dir.as_path(),
                self.paths.cache_dir.as_path(),
                self.paths.config_dir.as_path(),
                links_dir.as_path(),
            ];
            self.suggestions
                .list(&backed, &cfg.folder_or_default(), &own)
        } else {
            Vec::new()
        };
        BackupOverview {
            folders,
            suggestions,
            suggestions_dismissed: cfg.backup_suggestions_dismissed,
            cloud_folder: cfg.backup_cloud.as_ref().map(|c| c.vault_path()),
            default_excludes: backup::scan::default_excludes(),
        }
    }

    pub(super) async fn add_backup(self: &Arc<Self>, path: &str) -> Result<BackupInfo, RpcError> {
        let path = PathBuf::from(path.trim());
        if !path.is_absolute() || !path.is_dir() {
            return Err(rpc(ErrorCode::BadRequest, "That folder doesn't exist."));
        }
        let path = dunce_canonical(&path);
        let folder = {
            let _ops = self.backup_ops.lock().await;
            let mut inner = self.inner.lock().await;
            if inner.config.account.is_none() || inner.running.is_none() {
                return Err(rpc(
                    ErrorCode::NotSetUp,
                    "Connect your uloz.to account first.",
                ));
            }
            let others: Vec<PathBuf> = inner
                .config
                .backups
                .iter()
                .map(|f| f.path.clone())
                .collect();
            let other_refs: Vec<&Path> = others.iter().map(|p| p.as_path()).collect();
            let links_dir = backup::links::folder_for(&inner.config.folder_or_default());
            let own = [
                self.paths.data_dir.as_path(),
                self.paths.cache_dir.as_path(),
                self.paths.config_dir.as_path(),
                links_dir.as_path(),
            ];
            if let Some(problem) =
                backup::overlap_problem(&path, &inner.config.folder_or_default(), &own, &other_refs)
            {
                return Err(rpc(ErrorCode::BadRequest, problem));
            }
            let known = backup::known_folders()
                .into_iter()
                .find(|(_, _, p)| dunce_canonical(p) == path);
            let base = match &known {
                Some((_, name, _)) => name.to_string(),
                None => path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Folder".into()),
            };
            let taken: Vec<String> = inner
                .config
                .backups
                .iter()
                .map(|f| f.name.clone())
                .collect();
            let folder = BackupFolder {
                id: format!("{:012x}", rand::random::<u64>() & 0xffff_ffff_ffff),
                name: backup::unique_name(&base, &taken),
                path,
                known: known.map(|(k, _, _)| k),
                enabled: true,
                excludes: backup::scan::default_excludes(),
                added_ms: now_ms(),
                on_demand: false,
                keep_bytes: None,
                leaving_on_demand: false,
            };
            inner.config.backups.push(folder.clone());
            inner.config.save(&self.paths).map_err(internal)?;
            folder
        };
        info!("backing up {} as {}", folder.path.display(), folder.name);
        match self.backup_manager() {
            Some(m) => m.add(folder.clone()).await,
            None => self.start_backups().await.map_err(|e| {
                rpc(
                    ErrorCode::Network,
                    format!("Couldn't set up backups in your vault: {e:#}"),
                )
            })?,
        }
        self.backups_changed.notify_one();
        let ov = self.backup_overview().await;
        ov.folders
            .into_iter()
            .find(|f| f.id == folder.id)
            .ok_or_else(|| internal("the new backup disappeared"))
    }

    pub(super) async fn save_backup_folders(&self, manager: &Manager) -> Result<(), RpcError> {
        let mut inner = self.inner.lock().await;
        inner.config.backups = manager.folders();
        inner.config.save(&self.paths).map_err(internal)?;
        self.backups_changed.notify_one();
        Ok(())
    }

    pub(super) fn need_manager(&self) -> Result<Arc<Manager>, RpcError> {
        self.backup_manager()
            .ok_or_else(|| rpc(ErrorCode::NotSetUp, "Backups aren't running right now."))
    }

    pub(super) async fn set_backup_enabled(&self, id: &str, enabled: bool) -> Result<(), RpcError> {
        let _ops = self.backup_ops.lock().await;
        let m = self.need_manager()?;
        m.set_enabled(id, enabled)
            .await
            .map_err(|e| rpc(ErrorCode::NotFound, format!("{e:#}")))?;
        self.save_backup_folders(&m).await
    }

    pub(super) async fn remove_backup(
        &self,
        id: &str,
        delete_from_cloud: bool,
    ) -> Result<(), RpcError> {
        let _ops = self.backup_ops.lock().await;
        match self.backup_manager() {
            Some(m) => {
                m.remove(id, delete_from_cloud).await.map_err(|e| {
                    match e.downcast::<drive_core::remote::RemoteError>() {
                        Ok(re) => remote_rpc(re),
                        Err(e) => rpc(ErrorCode::NotFound, format!("{e:#}")),
                    }
                })?;
                self.save_backup_folders(&m).await
            }
            None => {
                // Not running (signed out, starting): just forget it.
                let mut inner = self.inner.lock().await;
                inner.config.backups.retain(|f| f.id != id);
                inner.config.save(&self.paths).map_err(internal)?;
                self.backups_changed.notify_one();
                Ok(())
            }
        }
    }

    pub(super) async fn set_backup_excludes(
        &self,
        id: &str,
        excludes: Vec<String>,
    ) -> Result<BackupInfo, RpcError> {
        let excludes: Vec<String> = excludes
            .into_iter()
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect();
        if let Some(bad) = excludes
            .iter()
            .find(|e| e.contains('/') || e.contains('\\'))
        {
            return Err(rpc(
                ErrorCode::BadRequest,
                format!(
                    "\"{bad}\" contains a slash. Use a file or folder name, like node_modules or *.iso."
                ),
            ));
        }
        {
            let _ops = self.backup_ops.lock().await;
            let m = self.need_manager()?;
            m.set_excludes(id, excludes)
                .await
                .map_err(|e| rpc(ErrorCode::NotFound, format!("{e:#}")))?;
            self.save_backup_folders(&m).await?;
        }
        self.backup_overview()
            .await
            .folders
            .into_iter()
            .find(|f| f.id == id)
            .ok_or_else(|| rpc(ErrorCode::NotFound, "No such backup"))
    }

    pub(super) async fn dismiss_backup_suggestions(&self) -> Result<(), RpcError> {
        let mut inner = self.inner.lock().await;
        inner.config.backup_suggestions_dismissed = true;
        inner.config.save(&self.paths).map_err(internal)?;
        self.backups_changed.notify_one();
        Ok(())
    }

    // ---- merged views --------------------------------------------------------------------

    pub(super) fn merged_status(&self, main: Status) -> Status {
        self.backup_engines()
            .into_iter()
            .fold(main, |m, (_, e)| merge_status(m, &e.status()))
    }

    pub(super) fn merged_activity(
        &self,
        mut list: Vec<Activity>,
        limit: u32,
    ) -> Result<Vec<Activity>, RpcError> {
        let engines = self.backup_engines();
        if engines.is_empty() {
            return Ok(list);
        }
        for (slot, e) in engines {
            let more = e.db().recent_activity(limit, None).map_err(internal)?;
            list.extend(more.into_iter().map(|a| {
                let located = self.locate(slot, &a.path);
                map_activity(a, slot, located)
            }));
        }
        list.sort_by(|a, b| b.at_ms.cmp(&a.at_ms));
        list.truncate(limit as usize);
        Ok(list)
    }

    pub(super) fn merged_issues(&self, mut list: Vec<Issue>) -> Result<Vec<Issue>, RpcError> {
        let engines = self.backup_engines();
        if engines.is_empty() {
            return Ok(list);
        }
        for (slot, e) in engines {
            let more = e.issues().map_err(internal)?;
            list.extend(more.into_iter().map(|i| {
                let located = i.path.as_deref().and_then(|p| self.locate(slot, p));
                // On-demand engines are one folder each: their engine-wide issues are too.
                let located = located.or_else(|| {
                    (slot > 0)
                        .then(|| self.on_demand_by_slot(slot))
                        .flatten()
                        .map(|o| (o.folder_id.clone(), String::new(), o.name.clone()))
                        .filter(|_| i.path.is_none())
                });
                map_issue(i, slot, located)
            }));
        }
        list.sort_by(|a, b| b.created_ms.cmp(&a.created_ms));
        Ok(list)
    }

    pub(super) fn merged_transfers(&self, main: TransferList) -> TransferList {
        let engines = self.backup_engines();
        if engines.is_empty() {
            return main;
        }
        let lists: Vec<(i64, TransferList)> = engines
            .into_iter()
            .map(|(s, e)| (s, e.transfers()))
            .collect();
        let refs: Vec<(i64, &TransferList)> = lists.iter().map(|(s, l)| (*s, l)).collect();
        merge_transfers(&main, &refs, &|s, p| self.locate(s, p))
    }

    /// The engine that owns a (possibly negative, i.e. backup) issue or transfer id.
    pub(super) fn engine_for_id(
        &self,
        main: Arc<Engine>,
        id: i64,
    ) -> Result<(Arc<Engine>, i64), RpcError> {
        match decode_id(id) {
            None => Ok((main, id)),
            Some((slot, id)) => self
                .backup_engines()
                .into_iter()
                .find(|(s, _)| *s == slot)
                .map(|(_, e)| (e, id))
                .ok_or_else(|| rpc(ErrorCode::NotFound, "That backup isn't running right now.")),
        }
    }
}

/// Canonical path without Windows' `\\?\` prefix (so it compares with user-facing paths).
pub(super) fn dunce_canonical(p: &Path) -> PathBuf {
    let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC\\") => PathBuf::from(rest),
        _ => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_merge() {
        let main = Status {
            state: SyncState::UpToDate,
            headline: "Up to date".into(),
            ..Default::default()
        };
        let b = Status {
            state: SyncState::Syncing,
            uploading: 2,
            pending: 5,
            ..Default::default()
        };
        let s = merge_status(main, &b);
        assert_eq!(s.state, SyncState::Syncing);
        assert_eq!(s.headline, "Backing up 2 files");
        assert_eq!(s.uploading, 2);
        let main = Status {
            state: SyncState::Error,
            headline: "1 item needs attention".into(),
            issues: 1,
            ..Default::default()
        };
        let b = Status {
            state: SyncState::Error,
            issues: 2,
            headline: "2 items need attention".into(),
            ..Default::default()
        };
        assert_eq!(merge_status(main, &b).headline, "3 items need attention");
    }

    #[test]
    fn ids_round_trip() {
        for (id, slot) in [(1, 0), (7, 1), (123_456_789, 1023), (1, 1023)] {
            let e = encode_id(id, slot);
            assert!(e < 0);
            assert_eq!(decode_id(e), Some((slot, id)));
        }
        assert_eq!(decode_id(5), None);
    }
}
