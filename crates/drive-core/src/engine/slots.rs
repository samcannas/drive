//! How many files upload at once, shared by the Drive folder's engine and the engines linked to
//! it (folder backups). The setting means "files at once" for everything together: a backup
//! uses every slot while the Drive folder has nothing to upload, and a Drive-folder upload
//! that's waiting gets the next free slot before any backup.

use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

pub struct UploadSlots {
    state: Mutex<State>,
    notify: Notify,
}

struct State {
    limit: usize,
    in_use: usize,
    /// Drive-folder uploads waiting for a slot (they go first).
    priority_waiting: usize,
}

impl UploadSlots {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                limit: limit.max(1),
                in_use: 0,
                priority_waiting: 0,
            }),
            notify: Notify::new(),
        })
    }

    pub fn set_limit(&self, limit: usize) {
        self.state.lock().unwrap().limit = limit.max(1);
        self.notify.notify_waiters();
    }

    pub fn in_use(&self) -> usize {
        self.state.lock().unwrap().in_use
    }

    /// Waits for a slot; `priority` uploads (the Drive folder's) are served first.
    pub async fn acquire(self: &Arc<Self>, priority: bool) -> SlotGuard {
        let mut counted = false;
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut s = self.state.lock().unwrap();
                if priority && !counted {
                    s.priority_waiting += 1;
                    counted = true;
                }
                if s.in_use < s.limit && (priority || s.priority_waiting == 0) {
                    s.in_use += 1;
                    if counted {
                        s.priority_waiting -= 1;
                    }
                    return SlotGuard {
                        slots: self.clone(),
                    };
                }
            }
            // Dropped while waiting (cancelled): no longer waiting.
            let _undo = WaitGuard {
                slots: self,
                counted,
            };
            notified.await;
            std::mem::forget(_undo);
        }
    }
}

/// Undoes the waiting count if an `acquire` is dropped while it waits.
struct WaitGuard<'a> {
    slots: &'a UploadSlots,
    counted: bool,
}

impl Drop for WaitGuard<'_> {
    fn drop(&mut self) {
        if self.counted {
            self.slots.state.lock().unwrap().priority_waiting -= 1;
            self.slots.notify.notify_waiters();
        }
    }
}

/// A slot in use; freed when dropped.
pub struct SlotGuard {
    slots: Arc<UploadSlots>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.slots.state.lock().unwrap().in_use -= 1;
        self.slots.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn try_acquire(s: &Arc<UploadSlots>, priority: bool) -> Option<SlotGuard> {
        tokio::time::timeout(Duration::from_millis(50), s.acquire(priority))
            .await
            .ok()
    }

    #[tokio::test]
    async fn backups_use_every_slot_when_the_drive_folder_is_idle() {
        let s = UploadSlots::new(4);
        let mut held = Vec::new();
        for _ in 0..4 {
            held.push(try_acquire(&s, false).await.expect("free slot"));
        }
        assert!(try_acquire(&s, false).await.is_none(), "only 4 at once");
        drop(held.pop());
        assert!(try_acquire(&s, false).await.is_some());
    }

    #[tokio::test]
    async fn a_waiting_drive_folder_upload_goes_before_backups() {
        let s = UploadSlots::new(1);
        let first = s.acquire(false).await;
        let s2 = s.clone();
        let drive = tokio::spawn(async move { s2.acquire(true).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        // A backup asking now must wait behind the Drive-folder upload.
        let s3 = s.clone();
        let backup = tokio::spawn(async move { s3.acquire(false).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(first);
        let d = tokio::time::timeout(Duration::from_secs(1), drive)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !backup.is_finished(),
            "backup waits while the slot is taken"
        );
        drop(d);
        tokio::time::timeout(Duration::from_secs(1), backup)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_cancelled_wait_doesnt_block_backups() {
        let s = UploadSlots::new(1);
        let first = s.acquire(false).await;
        // A Drive-folder upload starts waiting, then is cancelled.
        assert!(try_acquire(&s, true).await.is_none());
        drop(first);
        assert!(try_acquire(&s, false).await.is_some());
    }

    #[tokio::test]
    async fn a_larger_limit_lets_waiting_uploads_start() {
        let s = UploadSlots::new(1);
        let _a = s.acquire(false).await;
        let s2 = s.clone();
        let b = tokio::spawn(async move { s2.acquire(false).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        s.set_limit(2);
        let _b = tokio::time::timeout(Duration::from_secs(1), b)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(s.in_use(), 2);
    }
}
