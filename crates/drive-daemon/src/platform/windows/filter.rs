//! Cloud Files callbacks: serve file contents, approve deletes/renames, forward changes.
//!
//! Callbacks run on Windows' thread pool. Async work is driven on the daemon's Tokio runtime
//! with `Handle::block_on` (these threads are never runtime threads). Nothing here panics;
//! failures are returned as Cloud Files errors so apps get a clear message.

use std::sync::Arc;
use std::time::Duration;

use cloud_filter::error::{CResult, CloudErrorKind};
use cloud_filter::filter::{Request, StateWatchEvent, SyncFilter, info, ticket};
use cloud_filter::utility::WriteAt;
use drive_core::db::NodeId;
use drive_core::engine::EngineError;
use drive_core::remote::RemoteError;
use futures::StreamExt;
use tracing::{debug, error, info, warn};

use super::util::{BlobId, decode_blob};
use super::{Msg, Shared};

/// Transfers are written in pieces this big (a multiple of 4 KiB).
const CHUNK: usize = 1024 * 1024;
/// How far past the requested range we read when Windows says more will be wanted.
const READ_AHEAD: u64 = 32 * 1024 * 1024;
/// Give up if the cloud doesn't answer / send anything for this long (Windows itself gives
/// up after 60 s without a transfer).
const STALL: Duration = Duration::from_secs(45);
const PAGE: u64 = 4096;

pub struct Filter {
    shared: Arc<Shared>,
}

impl Filter {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }

    fn node_id(&self, req: &Request) -> Option<NodeId> {
        match decode_blob(self.shared.tag, req.file_blob()) {
            BlobId::Node(id) => Some(id),
            BlobId::Foreign => {
                let rel = self.shared.rel(&req.path())?;
                self.shared
                    .engine()?
                    .node_by_path(&rel)
                    .ok()
                    .flatten()
                    .map(|n| n.id)
            }
        }
    }

    fn fetch(
        &self,
        req: &Request,
        ticket: &ticket::FetchData,
        info: &info::FetchData,
    ) -> CResult<()> {
        let engine = self
            .shared
            .engine()
            .ok_or(CloudErrorKind::ProviderNotRunning)?;
        let id = self.node_id(req).ok_or_else(|| {
            warn!("no Drive item for {}", req.path().display());
            CloudErrorKind::NotInSync
        })?;
        let size = req.file_size();
        let required = info.required_file_range();
        debug!(
            "{} requested {:?} of {}",
            requester(req),
            required,
            req.path().display()
        );
        let optional = info.optional_file_range();
        let start = required.start / PAGE * PAGE;
        let mut end = required.end;
        if optional.end > end && optional.start <= end {
            let cap = if info.explicit_hydration() {
                u64::MAX
            } else {
                required.end.saturating_add(READ_AHEAD)
            };
            end = optional.end.min(cap);
        }
        end = end
            .div_ceil(PAGE)
            .saturating_mul(PAGE)
            .min(size)
            .max(required.end.min(size));
        if end <= start {
            return Ok(());
        }
        debug!(
            "fetching {} bytes {start}..{end} of {}",
            end - start,
            req.path().display()
        );

        let result: Result<(), CloudErrorKind> = self.shared.rt.block_on(async {
            let stream = tokio::time::timeout(STALL, engine.read(id, start, Some(end - start)))
                .await
                .map_err(|_| CloudErrorKind::NetworkUnavailable)?
                .map_err(|e| cloud_error(&e))?;
            let mut stream = stream;
            let mut buf: Vec<u8> = Vec::with_capacity(CHUNK * 2);
            let mut pos = start;
            loop {
                let next = tokio::time::timeout(STALL, stream.next())
                    .await
                    .map_err(|_| {
                        warn!("download of {} stalled", req.path().display());
                        CloudErrorKind::NetworkUnavailable
                    })?;
                match next {
                    Some(Ok(bytes)) => {
                        let room = (end - pos) as usize - buf.len();
                        buf.extend_from_slice(&bytes[..bytes.len().min(room)]);
                        while buf.len() >= CHUNK {
                            ticket.write_at(&buf[..CHUNK], pos).map_err(|e| {
                                debug!("transfer to {} failed: {e}", req.path().display());
                                CloudErrorKind::Unsuccessful
                            })?;
                            buf.drain(..CHUNK);
                            pos += CHUNK as u64;
                            let _ = ticket.report_progress(size, pos);
                        }
                        if pos + buf.len() as u64 >= end {
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        warn!("download of {} failed: {e}", req.path().display());
                        return Err(CloudErrorKind::NetworkUnavailable);
                    }
                    None => break,
                }
            }
            if pos + (buf.len() as u64) < end {
                warn!(
                    "the cloud sent {} of {} bytes for {}",
                    pos + buf.len() as u64 - start,
                    end - start,
                    req.path().display()
                );
                return Err(CloudErrorKind::Unsuccessful);
            }
            if !buf.is_empty() {
                ticket.write_at(&buf, pos).map_err(|e| {
                    debug!("transfer to {} failed: {e}", req.path().display());
                    CloudErrorKind::Unsuccessful
                })?;
                let _ = ticket.report_progress(size, end);
            }
            Ok(())
        });
        if let Err(kind) = result {
            info!(
                "couldn't download {} for {}: {kind:?}",
                req.path().display(),
                requester(req)
            );
            return Err(kind);
        }
        engine.touch(id);
        // Explicit hydrations come from `LocalSide::hydrate`, whose caller records the download
        // itself (recording it twice would log it twice).
        if self.shared.add_coverage(id, start..end, size) && !info.explicit_hydration() {
            engine.mark_hydrated(id, true);
        }
        Ok(())
    }

    fn changed(&self, paths: Vec<std::path::PathBuf>) {
        self.shared.send(Msg::Paths(paths));
    }
}

fn cloud_error(e: &EngineError) -> CloudErrorKind {
    match e {
        EngineError::Remote(RemoteError::Network(_) | RemoteError::RateLimited) => {
            CloudErrorKind::NetworkUnavailable
        }
        EngineError::Remote(RemoteError::Auth(_)) => CloudErrorKind::AuthenticationFailed,
        EngineError::Remote(RemoteError::DownloadNotAllowed(_)) => CloudErrorKind::AccessDenied,
        EngineError::Remote(RemoteError::Integrity(_)) => CloudErrorKind::ValidationFailed,
        EngineError::NotFound(_) | EngineError::Remote(RemoteError::NotFound(_)) => {
            CloudErrorKind::NotInSync
        }
        _ => CloudErrorKind::Unsuccessful,
    }
}

/// Which program asked for a file ("Explorer (4312)"), for the log.
fn requester(req: &Request) -> String {
    match req.process() {
        Some(p) => {
            let name = p
                .path()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| "a program".into());
            format!("{name} ({})", p.id())
        }
        None => "a program".into(),
    }
}

impl SyncFilter for Filter {
    fn fetch_data(
        &self,
        request: Request,
        ticket: ticket::FetchData,
        info: info::FetchData,
    ) -> CResult<()> {
        self.fetch(&request, &ticket, &info)
    }

    fn cancel_fetch_data(&self, request: Request, info: info::CancelFetchData) {
        debug!(
            "download of {} cancelled ({:?})",
            request.path().display(),
            info.file_range()
        );
    }

    fn dehydrate(
        &self,
        request: Request,
        ticket: ticket::Dehydrate,
        _info: info::Dehydrate,
    ) -> CResult<()> {
        // Windows only asks for in-sync files; their content is safely in the cloud.
        debug!("allowing Windows to free up {}", request.path().display());
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)
    }

    fn dehydrated(&self, request: Request, _info: info::Dehydrated) {
        if let Some(id) = self.node_id(&request) {
            self.shared.clear_coverage(id);
            if let Some(engine) = self.shared.engine() {
                engine.mark_hydrated(id, false);
            }
        }
    }

    fn delete(&self, request: Request, ticket: ticket::Delete, info: info::Delete) -> CResult<()> {
        debug!(
            "allowing delete{} of {}",
            if info.is_undelete() { " (undo)" } else { "" },
            request.path().display()
        );
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)?;
        self.changed(vec![request.path()]);
        Ok(())
    }

    fn deleted(&self, request: Request, _info: info::Deleted) {
        self.changed(vec![request.path()]);
    }

    fn rename(&self, request: Request, ticket: ticket::Rename, info: info::Rename) -> CResult<()> {
        debug!(
            "allowing move of {} to {}",
            request.path().display(),
            info.target_path().display()
        );
        ticket.pass().map_err(|_| CloudErrorKind::Unsuccessful)?;
        self.changed(vec![request.path(), info.target_path()]);
        Ok(())
    }

    fn renamed(&self, request: Request, info: info::Renamed) {
        self.changed(vec![info.source_path(), request.path()]);
    }

    fn closed(&self, request: Request, _info: info::Closed) {
        // Content changes are reported by the watcher too; this catches them a bit earlier.
        self.changed(vec![request.path()]);
    }

    fn state_changed(&self, event: StateWatchEvent) {
        match event {
            StateWatchEvent::Changed(paths) => self.changed(paths),
            StateWatchEvent::Stopped(e) => {
                error!("watching the Drive folder stopped: {e}");
                self.shared.send(Msg::WatcherStopped);
            }
        }
    }
}
