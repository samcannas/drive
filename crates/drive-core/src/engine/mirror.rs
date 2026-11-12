//! Planning for **mirror** engines (folder backups): the folder on this device is the source of
//! truth and the cloud copy follows it, one way.
//!
//! Unlike [`super::plan`], nothing here ever changes the local side. Whatever differs in the
//! cloud is put back in line with the local rows:
//!
//! | Cloud state | Result |
//! |---|---|
//! | Item we uploaded, still there | nothing (or moved back if someone moved it: [`Change::Rebase`]) |
//! | Item we uploaded, gone | upload it again ([`Change::Resurrect`]) |
//! | Unknown item where an identical, not yet uploaded local file is | adopt it, no upload ([`Change::Link`]) |
//! | Any other unknown item inside a backed-up folder | remove it from the copy ([`Change::TrashRemote`]) |
//! | Anything under a top-level folder that isn't backed up (any more) | left alone |
//!
//! The adoption rule is what makes "switch a backup off and on again" and "reinstall Drive"
//! cheap: files already in the cloud with the same size and modification time are recognised
//! instead of being uploaded again.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::db::{Node, NodeId, ROOT_ID, name_key};
use crate::remote::{Kind, RemoteEntry, RemoteTree};

use super::plan::{Change, Plan, PlanError};

/// Modification times closer than this count as equal (FAT/exFAT store 2-second steps).
pub const MTIME_TOLERANCE_MS: i64 = 2_000;

/// Removing at least this many files from the cloud copy, and at least a quarter of it, in one
/// scan needs the user's confirmation (the local side may simply be unreadable right now).
pub const MASS_TRASH_FILES: u32 = 200;

pub struct MirrorInput<'a> {
    pub nodes: &'a [Node],
    pub tree: &'a RemoteTree,
    /// Like [`super::plan::PlanInput::scan_seq`].
    pub scan_seq: i64,
    pub orphans: &'a HashSet<String>,
    /// Like [`super::plan::PlanInput::busy`].
    pub busy: &'a HashSet<NodeId>,
    /// Name keys ([`name_key`]) of the top-level folders that are backed up. Cloud items under
    /// other top-level folders are left alone.
    pub roots: &'a HashSet<String>,
    pub allow_mass_removal: bool,
}

pub fn plan_mirror(input: &MirrorInput<'_>) -> Result<Plan, PlanError> {
    let tree = input.tree;
    if tree.entries.is_empty() && !tree.skipped.is_empty() {
        return Err(PlanError::WrongPassword(tree.skipped.len()));
    }
    let mut out = Plan::default();

    let nodes: HashMap<NodeId, &Node> = input.nodes.iter().map(|n| (n.id, n)).collect();
    let mut by_remote: HashMap<&str, NodeId> = HashMap::new();
    let mut children: HashMap<(NodeId, String), NodeId> = HashMap::new();
    for n in input.nodes {
        if let Some(r) = n.remote_id.as_deref() {
            by_remote.insert(r, n.id);
        }
        if let Some(p) = n.parent_id
            && !n.deleted
        {
            children.insert((p, name_key(&n.name)), n.id);
        }
    }
    let remote: HashMap<&str, &RemoteEntry> = tree
        .entries
        .iter()
        .filter(|e| !input.orphans.contains(&e.id))
        .map(|e| (e.id.as_str(), e))
        .collect();
    let mut kids: HashMap<&str, Vec<&RemoteEntry>> = HashMap::new();
    for e in remote.values() {
        kids.entry(e.parent_id.as_str()).or_default().push(e);
    }
    for v in kids.values_mut() {
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    }
    let file_count = |root: &RemoteEntry| -> u32 {
        if root.kind == Kind::File {
            return 1;
        }
        let mut count = 0;
        let mut stack = vec![root.id.as_str()];
        while let Some(id) = stack.pop() {
            for c in kids.get(id).into_iter().flatten() {
                match c.kind {
                    Kind::File => count += 1,
                    Kind::Folder => stack.push(c.id.as_str()),
                }
            }
        }
        count
    };

    if nodes.get(&ROOT_ID).and_then(|r| r.remote_id.as_deref()) != Some(tree.root_id.as_str()) {
        out.changes
            .push(Change::SetRootRemote(tree.root_id.clone()));
    }

    let unsettled = |n: &Node| input.busy.contains(&n.id) || n.op_seq > input.scan_seq;
    let mut trash: Vec<(RemoteEntry, u32)> = Vec::new();
    let mut managed_files = 0u32;
    let mut linked: HashSet<NodeId> = HashSet::new();

    let mut queue: VecDeque<(&str, NodeId)> = VecDeque::from([(tree.root_id.as_str(), ROOT_ID)]);
    while let Some((remote_parent, local_parent)) = queue.pop_front() {
        let top = remote_parent == tree.root_id;
        for e in kids.get(remote_parent).cloned().unwrap_or_default() {
            if top && !input.roots.contains(&name_key(&e.name)) {
                continue; // not (or no longer) backed up: leave the cloud copy alone
            }
            if e.kind == Kind::File {
                managed_files += 1;
            }
            if let Some(&id) = by_remote.get(e.id.as_str()) {
                let n = nodes[&id];
                if n.kind != e.kind || n.deleted {
                    continue;
                }
                if unsettled(n) {
                    if e.kind == Kind::Folder {
                        queue.push_back((e.id.as_str(), id));
                    }
                    continue;
                }
                let moved = n.remote_parent_id.as_deref() != Some(e.parent_id.as_str())
                    || n.remote_name.as_deref() != Some(e.name.as_str());
                if moved {
                    // Someone moved our copy; the local location wins.
                    out.changes.push(Change::Rebase {
                        node: id,
                        entry: e.clone(),
                    });
                }
                if e.kind == Kind::Folder {
                    queue.push_back((e.id.as_str(), id));
                }
                continue;
            }

            // An item we don't know. Adopt it if it matches a local item that isn't uploaded
            // yet; otherwise it doesn't belong in the copy.
            let local = children
                .get(&(local_parent, name_key(&e.name)))
                .map(|id| nodes[id]);
            match local {
                Some(x) if unsettled(x) => {} // probably our own upload finishing; next scan
                Some(x)
                    if x.kind == e.kind
                        && x.remote_id.is_none()
                        && !linked.contains(&x.id)
                        && (e.kind == Kind::Folder
                            || (x.size == e.size
                                && (x.mtime_ms - e.mtime_ms).abs() <= MTIME_TOLERANCE_MS)) =>
                {
                    linked.insert(x.id);
                    out.changes.push(Change::Link {
                        node: x.id,
                        generation: x.generation,
                        entry: e.clone(),
                    });
                    if e.kind == Kind::Folder {
                        queue.push_back((e.id.as_str(), x.id));
                    }
                }
                _ => {
                    let files = file_count(e);
                    trash.push((e.clone(), files));
                }
            }
        }
    }

    // Our uploads that disappeared from the cloud: upload them again.
    for n in input.nodes {
        if n.id == ROOT_ID || unsettled(n) {
            continue;
        }
        let Some(r) = n.remote_id.as_deref() else {
            continue;
        };
        if remote.contains_key(r) || input.orphans.contains(r) {
            continue;
        }
        if n.deleted {
            out.changes.push(Change::DropTombstone { node: n.id });
        } else {
            out.changes.push(Change::Resurrect {
                node: n.id,
                generation: n.generation,
            });
        }
    }

    let trash_files: u32 = trash.iter().map(|(_, f)| *f).sum();
    if !input.allow_mass_removal
        && trash_files >= MASS_TRASH_FILES
        && trash_files.saturating_mul(4) >= managed_files
    {
        out.held_removal_files = trash_files;
    } else {
        out.changes.extend(
            trash
                .into_iter()
                .map(|(entry, _)| Change::TrashRemote { entry }),
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(
        id: NodeId,
        parent: NodeId,
        name: &str,
        kind: Kind,
        remote: Option<(&str, &str)>,
    ) -> Node {
        Node {
            id,
            parent_id: Some(parent),
            name: name.into(),
            kind,
            size: 10,
            mtime_ms: 1_000_000,
            remote_id: remote.map(|(id, _)| id.to_string()),
            remote_parent_id: remote.map(|(_, p)| p.to_string()),
            remote_name: remote.map(|_| name.to_string()),
            dirty: false,
            deleted: false,
            pending: 0,
            generation: 0,
            op_seq: 0,
            changed_ms: 0,
            retry_at_ms: 0,
            attempts: 0,
            hydrated: true,
            pinned: false,
            accessed_ms: 0,
            upload_mode: 0,
        }
    }

    fn root() -> Node {
        let mut r = node(ROOT_ID, 0, "", Kind::Folder, Some(("R", "")));
        r.parent_id = None;
        r
    }

    fn entry(id: &str, parent: &str, name: &str, kind: Kind) -> RemoteEntry {
        RemoteEntry {
            id: id.into(),
            parent_id: parent.into(),
            kind,
            name: name.into(),
            size: 10,
            mtime_ms: 1_000_000,
        }
    }

    fn run(nodes: &[Node], entries: Vec<RemoteEntry>, roots: &[&str]) -> Plan {
        let tree = RemoteTree {
            root_id: "R".into(),
            entries,
            skipped: vec![],
        };
        let roots: HashSet<String> = roots.iter().map(|r| name_key(r)).collect();
        plan_mirror(&MirrorInput {
            nodes,
            tree: &tree,
            scan_seq: 10,
            orphans: &HashSet::new(),
            busy: &HashSet::new(),
            roots: &roots,
            allow_mass_removal: false,
        })
        .unwrap()
    }

    fn unsynced(id: NodeId, parent: NodeId, name: &str, kind: Kind) -> Node {
        let mut n = node(id, parent, name, kind, None);
        n.pending = crate::db::pending::PUSH;
        n.dirty = kind == Kind::File;
        n
    }

    #[test]
    fn adopts_identical_files_and_folders() {
        let nodes = [
            root(),
            unsynced(2, 1, "Documents", Kind::Folder),
            unsynced(3, 2, "a.txt", Kind::File),
        ];
        let p = run(
            &nodes,
            vec![
                entry("D", "R", "Documents", Kind::Folder),
                entry("f", "D", "a.txt", Kind::File),
            ],
            &["Documents"],
        );
        assert!(
            matches!(
                &p.changes[..],
                [Change::Link { node: 2, .. }, Change::Link { node: 3, .. }]
            ),
            "{:?}",
            p.changes
        );
    }

    #[test]
    fn stale_or_unknown_copies_are_trashed() {
        let mut changed = unsynced(3, 2, "a.txt", Kind::File);
        changed.size = 99;
        let mut docs = node(2, 1, "Documents", Kind::Folder, Some(("D", "R")));
        docs.remote_name = Some("Documents".into());
        let nodes = [root(), docs, changed];
        let p = run(
            &nodes,
            vec![
                entry("D", "R", "Documents", Kind::Folder),
                entry("f", "D", "a.txt", Kind::File),
                entry("g", "D", "gone-locally.txt", Kind::File),
            ],
            &["Documents"],
        );
        let trashed: Vec<_> = p
            .changes
            .iter()
            .filter_map(|c| match c {
                Change::TrashRemote { entry } => Some(entry.id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(trashed, ["f", "g"]);
    }

    #[test]
    fn unmanaged_top_level_folders_are_left_alone() {
        let p = run(
            &[root()],
            vec![
                entry("V", "R", "Videos", Kind::Folder),
                entry("v", "V", "clip.mp4", Kind::File),
            ],
            &["Documents"],
        );
        assert!(p.changes.is_empty(), "{:?}", p.changes);
    }

    #[test]
    fn deleted_or_moved_copies_are_repaired() {
        let docs = node(2, 1, "Documents", Kind::Folder, Some(("D", "R")));
        let a = node(3, 2, "a.txt", Kind::File, Some(("f", "D")));
        let b = node(4, 2, "b.txt", Kind::File, Some(("g", "D")));
        let p = run(
            &[root(), docs, a, b],
            vec![
                entry("D", "R", "Documents", Kind::Folder),
                // a.txt deleted in the cloud; b.txt renamed there.
                entry("g", "D", "renamed.txt", Kind::File),
            ],
            &["Documents"],
        );
        assert!(p.changes.contains(&Change::Resurrect {
            node: 3,
            generation: 0
        }));
        assert!(
            p.changes
                .iter()
                .any(|c| matches!(c, Change::Rebase { node: 4, .. }))
        );
    }

    #[test]
    fn many_unknown_files_are_held() {
        let docs = node(2, 1, "Documents", Kind::Folder, Some(("D", "R")));
        let mut entries = vec![entry("D", "R", "Documents", Kind::Folder)];
        for i in 0..300 {
            entries.push(entry(&format!("x{i}"), "D", &format!("f{i}"), Kind::File));
        }
        let p = run(&[root(), docs], entries, &["Documents"]);
        assert_eq!(p.held_removal_files, 300);
        assert!(
            !p.changes
                .iter()
                .any(|c| matches!(c, Change::TrashRemote { .. }))
        );
    }

    #[test]
    fn in_flight_items_are_not_judged() {
        let mut x = unsynced(3, 2, "a.txt", Kind::File);
        x.op_seq = 11;
        let mut docs = node(2, 1, "Documents", Kind::Folder, Some(("D", "R")));
        docs.remote_name = Some("Documents".into());
        let p = run(
            &[root(), docs, x],
            vec![
                entry("D", "R", "Documents", Kind::Folder),
                entry("new", "D", "a.txt", Kind::File),
            ],
            &["Documents"],
        );
        assert!(p.changes.is_empty(), "{:?}", p.changes);
    }
}
