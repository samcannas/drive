//! The disk space advisor's calls (the measuring and the report are in `crate::space`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use drive_core::ipc::{BackupInfo, ErrorCode, Event, RpcError, SpaceReport};
use serde_json::{Value, json};
use tracing::info;

use super::{Service, rpc};
use crate::backup::{self, DEFAULT_KEEP_BYTES};
use crate::platform;
use crate::space::scan::same_path;
use crate::space::{BackupRef, Context};

impl Service {
    /// What the report needs besides the measurement, as of now.
    pub(super) async fn space_context(&self) -> Context {
        let (folder, engine, account) = {
            let inner = self.inner.lock().await;
            (
                inner.config.folder_or_default(),
                inner.running.as_ref().map(|r| r.engine.clone()),
                inner.config.account.is_some(),
            )
        };
        let totals = engine
            .as_ref()
            .and_then(|e| e.db().storage_totals().ok())
            .unwrap_or_default();
        let backups = self
            .backup_overview()
            .await
            .folders
            .into_iter()
            .map(|f| BackupRef {
                id: f.id,
                path: PathBuf::from(&f.path),
                on_demand: f.on_demand,
                keep_bytes: if f.keep_bytes > 0 {
                    f.keep_bytes
                } else {
                    DEFAULT_KEEP_BYTES
                },
                cloud_only_bytes: if f.on_demand {
                    f.bytes.saturating_sub(f.local_bytes)
                } else {
                    0
                },
            })
            .collect();
        let local = totals.cache_bytes + totals.pinned_bytes;
        Context {
            home: dirs::home_dir().unwrap_or_default(),
            drive_folder: folder,
            own_dirs: vec![
                self.paths.data_dir.clone(),
                self.paths.cache_dir.clone(),
                self.paths.config_dir.clone(),
            ],
            backups,
            other_clouds: platform::other_cloud_folders(),
            known: backup::known_folders()
                .into_iter()
                .map(|(k, n, p)| (k, n.to_string(), p))
                .collect(),
            can_save_space: platform::ON_DEMAND_FOLDERS && account,
            drive_cache_bytes: totals.cache_bytes,
            drive_local_bytes: local,
            drive_cloud_only_bytes: totals.cloud_bytes.saturating_sub(local),
            drive_content_dir: (!cfg!(windows)).then(|| self.paths.cache_dir.join("content")),
            temp_dir: Some(crate::space::long_path(&std::env::temp_dir())),
            keep_bytes: DEFAULT_KEEP_BYTES,
        }
    }

    /// The report; starts the first scan, or a new one when asked to.
    pub(super) async fn space(self: &Arc<Self>, rescan: bool) -> SpaceReport {
        if rescan || self.space.needs_first_scan() {
            self.start_space_scan().await;
        }
        let ctx = self.space_context().await;
        self.space.report(&ctx)
    }

    pub(super) fn cancel_space_scan(&self) {
        self.space.cancel();
    }

    async fn start_space_scan(self: &Arc<Self>) {
        let Some(progress) = self.space.begin() else {
            return; // already scanning
        };
        let ctx = self.space_context().await;
        let home = ctx.home.clone();
        let ticker = {
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if !me.space.is_scanning() {
                        break;
                    }
                    let _ = me.events.send(Event::Space(me.space.report(&ctx)));
                }
            })
        };
        let me = self.clone();
        tokio::spawn(async move {
            let advisor = me.space.clone();
            let p = progress.clone();
            let _ = tokio::task::spawn_blocking(move || advisor.run(&home, &p)).await;
            ticker.abort();
            me.emit_space().await;
        });
    }

    pub(super) async fn emit_space(&self) {
        let ctx = self.space_context().await;
        let _ = self.events.send(Event::Space(self.space.report(&ctx)));
    }

    /// Backs a folder up (if it isn't yet) and makes it on-demand.
    pub(super) async fn save_space(
        self: &Arc<Self>,
        path: &str,
        keep_bytes: Option<u64>,
    ) -> Result<BackupInfo, RpcError> {
        if !platform::ON_DEMAND_FOLDERS {
            return Err(rpc(
                ErrorCode::BadRequest,
                "Saving space in a folder is only available on Windows for now. Back the folder up to keep it safe.",
            ));
        }
        let p = PathBuf::from(path.trim());
        if !p.is_absolute() || !p.is_dir() {
            return Err(rpc(ErrorCode::BadRequest, "That folder doesn't exist."));
        }
        let canon = super::backups::dunce_canonical(&p);
        let existing = self
            .backup_overview()
            .await
            .folders
            .into_iter()
            .find(|f| same_path(Path::new(&f.path), &canon));
        let (id, added) = match existing {
            Some(f) if f.on_demand && keep_bytes.is_none() => return Ok(f),
            Some(f) => {
                if !f.enabled {
                    self.set_backup_enabled(&f.id, true).await?;
                }
                (f.id, false)
            }
            None => (self.add_backup(path).await?.id, true),
        };
        let info = self
            .set_backup_on_demand(&id, true, keep_bytes)
            .await
            .map_err(|e| {
                if added {
                    rpc(
                        e.code,
                        format!(
                            "The folder is backed up now, but saving space in it didn't start: {}",
                            e.message
                        ),
                    )
                } else {
                    e
                }
            })?;
        info!("saving space in {}", canon.display());
        let me = self.clone();
        tokio::spawn(async move { me.emit_space().await });
        Ok(info)
    }

    /// Frees every downloaded, not pinned file in the Drive folder.
    pub(super) async fn free_drive_cache(self: &Arc<Self>) -> Result<Value, RpcError> {
        let engine = self
            .inner
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
            })?;
        let freed = engine.free_unpinned().await;
        let me = self.clone();
        tokio::spawn(async move { me.emit_space().await });
        Ok(json!({ "freed_bytes": freed }))
    }
}
