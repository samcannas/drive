//! The transfer list: what is moving, what is waiting and why, how long it will take, and the
//! user's per-file controls (pause, upload tonight, upload now).

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::{Engine, EngineError, Result};
use crate::db::{Node, NodeId, UploadGate, now_ms, upload_mode};
use crate::ipc::{
    Event, TransferDirection, TransferItem, TransferList, TransferMode, TransferState,
};
use crate::remote::resumable::UploadPhase;

/// Most items in one list.
const MAX_ITEMS: u32 = 200;
/// Weight of the newest sample in the smoothed rate.
const RATE_ALPHA: f64 = 0.3;

pub(super) fn mode_of(v: i64) -> TransferMode {
    match v {
        upload_mode::PAUSED => TransferMode::Paused,
        upload_mode::TONIGHT => TransferMode::Tonight,
        upload_mode::NOW => TransferMode::Now,
        _ => TransferMode::Auto,
    }
}

fn mode_value(m: TransferMode) -> i64 {
    match m {
        TransferMode::Auto => upload_mode::AUTO,
        TransferMode::Paused => upload_mode::PAUSED,
        TransferMode::Tonight => upload_mode::TONIGHT,
        TransferMode::Now => upload_mode::NOW,
    }
}

/// "in 30 s", "in 4 min", "in 2 h".
fn in_time(ms: i64) -> String {
    let s = (ms.max(0) + 999) / 1000;
    if s < 60 {
        format!("in {s} s")
    } else if s < 3600 {
        format!("in {} min", (s + 59) / 60)
    } else {
        format!("in {} h", s / 3600)
    }
}

/// Plain-language reason for a failed attempt, from the stored error text.
fn failure_reason(err: &str, retry_in_ms: i64) -> String {
    let e = err.to_ascii_lowercase();
    let when = in_time(retry_in_ms);
    if e.contains("open in another app") {
        "Open in another app. Drive uploads it once it's closed.".into()
    } else if e.contains("changed while uploading") {
        format!("The file is still changing. Trying again {when}.")
    } else if e.contains("rate limit") || e.contains("slow down") {
        format!("uloz.to asked Drive to slow down. Trying again {when}.")
    } else if e.contains("network")
        || e.contains("connect")
        || e.contains("interrupted")
        || e.contains("stalled")
    {
        format!("Connection problem. Trying again {when}.")
    } else if e.contains("quota") || e.contains("storage") {
        "Your uloz.to storage is full.".into()
    } else {
        format!("Upload failed. Trying again {when}.")
    }
}

impl Engine {
    /// Which uploads may start now, given the overnight window.
    pub(super) fn upload_gate(&self) -> UploadGate {
        let cfg = self.cfg.read().unwrap();
        match cfg.night {
            Some(w) if !w.is_open() => UploadGate {
                night_closed: true,
                // Without automatic holding, no size qualifies. (i64::MAX, not u64::MAX: the
                // database takes it as a signed integer, where u64::MAX would be -1 and hold
                // every file.)
                night_min_bytes: if cfg.night_auto {
                    cfg.night_min_bytes
                } else {
                    i64::MAX as u64
                },
            },
            _ => UploadGate::default(),
        }
    }

    /// Whether this file's upload is held now: paused, held for tonight, or (with automatic
    /// overnight uploads) large, while the night window is closed. Same rule as the queue's.
    pub(super) fn held(&self, n: &Node) -> bool {
        let gate = self.upload_gate();
        match n.upload_mode {
            upload_mode::PAUSED => true,
            upload_mode::TONIGHT => gate.night_closed,
            upload_mode::AUTO => gate.night_closed && n.size >= gate.night_min_bytes,
            _ => false,
        }
    }

    /// Pause, resume ("auto"), defer to tonight, or force one upload.
    pub fn set_transfer_mode(&self, id: NodeId, mode: TransferMode) -> Result<()> {
        if !self.db.set_upload_mode(id, mode_value(mode))? {
            return Err(EngineError::NotFound(format!("transfer {id}")));
        }
        let held = match mode {
            TransferMode::Paused => true,
            TransferMode::Tonight => self.upload_gate().night_closed,
            _ => false,
        };
        if held && let Some(c) = self.upload_cancels.lock().unwrap().get(&id) {
            // Stops at the next chunk; the confirmed part stays on the server.
            c.cancel();
        }
        self.wake_worker.notify_one();
        self.status_changed.notify_one();
        Ok(())
    }

    /// Updates the smoothed rate of every running transfer (called about once a second).
    pub(super) fn sample_rates(&self) {
        let now = Instant::now();
        for t in self.transfers.lock().unwrap().values_mut() {
            let dt = now.duration_since(t.last_at).as_secs_f64();
            if dt < 0.5 {
                continue;
            }
            let done = t.progress.done();
            let inst = done.saturating_sub(t.last_done) as f64 / dt;
            t.rate = if t.rate == 0.0 {
                inst
            } else {
                RATE_ALPHA * inst + (1.0 - RATE_ALPHA) * t.rate
            };
            t.last_done = done;
            t.last_at = now;
        }
    }

    /// Sends the list to subscribers while anything is transferring or queued (and once more
    /// when it empties), at most about once a second.
    pub(super) fn push_transfers(&self) {
        let busy =
            !self.transfers.lock().unwrap().is_empty() || self.db.pending_count().unwrap_or(0) > 0;
        let mut pushed = self.transfers_pushed.lock().unwrap();
        if !busy && !pushed.1 {
            return;
        }
        if busy && pushed.1 && pushed.0.elapsed().as_millis() < 900 {
            return;
        }
        let list = self.transfers();
        *pushed = (Instant::now(), !list.items.is_empty());
        drop(pushed);
        let _ = self.events.send(Event::Transfers(list));
    }

    /// Everything that is uploading, downloading, queued or held.
    pub fn transfers(&self) -> TransferList {
        let cfg = self.config();
        let gate = self.upload_gate();
        let now = now_ms();
        let debounce = cfg.debounce.as_millis() as i64;
        let mut list = TransferList::default();
        let mut active_nodes = HashSet::new();
        let mut items = Vec::new();

        // Running transfers.
        for t in self.transfers.lock().unwrap().values() {
            active_nodes.insert((t.node, t.upload));
            let done = t.progress.done().min(t.total);
            let rate = t.rate.max(0.0) as u64;
            let (state, reason) = match (t.upload, t.progress.phase()) {
                (true, UploadPhase::Preparing) => (
                    TransferState::Preparing,
                    Some("Checking the file before uploading".to_string()),
                ),
                (true, UploadPhase::Verifying) => (
                    TransferState::Verifying,
                    Some("uloz.to is checking the upload".to_string()),
                ),
                _ => (TransferState::Active, None),
            };
            if t.upload {
                list.upload_rate_bps += rate;
                list.upload_remaining_bytes += t.total - done;
            } else {
                list.download_rate_bps += rate;
            }
            items.push(TransferItem {
                id: t.node,
                path: t.path.clone(),
                direction: if t.upload {
                    TransferDirection::Upload
                } else {
                    TransferDirection::Download
                },
                state,
                reason,
                bytes_done: done,
                bytes_total: t.total,
                rate_bps: rate,
                eta_secs: (rate > 0).then(|| (t.total - done).div_ceil(rate)),
                retry_at_ms: None,
                resumable: t.resumable
                    || (t.upload
                        && self
                            .remote
                            .resumable_min_size()
                            .is_some_and(|m| t.total >= m)),
                mode: TransferMode::Auto,
            });
        }

        // Queued and held uploads.
        let (queued, total) = self.db.queued_uploads(MAX_ITEMS, gate).unwrap_or_default();
        let in_flight = self.in_flight.lock().unwrap().clone();
        let night_label = cfg.night.map(|w| w.start_label());
        for n in queued {
            if active_nodes.contains(&(n.id, true)) {
                continue;
            }
            let path = self
                .db
                .path_of(n.id)
                .ok()
                .flatten()
                .unwrap_or_else(|| n.name.clone());
            let (bytes_done, resumable) = match self.db.upload_session(n.id).ok().flatten() {
                Some(s) => {
                    let done = self
                        .remote
                        .resume_info(&s.state)
                        .filter(|i| i.total > 0)
                        .map(|i| (i.confirmed as f64 * n.size as f64 / i.total as f64) as u64)
                        .unwrap_or(0);
                    (done.min(n.size), true)
                }
                None => (0, false),
            };
            let (state, reason, retry_at) =
                self.queued_state(&n, gate, now, debounce, &in_flight, night_label.as_deref());
            if !matches!(state, TransferState::Paused | TransferState::Scheduled) {
                list.upload_remaining_bytes += n.size.saturating_sub(bytes_done);
            }
            items.push(TransferItem {
                id: n.id,
                path,
                direction: TransferDirection::Upload,
                state,
                reason,
                bytes_done,
                bytes_total: n.size,
                rate_bps: 0,
                eta_secs: None,
                retry_at_ms: retry_at,
                resumable,
                mode: mode_of(n.upload_mode),
            });
        }

        let order = |s: TransferState| match s {
            TransferState::Active | TransferState::Preparing | TransferState::Verifying => 0,
            TransferState::Queued | TransferState::Waiting => 1,
            TransferState::Paused | TransferState::Scheduled => 2,
        };
        items.sort_by_key(|i| (order(i.state), i.direction == TransferDirection::Download));
        list.total_count = (total as usize).max(items.len()) as u32
            + items
                .iter()
                .filter(|i| i.direction == TransferDirection::Download)
                .count() as u32;
        items.truncate(MAX_ITEMS as usize);
        list.items = items;
        if list.upload_rate_bps > 0 && list.upload_remaining_bytes > 0 {
            list.upload_eta_secs = Some(list.upload_remaining_bytes.div_ceil(list.upload_rate_bps));
        }
        list.night_window_open = cfg.night.is_some() && !gate.night_closed;
        let speed = self.upload_speed();
        list.auto_upload_limit_bps = speed.limit;
        list.auto_upload_checking = speed.mode == super::adaptive::Mode::Checking;
        if let Some(w) = cfg.night.filter(|_| gate.night_closed) {
            list.night_window_opens_ms =
                Some(w.next_open_at(&chrono::Local::now()).timestamp_millis());
        }
        list
    }

    fn queued_state(
        &self,
        n: &Node,
        gate: UploadGate,
        now: i64,
        debounce: i64,
        in_flight: &HashSet<NodeId>,
        night: Option<&str>,
    ) -> (TransferState, Option<String>, Option<i64>) {
        let tonight = |why: &str| {
            let at = night.map(|t| format!(" ({t})")).unwrap_or_default();
            (TransferState::Scheduled, Some(format!("{why}{at}")), None)
        };
        match n.upload_mode {
            upload_mode::PAUSED => return (TransferState::Paused, Some("Paused".into()), None),
            upload_mode::TONIGHT if gate.night_closed => return tonight("Waiting for tonight"),
            upload_mode::AUTO if gate.night_closed && n.size >= gate.night_min_bytes => {
                return tonight("Large file, waiting for tonight");
            }
            _ => {}
        }
        let waiting = |r: &str| (TransferState::Waiting, Some(r.to_string()), None);
        if self.signed_out.load(Ordering::Relaxed) {
            return waiting("Sign in to uloz.to to keep uploading");
        }
        if self.paused.load(Ordering::Relaxed) {
            return (
                TransferState::Paused,
                Some("Syncing is paused".into()),
                None,
            );
        }
        if self.offline.load(Ordering::Relaxed) {
            return waiting("Offline. Uploads continue when you're back online.");
        }
        if n.retry_at_ms > now {
            let err = self
                .db
                .read(|c| {
                    c.query_row(
                        "SELECT COALESCE(last_error, '') FROM node WHERE id = ?1",
                        [n.id],
                        |r| r.get::<_, String>(0),
                    )
                })
                .unwrap_or_default();
            return (
                TransferState::Waiting,
                Some(failure_reason(&err, n.retry_at_ms - now)),
                Some(n.retry_at_ms),
            );
        }
        if n.changed_ms > now - debounce {
            return waiting("Waiting for the file to finish changing");
        }
        if self.parent_remote_id(n).ok().flatten().is_none() {
            return waiting("Waiting for its folder to be created");
        }
        if in_flight.contains(&n.id) {
            return (TransferState::Queued, Some("Starting".into()), None);
        }
        (
            TransferState::Queued,
            Some("Waiting for other uploads to finish".into()),
            None,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_plain() {
        assert_eq!(in_time(1_200), "in 2 s");
        assert_eq!(in_time(90_000), "in 2 min");
        assert!(
            failure_reason("network error: error sending request", 30_000)
                .starts_with("Connection problem")
        );
        assert!(
            failure_reason("x.bin is open in another app", 1).starts_with("Open in another app")
        );
        assert!(failure_reason("weird", 5_000).contains("in 5 s"));
    }
}
