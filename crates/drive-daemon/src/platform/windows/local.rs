//! The engine's view of the sync root: cloud changes become placeholder operations.

use std::io;
use std::path::Path;
use std::sync::Arc;

use cloud_filter::metadata::{Metadata, MetadataExt};
use cloud_filter::placeholder::{ConvertOptions, PinOptions, PinState, Placeholder, UpdateOptions};
use cloud_filter::placeholder_create_info::PlaceholderCreateInfo;
use drive_core::db::NodeId;
use drive_core::engine::local::{LocalItem, LocalSide, UploadSource};
use drive_core::remote::Kind;
use tracing::{debug, info, warn};

use super::util::{
    self, BlobId, encode_blob, filetime_from_ms, inspect, is_sharing_violation, open_attrs,
    open_exclusive, open_meta, open_read_deny_write, parent_and_name, win_err,
};
use super::{Msg, Shared};

pub struct WinLocal {
    shared: Arc<Shared>,
}

impl WinLocal {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    fn blob(&self, id: NodeId) -> Vec<u8> {
        encode_blob(self.shared.tag, id)
    }

    fn metadata(kind: Kind, size: u64, mtime_ms: i64) -> Metadata {
        let ft = filetime_from_ms(mtime_ms);
        let m = match kind {
            Kind::File => Metadata::file().size(size),
            Kind::Folder => Metadata::directory(),
        };
        m.creation_time(ft)
            .last_write_time(ft)
            .change_time(ft)
            .last_access_time(ft)
    }

    /// Whether `id` names a live node (vs. a leftover identity).
    fn node_alive(&self, id: NodeId) -> bool {
        self.shared
            .engine()
            .and_then(|e| e.db().node(id).ok().flatten())
            .is_some_and(|n| !n.deleted)
    }

    /// `create` found something at the path already.
    fn create_over_existing(&self, item: &LocalItem, abs: &Path, err: io::Error) -> io::Result<()> {
        let Some(existing) = inspect(abs, self.shared.tag)? else {
            return Err(err);
        };
        match (&existing.ph, item.kind, existing.is_dir) {
            // Ours already (e.g. a retried create).
            (Some(ph), _, _) if ph.id == BlobId::Node(item.id) => Ok(()),
            // An ordinary folder (or one from an older database) with the same name: merge.
            (ph, Kind::Folder, true)
                if ph.as_ref().is_none_or(|p| match p.id {
                    BlobId::Node(other) => !self.node_alive(other),
                    BlobId::Foreign => true,
                }) =>
            {
                info!("adopting existing folder {}", item.path);
                set_identity(abs, &self.blob(item.id), true)?;
                // Its contents may be unknown to the engine.
                self.shared.send(Msg::Subtree(abs.to_path_buf()));
                Ok(())
            }
            // A stale placeholder without local changes: replace it.
            (Some(ph), Kind::File, false)
                if (ph.in_sync || ph.partial_on_disk)
                    && match ph.id {
                        BlobId::Node(other) => !self.node_alive(other),
                        BlobId::Foreign => true,
                    } =>
            {
                debug!("replacing stale placeholder {}", item.path);
                let (parent, name) = parent_and_name(&item.path);
                PlaceholderCreateInfo::new(name)
                    .metadata(Self::metadata(item.kind, item.size, item.mtime_ms))
                    .blob(self.blob(item.id))
                    .mark_in_sync()
                    .overwrite()
                    .create(self.shared.abs(parent))
                    .map(|_| ())
                    .map_err(win_err)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", item.path),
            )),
        }
    }

    fn hydrate_in_background(&self, item: &LocalItem) {
        let shared = self.shared.clone();
        let item = item.clone();
        // Detached: the join handle isn't needed.
        drop(self.shared.rt.spawn_blocking(move || {
            let me = WinLocal {
                shared: shared.clone(),
            };
            if let Err(e) = me.hydrate(&item) {
                warn!("downloading the new version of {} failed: {e}", item.path);
            } else if let Some(engine) = shared.engine() {
                engine.mark_hydrated(item.id, true);
            }
        }));
    }
}

/// Makes `path` a placeholder with `blob` (converting an ordinary item, or replacing the
/// identity of an existing placeholder).
pub fn set_identity(path: &Path, blob: &[u8], in_sync: bool) -> io::Result<()> {
    let is_placeholder = inspect(path, 0)?.is_some_and(|d| d.ph.is_some());
    let mut ph = Placeholder::from(open_attrs(path)?);
    if is_placeholder {
        let mut opts = UpdateOptions::default().blob(blob);
        if in_sync {
            opts = opts.mark_in_sync();
        }
        ph.update(opts, None).map_err(win_err)?;
    } else {
        let mut opts = ConvertOptions::default().blob(blob.to_vec());
        if in_sync {
            opts = opts.mark_in_sync();
        }
        ph.convert_to_placeholder(opts, None).map_err(win_err)?;
    }
    Ok(())
}

/// Best effort: marks a folder placeholder (or the sync root) in sync.
pub fn mark_folder_in_sync(path: &Path) {
    match open_attrs(path) {
        Ok(f) => {
            if let Err(e) = Placeholder::from(f).mark_in_sync(true, None) {
                debug!("marking {} in sync: {e}", path.display());
            }
        }
        Err(e) => debug!("opening {}: {e}", path.display()),
    }
}

fn not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(3)
}

impl LocalSide for WinLocal {
    fn create(&self, item: &LocalItem) -> io::Result<()> {
        if item.path.is_empty() {
            return Ok(());
        }
        let _op = self.shared.begin_op(&[&item.path]);
        let abs = self.shared.abs(&item.path);
        let (parent, name) = parent_and_name(&item.path);
        let mut info = PlaceholderCreateInfo::new(name)
            .metadata(Self::metadata(item.kind, item.size, item.mtime_ms))
            .blob(self.blob(item.id))
            .mark_in_sync();
        if item.kind == Kind::Folder {
            info = info.has_no_children();
        }
        if let Err(e) = info.create(self.shared.abs(parent)) {
            let e = win_err(e);
            if abs.exists() {
                self.create_over_existing(item, &abs, e)?;
            } else {
                return Err(e);
            }
        }
        if item.pinned {
            set_pin(&abs, PinState::Pinned, false)?;
        }
        debug!("created placeholder {}", item.path);
        Ok(())
    }

    fn replace(&self, item: &LocalItem) -> io::Result<()> {
        let _op = self.shared.begin_op(&[&item.path]);
        let abs = self.shared.abs(&item.path);
        let Some(disk) = inspect(&abs, self.shared.tag)? else {
            return Err(io::Error::new(io::ErrorKind::NotFound, item.path.clone()));
        };
        if disk.is_dir {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a folder", item.path),
            ));
        }
        let blob = self.blob(item.id);
        let f = open_exclusive(&abs).map_err(|e| {
            if is_sharing_violation(&e) {
                io::Error::new(io::ErrorKind::WouldBlock, format!("{} is open", item.path))
            } else {
                e
            }
        })?;
        let mut ph = Placeholder::from(f);
        if disk.ph.is_none() {
            // Clean but never converted (e.g. converting after its upload failed).
            ph.convert_to_placeholder(
                ConvertOptions::default().blob(blob.clone()).mark_in_sync(),
                None,
            )
            .map_err(win_err)?;
        }
        // Windows refuses to dehydrate pinned files: unpin for the update, then pin again and
        // download the new version.
        let pinned = item.pinned || disk.ph.as_ref().is_some_and(|p| p.pin == PinState::Pinned);
        if pinned {
            ph.mark_pin(PinState::Unspecified, PinOptions::default())
                .map_err(win_err)?;
        }
        let updated = ph
            .update(
                UpdateOptions::default()
                    .metadata(Self::metadata(Kind::File, item.size, item.mtime_ms))
                    .blob(&blob)
                    .update_if_in_sync()
                    .mark_in_sync()
                    .dehydrate(),
                None,
            )
            .map(|_| ());
        if pinned {
            let _ = ph.mark_pin(PinState::Pinned, PinOptions::default());
        }
        drop(ph);
        updated.map_err(win_err)?;
        self.shared.clear_coverage(item.id);
        debug!("replaced {} with a newer cloud version", item.path);
        if pinned {
            self.hydrate_in_background(item);
        }
        Ok(())
    }

    fn rename(&self, from: &str, item: &LocalItem) -> io::Result<()> {
        let _op = self.shared.begin_op(&[from, &item.path]);
        let src = self.shared.abs(from);
        let dst = self.shared.abs(&item.path);
        match std::fs::rename(&src, &dst) {
            Ok(()) => {
                debug!("moved {from} -> {}", item.path);
                Ok(())
            }
            // Already where it should be.
            Err(e) if not_found(&e) && dst.exists() => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn remove(&self, item: &LocalItem) -> io::Result<()> {
        let _op = self.shared.begin_op(&[&item.path]);
        let abs = self.shared.abs(&item.path);
        let result = match std::fs::symlink_metadata(&abs) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(&abs),
            Ok(_) => super::remove_file_force(&abs),
            Err(e) => Err(e),
        };
        match result {
            Ok(()) => {
                self.shared.clear_coverage(item.id);
                debug!("removed {}", item.path);
                Ok(())
            }
            Err(e) if not_found(&e) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn open_for_upload(&self, item: &LocalItem) -> io::Result<UploadSource> {
        let abs = self.shared.abs(&item.path);
        // Probe with writers denied: if an app still has the file open for writing, wait
        // until it's done. The probe is released straight away so the upload itself never
        // blocks an app from saving; a save during the upload is caught by `uploaded()`
        // (size/mtime changed) and uploaded again.
        drop(open_read_deny_write(&abs, false).map_err(|e| {
            if is_sharing_violation(&e) {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("{} is open in another app", item.path),
                )
            } else {
                e
            }
        })?);
        let file = util::open_read_shared(&abs)?;
        let meta = file.metadata()?;
        if meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is a folder", item.path),
            ));
        }
        Ok(UploadSource {
            size: meta.len(),
            mtime_ms: util::mtime_ms(&meta),
            file,
        })
    }

    fn uploaded(&self, item: &LocalItem, size: u64, mtime_ms: i64) -> io::Result<bool> {
        let _op = self.shared.begin_op(&[&item.path]);
        let abs = self.shared.abs(&item.path);
        // Deny writers while we compare and mark, so a concurrent save can't be marked in sync.
        let file = match open_read_deny_write(&abs, true) {
            Ok(f) => f,
            Err(e) if is_sharing_violation(&e) => {
                // Someone is writing: their change will be reported (and uploaded) separately.
                // Re-check this path once the app lets go of it.
                self.shared.send(Msg::Paths(vec![abs]));
                return Ok(true);
            }
            Err(e) if not_found(&e) => return Ok(true),
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if meta.len() != size || util::mtime_ms(&meta) != mtime_ms {
            debug!("{} changed while uploading", item.path);
            return Ok(false);
        }
        let blob = self.blob(item.id);
        let mut ph = Placeholder::from(file);
        let result = match ph.info() {
            Ok(Some(_)) => ph
                .update(UpdateOptions::default().blob(&blob).mark_in_sync(), None)
                .map(|_| ()),
            Ok(None) => ph
                .convert_to_placeholder(ConvertOptions::default().blob(blob).mark_in_sync(), None)
                .map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            warn!("marking {} as synced failed: {e}", item.path);
            self.shared.send(Msg::Paths(vec![abs]));
            return Ok(true);
        }
        if item.pinned {
            let _ = ph.mark_pin(PinState::Pinned, PinOptions::default());
        }
        debug!("{} is in sync", item.path);
        Ok(true)
    }

    fn evict(&self, item: &LocalItem) -> io::Result<bool> {
        let _op = self.shared.begin_op(&[&item.path]);
        let abs = self.shared.abs(&item.path);
        let Some(disk) = inspect(&abs, self.shared.tag)? else {
            return Ok(false);
        };
        if disk.is_dir
            || disk
                .ph
                .as_ref()
                .is_some_and(|p| p.pin == PinState::Pinned || !p.in_sync)
        {
            return Ok(false);
        }
        let file = match open_exclusive(&abs) {
            Ok(f) => f,
            Err(e) if is_sharing_violation(&e) || not_found(&e) => return Ok(false),
            Err(e) => return Err(e),
        };
        let mut ph = Placeholder::from(file);
        let result = if disk.ph.is_none() {
            // Uploaded but never converted: convert and drop the data in one go.
            ph.convert_to_placeholder(
                ConvertOptions::default()
                    .blob(self.blob(item.id))
                    .mark_in_sync()
                    .dehydrate(),
                None,
            )
            .map(|_| ())
        } else {
            ph.update(
                UpdateOptions::default().update_if_in_sync().dehydrate(),
                None,
            )
            .map(|_| ())
        };
        match result {
            Ok(()) => {
                self.shared.clear_coverage(item.id);
                debug!("freed up {}", item.path);
                Ok(true)
            }
            Err(e) => {
                debug!("can't free up {} now: {e}", item.path);
                Ok(false)
            }
        }
    }

    fn hydrate(&self, item: &LocalItem) -> io::Result<()> {
        let abs = self.shared.abs(&item.path);
        let Some(disk) = inspect(&abs, self.shared.tag)? else {
            return Err(io::Error::new(io::ErrorKind::NotFound, item.path.clone()));
        };
        match &disk.ph {
            None => return Ok(()),
            Some(p) if !p.partial_on_disk => return Ok(()),
            Some(_) => {}
        }
        // Triggers our own fetch_data callback, which streams on the runtime; the engine calls
        // this on a blocking thread, so nothing here waits for itself.
        let mut ph = Placeholder::from(open_meta(&abs)?);
        ph.hydrate(..).map_err(win_err)?;
        debug!("downloaded {}", item.path);
        Ok(())
    }

    fn set_pinned(&self, item: &LocalItem, pinned: bool) -> io::Result<()> {
        let _op = self.shared.begin_op(&[&item.path]);
        let abs = self.shared.abs(&item.path);
        // Unpinned means "online-only" in Windows (it would be dehydrated again right after every
        // open); without a pin, items are simply "available on demand".
        let state = if pinned {
            PinState::Pinned
        } else {
            PinState::Unspecified
        };
        set_pin(&abs, state, item.kind == Kind::Folder)
    }
}

pub fn set_pin(path: &Path, state: PinState, recurse: bool) -> io::Result<()> {
    let mut ph = Placeholder::from(open_attrs(path)?);
    let mut opts = PinOptions::default();
    if recurse {
        opts.recurse();
    }
    match ph.mark_pin(state, opts) {
        Ok(_) => Ok(()),
        // ERROR_NOT_A_CLOUD_FILE: an ordinary file that hasn't been uploaded yet.
        Err(e) if e.code() == windows::Win32::Foundation::ERROR_NOT_A_CLOUD_FILE.to_hresult() => {
            Ok(())
        }
        Err(e) => Err(win_err(e)),
    }
}
