//! System notifications, only for issues the user has to act on. Routine progress never
//! notifies.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};

use drive_core::ipc::{Call, Issue, IssueKind};
use tauri_plugin_notification::NotificationExt;

use crate::backend::Backend;

fn seen() -> &'static Mutex<HashSet<i64>> {
    static SEEN: OnceLock<Mutex<HashSet<i64>>> = OnceLock::new();
    SEEN.get_or_init(Default::default)
}

fn title(kind: IssueKind) -> Option<&'static str> {
    Some(match kind {
        IssueKind::Auth => "Sign in to keep syncing",
        IssueKind::MassDelete => "Confirm deleting files from the cloud",
        IssueKind::WrongPassword => "Enter your encryption password",
        IssueKind::DiskFull => "Your disk is almost full",
        _ => return None,
    })
}

pub async fn check_issues(backend: &Arc<Backend>) {
    let Ok(issues) = backend.call::<Vec<Issue>>(Call::ListIssues).await else {
        return;
    };
    let enabled = backend.cached_settings().is_none_or(|s| s.notifications);
    let fresh: Vec<Issue> = {
        let mut seen = seen().lock().unwrap();
        // Forget resolved issues so a recurrence notifies again.
        seen.retain(|id| issues.iter().any(|i| i.id == *id));
        issues
            .into_iter()
            .filter(|i| title(i.kind).is_some() && seen.insert(i.id))
            .collect()
    };
    if !enabled {
        return;
    }
    for issue in fresh {
        let result = backend
            .app
            .notification()
            .builder()
            .title(title(issue.kind).unwrap_or("Drive"))
            .body(&issue.message)
            .show();
        if let Err(e) = result {
            tracing::warn!("could not show a notification: {e}");
        }
    }
}
