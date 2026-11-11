//! Pure planning of how to fold a remote scan into the local state.
//!
//! The planner works on an in-memory model of the `node` table and never touches the disk or
//! the network, which makes the subtle cases (moves, replacements, conflicts, deletions with
//! local edits, duplicate names) easy to test. The engine applies the resulting [`Change`]s.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::db::{Node, NodeId, ROOT_ID, name_key, pending};
use crate::remote::{Kind, RemoteEntry, RemoteTree};

/// New nodes get negative temporary ids until they are inserted.
pub type TempId = i64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// Record the vault's root folder id.
    SetRootRemote(String),
    /// A new remote item: insert a node and show it locally.
    Create { temp_id: TempId, parent: NodeId, name: String, entry: RemoteEntry, needs_move: bool, pinned: bool },
    /// The item moved/was renamed remotely: move it locally too.
    Move { node: NodeId, generation: i64, parent: NodeId, name: String, entry: RemoteEntry, needs_move: bool },
    /// Remote location changed but local intent wins (pending local move or delete).
    Rebase { node: NodeId, entry: RemoteEntry },
    /// A clean file got new content remotely.
    Replace { node: NodeId, generation: i64, entry: RemoteEntry },
    /// A local folder (not yet created remotely, or recreated) matched to a remote folder.
    Adopt { node: NodeId, generation: i64, entry: RemoteEntry },
    /// Rename a local item out of the way of a remote one. `detach` forgets the remote copy
    /// (it no longer exists), so the item is uploaded as a new file under the new name.
    MoveAside { node: NodeId, generation: i64, name: String, detach: bool },
    /// Remote deletion of an item without local changes: remove it (and its subtree).
    Remove { node: NodeId, generation: i64 },
    /// Remote deletion of an item with local changes: keep it and upload it again.
    Resurrect { node: NodeId, generation: i64 },
    /// A locally deleted item is gone remotely too: drop the tombstone.
    DropTombstone { node: NodeId },
}

#[derive(Debug, Default)]
pub struct Plan {
    pub changes: Vec<Change>,
    /// Files that would be removed locally but were held back by the mass-deletion guard.
    pub held_removal_files: u32,
    /// Remote entries skipped because their names can't exist on this platform.
    pub invalid_names: Vec<RemoteEntry>,
    /// Names of local items renamed because of a conflict.
    pub conflicts: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PlanError {
    #[error("none of the {0} items in the vault can be decrypted with the saved password")]
    WrongPassword(usize),
}

pub struct PlanInput<'a> {
    pub nodes: &'a [Node],
    pub tree: &'a RemoteTree,
    /// Operation sequence number when the scan started; nodes our worker touched after that
    /// are left alone (the listing may predate the change).
    pub scan_seq: i64,
    pub orphans: &'a HashSet<String>,
    pub valid_name: &'a dyn Fn(&str) -> bool,
    pub device_name: &'a str,
    /// Date stamp for conflict names, e.g. `2026-11-10 1432`.
    pub stamp: &'a str,
    /// Apply large remote deletions without holding them back.
    pub allow_mass_removal: bool,
}

#[derive(Debug, Clone)]
struct MNode {
    parent: Option<NodeId>,
    name: String,
    kind: Kind,
    remote_id: Option<String>,
    base_parent: Option<String>,
    base_name: Option<String>,
    dirty: bool,
    deleted: bool,
    pending: i64,
    generation: i64,
    op_seq: i64,
    pinned: bool,
}

struct Model {
    nodes: HashMap<NodeId, MNode>,
    by_name: HashMap<(NodeId, String), NodeId>,
    by_remote: HashMap<String, NodeId>,
    children: HashMap<NodeId, Vec<NodeId>>,
    next_temp: TempId,
}

impl Model {
    fn new(nodes: &[Node]) -> Self {
        let mut m = Model {
            nodes: HashMap::with_capacity(nodes.len()),
            by_name: HashMap::with_capacity(nodes.len()),
            by_remote: HashMap::with_capacity(nodes.len()),
            children: HashMap::new(),
            next_temp: -1,
        };
        for n in nodes {
            m.nodes.insert(
                n.id,
                MNode {
                    parent: n.parent_id,
                    name: n.name.clone(),
                    kind: n.kind,
                    remote_id: n.remote_id.clone(),
                    base_parent: n.remote_parent_id.clone(),
                    base_name: n.remote_name.clone(),
                    dirty: n.dirty,
                    deleted: n.deleted,
                    pending: n.pending,
                    generation: n.generation,
                    op_seq: n.op_seq,
                    pinned: n.pinned,
                },
            );
            if let Some(p) = n.parent_id {
                m.children.entry(p).or_default().push(n.id);
                if !n.deleted {
                    m.by_name.insert((p, name_key(&n.name)), n.id);
                }
            }
            if let Some(r) = &n.remote_id {
                m.by_remote.insert(r.clone(), n.id);
            }
        }
        m
    }

    fn rename(&mut self, id: NodeId, parent: NodeId, name: &str) {
        let n = self.nodes.get_mut(&id).expect("node exists");
        if let Some(old_parent) = n.parent {
            let key = (old_parent, name_key(&n.name));
            if self.by_name.get(&key) == Some(&id) {
                self.by_name.remove(&key);
            }
            if old_parent != parent
                && let Some(v) = self.children.get_mut(&old_parent)
            {
                v.retain(|c| *c != id);
            }
        }
        if n.parent != Some(parent) {
            self.children.entry(parent).or_default().push(id);
        }
        n.parent = Some(parent);
        n.name = name.to_string();
        if !n.deleted {
            self.by_name.insert((parent, name_key(name)), id);
        }
    }

    fn set_remote(&mut self, id: NodeId, e: &RemoteEntry) {
        let n = self.nodes.get_mut(&id).expect("node exists");
        if let Some(old) = n.remote_id.take() {
            self.by_remote.remove(&old);
        }
        n.remote_id = Some(e.id.clone());
        n.base_parent = Some(e.parent_id.clone());
        n.base_name = Some(e.name.clone());
        self.by_remote.insert(e.id.clone(), id);
    }

    fn clear_remote(&mut self, id: NodeId) {
        let n = self.nodes.get_mut(&id).expect("node exists");
        if let Some(old) = n.remote_id.take() {
            self.by_remote.remove(&old);
        }
    }

    fn insert(&mut self, parent: NodeId, name: &str, e: &RemoteEntry, pinned: bool) -> TempId {
        let id = self.next_temp;
        self.next_temp -= 1;
        self.nodes.insert(
            id,
            MNode {
                parent: Some(parent),
                name: name.to_string(),
                kind: e.kind,
                remote_id: Some(e.id.clone()),
                base_parent: Some(e.parent_id.clone()),
                base_name: Some(e.name.clone()),
                dirty: false,
                deleted: false,
                pending: 0,
                generation: 0,
                op_seq: 0,
                pinned,
            },
        );
        self.children.entry(parent).or_default().push(id);
        self.by_name.insert((parent, name_key(name)), id);
        self.by_remote.insert(e.id.clone(), id);
        id
    }

    fn taken(&self, parent: NodeId, name: &str, except: Option<NodeId>) -> bool {
        self.by_name.get(&(parent, name_key(name))).is_some_and(|id| Some(*id) != except)
    }

    /// `name`, or `stem (2).ext`, `stem (3).ext`, … — the first that is free in `parent`.
    fn unique_name(&self, parent: NodeId, name: &str, except: Option<NodeId>) -> String {
        if !self.taken(parent, name, except) {
            return name.to_string();
        }
        let (stem, ext) = split_ext(name);
        (2..).map(|i| format!("{stem} ({i}){ext}")).find(|n| !self.taken(parent, n, except)).expect("infinite range")
    }

    fn depth(&self, mut id: NodeId) -> usize {
        let mut d = 0;
        while let Some(p) = self.nodes.get(&id).and_then(|n| n.parent) {
            d += 1;
            id = p;
        }
        d
    }

    fn has_local_changes(&self, id: NodeId) -> bool {
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            let n = &self.nodes[&cur];
            if !n.deleted && (n.dirty || n.pending & pending::PUSH != 0 || (n.remote_id.is_none() && cur != id)) {
                return true;
            }
            stack.extend(self.children.get(&cur).into_iter().flatten().copied());
        }
        false
    }

    fn file_count(&self, id: NodeId) -> u32 {
        let mut stack = vec![id];
        let mut count = 0;
        while let Some(cur) = stack.pop() {
            let n = &self.nodes[&cur];
            if n.kind == Kind::File && !n.deleted {
                count += 1;
            }
            stack.extend(self.children.get(&cur).into_iter().flatten().copied());
        }
        count
    }
}

/// Splits `name.ext` into (`name`, `.ext`); dotfiles and extension-less names keep everything.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

/// Name for the losing copy of a same-name collision in the cloud. Deterministic, so every
/// device picks the same name.
pub fn conflicted_copy_name(name: &str) -> String {
    let (stem, ext) = split_ext(name);
    format!("{stem} (conflicted copy){ext}")
}

pub fn conflict_name(name: &str, device: &str, stamp: &str) -> String {
    let (stem, ext) = split_ext(name);
    format!("{stem} (conflict {device} {stamp}){ext}")
}

pub fn plan(input: &PlanInput<'_>) -> Result<Plan, PlanError> {
    let tree = input.tree;
    if tree.entries.is_empty() && !tree.skipped.is_empty() {
        return Err(PlanError::WrongPassword(tree.skipped.len()));
    }

    let mut m = Model::new(input.nodes);
    let mut out = Plan::default();
    let remote: HashMap<&str, &RemoteEntry> =
        tree.entries.iter().filter(|e| !input.orphans.contains(&e.id)).map(|e| (e.id.as_str(), e)).collect();
    let mut kids: HashMap<&str, Vec<&RemoteEntry>> = HashMap::new();
    for e in remote.values() {
        kids.entry(e.parent_id.as_str()).or_default().push(e);
    }
    for v in kids.values_mut() {
        // Deterministic order: oldest-looking first so duplicates keep stable names.
        v.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    }

    if m.nodes[&ROOT_ID].remote_id.as_deref() != Some(tree.root_id.as_str()) {
        out.changes.push(Change::SetRootRemote(tree.root_id.clone()));
        let n = m.nodes.get_mut(&ROOT_ID).unwrap();
        n.remote_id = Some(tree.root_id.clone());
        m.by_remote.insert(tree.root_id.clone(), ROOT_ID);
    }

    // Top-down walk so parents are always placed before their children.
    let mut queue: VecDeque<(&str, NodeId)> = VecDeque::from([(tree.root_id.as_str(), ROOT_ID)]);
    while let Some((remote_parent, local_parent)) = queue.pop_front() {
        let Some(children) = kids.get(remote_parent) else { continue };
        for e in children.clone() {
            if let Some(local) = place(&mut m, &mut out, input, &remote, e, local_parent)
                && e.kind == Kind::Folder
            {
                queue.push_back((e.id.as_str(), local));
            }
        }
    }

    // Remote deletions, outermost first.
    let mut missing: Vec<NodeId> = m
        .nodes
        .iter()
        .filter(|(id, n)| {
            **id > 0
                && **id != ROOT_ID
                && n.op_seq <= input.scan_seq
                && n.remote_id.as_deref().is_some_and(|r| !remote.contains_key(r))
        })
        .map(|(id, _)| *id)
        .collect();
    missing.sort_by_key(|id| (m.depth(*id), *id));
    let mut gone: HashSet<NodeId> = HashSet::new();
    let mut removals = Vec::new();
    let mut removal_files = 0u32;
    for id in missing {
        let mut anc = m.nodes[&id].parent;
        let mut covered = false;
        while let Some(a) = anc {
            if gone.contains(&a) {
                covered = true;
                break;
            }
            anc = m.nodes.get(&a).and_then(|n| n.parent);
        }
        if covered {
            continue;
        }
        let n = m.nodes[&id].clone();
        if n.deleted {
            out.changes.push(Change::DropTombstone { node: id });
            gone.insert(id);
        } else if n.kind == Kind::File {
            if n.dirty || n.pending & pending::PUSH != 0 {
                out.changes.push(Change::Resurrect { node: id, generation: n.generation });
                m.clear_remote(id);
            } else {
                removals.push(Change::Remove { node: id, generation: n.generation });
                removal_files += 1;
                gone.insert(id);
            }
        } else if m.has_local_changes(id) {
            out.changes.push(Change::Resurrect { node: id, generation: n.generation });
            m.clear_remote(id);
        } else {
            removal_files += m.file_count(id);
            removals.push(Change::Remove { node: id, generation: n.generation });
            gone.insert(id);
        }
    }

    // Removed nodes keep their remote id in the model, so this counts them too.
    let synced_files =
        m.nodes.iter().filter(|(id, n)| **id > 0 && n.kind == Kind::File && n.remote_id.is_some()).count() as u32;
    let suspicious = removal_files > 0
        && ((removal_files >= 100 && removal_files * 2 >= synced_files) || (remote.is_empty() && removal_files >= 10));
    if suspicious && !input.allow_mass_removal {
        out.held_removal_files = removal_files;
    } else {
        out.changes.extend(removals);
    }
    Ok(out)
}

/// Places one remote entry under `parent`. Returns the local node it maps to, if any.
fn place(
    m: &mut Model,
    out: &mut Plan,
    input: &PlanInput<'_>,
    remote: &HashMap<&str, &RemoteEntry>,
    e: &RemoteEntry,
    parent: NodeId,
) -> Option<NodeId> {
    if let Some(&id) = m.by_remote.get(&e.id) {
        let n = m.nodes[&id].clone();
        if n.op_seq > input.scan_seq || n.kind != e.kind {
            return Some(id);
        }
        let moved = n.base_parent.as_deref() != Some(e.parent_id.as_str()) || n.base_name.as_deref() != Some(e.name.as_str());
        if !moved {
            return Some(id);
        }
        if n.deleted || n.pending & pending::MOVE != 0 || !(input.valid_name)(&e.name) {
            // Our local location wins; the worker moves the remote item back/onwards.
            out.changes.push(Change::Rebase { node: id, entry: e.clone() });
            let mn = m.nodes.get_mut(&id).unwrap();
            mn.base_parent = Some(e.parent_id.clone());
            mn.base_name = Some(e.name.clone());
            if !n.deleted && (n.parent != Some(parent) || n.name != e.name) {
                mn.pending |= pending::MOVE;
            }
            return Some(id);
        }
        let name = m.unique_name(parent, &e.name, Some(id));
        let needs_move = name != e.name;
        if n.parent == Some(parent) && n.name == name {
            out.changes.push(Change::Rebase { node: id, entry: e.clone() });
        } else {
            out.changes.push(Change::Move { node: id, generation: n.generation, parent, name: name.clone(), entry: e.clone(), needs_move });
            m.rename(id, parent, &name);
        }
        let mn = m.nodes.get_mut(&id).unwrap();
        mn.base_parent = Some(e.parent_id.clone());
        mn.base_name = Some(e.name.clone());
        return Some(id);
    }

    if !(input.valid_name)(&e.name) {
        out.invalid_names.push(e.clone());
        return None;
    }

    let pinned = m.nodes.get(&parent).is_some_and(|p| p.pinned);
    let mut needs_move = false;
    if let Some(&x) = m.by_name.get(&(parent, name_key(&e.name))) {
        let xn = m.nodes[&x].clone();
        let old_gone = xn.remote_id.as_deref().is_some_and(|r| !remote.contains_key(r)) && xn.op_seq <= input.scan_seq;
        match (xn.kind, e.kind) {
            (Kind::File, Kind::File) if old_gone => {
                if xn.dirty || xn.pending & pending::PUSH != 0 {
                    // Edited here and replaced there: keep ours as a conflicted copy.
                    let name = m.unique_name(parent, &conflict_name(&xn.name, input.device_name, input.stamp), Some(x));
                    out.changes.push(Change::MoveAside { node: x, generation: xn.generation, name: name.clone(), detach: true });
                    out.conflicts.push(name.clone());
                    m.rename(x, parent, &name);
                    m.clear_remote(x);
                } else {
                    out.changes.push(Change::Replace { node: x, generation: xn.generation, entry: e.clone() });
                    m.set_remote(x, e);
                    return Some(x);
                }
            }
            (Kind::File, Kind::File) if xn.remote_id.is_none() => {
                // Created here and there with the same name before either synced.
                let name = m.unique_name(parent, &conflict_name(&xn.name, input.device_name, input.stamp), Some(x));
                out.changes.push(Change::MoveAside { node: x, generation: xn.generation, name: name.clone(), detach: false });
                out.conflicts.push(name.clone());
                m.rename(x, parent, &name);
            }
            (Kind::Folder, Kind::Folder) if xn.remote_id.is_none() || old_gone => {
                out.changes.push(Change::Adopt { node: x, generation: xn.generation, entry: e.clone() });
                m.set_remote(x, e);
                let mn = m.nodes.get_mut(&x).unwrap();
                mn.pending &= !pending::PUSH;
                return Some(x);
            }
            (a, b) if a == b => {
                // Two remote items with the same name, e.g. edits uploaded by two devices at the
                // same moment. Every device must resolve this identically or they would rename
                // back and forth: the smaller remote id keeps the name, the other becomes a
                // conflicted copy.
                let x_remote = xn.remote_id.clone().unwrap_or_default();
                if e.id < x_remote {
                    let copy = m.unique_name(parent, &conflicted_copy_name(&xn.name), None);
                    if x < 0 {
                        // `x` is itself being created by this plan: just create it under the copy name.
                        for c in out.changes.iter_mut() {
                            if let Change::Create { temp_id, name, needs_move, .. } = c
                                && *temp_id == x
                            {
                                *name = copy.clone();
                                *needs_move = true;
                            }
                        }
                    } else {
                        out.changes.push(Change::MoveAside { node: x, generation: xn.generation, name: copy.clone(), detach: false });
                    }
                    m.rename(x, parent, &copy);
                    let mn = m.nodes.get_mut(&x).unwrap();
                    mn.pending |= pending::MOVE;
                    out.conflicts.push(copy);
                } else {
                    needs_move = true;
                }
            }
            _ => {
                // A file and a folder with the same name: move the local one aside.
                let name = m.unique_name(parent, &conflict_name(&xn.name, input.device_name, input.stamp), Some(x));
                out.changes.push(Change::MoveAside { node: x, generation: xn.generation, name: name.clone(), detach: false });
                out.conflicts.push(name.clone());
                m.rename(x, parent, &name);
            }
        }
    }
    let name = if needs_move { m.unique_name(parent, &conflicted_copy_name(&e.name), None) } else { e.name.clone() };
    if needs_move {
        out.conflicts.push(name.clone());
    }
    let temp_id = m.insert(parent, &name, e, pinned);
    out.changes.push(Change::Create { temp_id, parent, name, entry: e.clone(), needs_move, pinned });
    Some(temp_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: NodeId, parent: NodeId, name: &str, kind: Kind, remote: Option<(&str, &str)>) -> Node {
        Node {
            id,
            parent_id: Some(parent),
            name: name.into(),
            kind,
            size: 1,
            mtime_ms: 0,
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
            hydrated: false,
            pinned: false,
            accessed_ms: 0,
        }
    }

    fn root() -> Node {
        let mut r = node(ROOT_ID, 0, "", Kind::Folder, Some(("R", "")));
        r.parent_id = None;
        r
    }

    fn entry(id: &str, parent: &str, name: &str, kind: Kind) -> RemoteEntry {
        RemoteEntry { id: id.into(), parent_id: parent.into(), kind, name: name.into(), size: 1, mtime_ms: 0 }
    }

    fn run(nodes: &[Node], entries: Vec<RemoteEntry>) -> Plan {
        let tree = RemoteTree { root_id: "R".into(), entries, skipped: vec![] };
        let orphans = HashSet::new();
        plan(&PlanInput {
            nodes,
            tree: &tree,
            scan_seq: 10,
            orphans: &orphans,
            valid_name: &crate::engine::local::valid_portable_name,
            device_name: "PC",
            stamp: "2026-11-10 1200",
            allow_mass_removal: false,
        })
        .unwrap()
    }

    #[test]
    fn creates_new_tree_in_order() {
        let p = run(&[root()], vec![entry("f", "D", "a.txt", Kind::File), entry("D", "R", "Docs", Kind::Folder)]);
        match &p.changes[..] {
            [Change::Create { temp_id: -1, parent: 1, name, .. }, Change::Create { parent: -1, name: n2, .. }] => {
                assert_eq!(name, "Docs");
                assert_eq!(n2, "a.txt");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn remote_move_and_rename() {
        let nodes = [root(), node(2, 1, "Docs", Kind::Folder, Some(("D", "R"))), node(3, 1, "a.txt", Kind::File, Some(("f", "R")))];
        let p = run(&nodes, vec![entry("D", "R", "Docs", Kind::Folder), entry("f", "D", "b.txt", Kind::File)]);
        assert_eq!(
            p.changes,
            vec![Change::Move { node: 3, generation: 0, parent: 2, name: "b.txt".into(), entry: entry("f", "D", "b.txt", Kind::File), needs_move: false }]
        );
    }

    #[test]
    fn local_pending_move_wins() {
        let mut n = node(3, 1, "mine.txt", Kind::File, Some(("f", "R")));
        n.remote_name = Some("a.txt".into());
        n.pending = pending::MOVE;
        let p = run(&[root(), n], vec![entry("f", "R", "theirs.txt", Kind::File)]);
        assert!(matches!(p.changes[..], [Change::Rebase { node: 3, .. }]));
    }

    #[test]
    fn replaced_clean_file_is_replaced() {
        let nodes = [root(), node(3, 1, "a.txt", Kind::File, Some(("old", "R")))];
        let p = run(&nodes, vec![entry("new", "R", "a.txt", Kind::File)]);
        assert!(matches!(&p.changes[..], [Change::Replace { node: 3, entry, .. }] if entry.id == "new"));
    }

    #[test]
    fn replaced_dirty_file_conflicts() {
        let mut n = node(3, 1, "a.txt", Kind::File, Some(("old", "R")));
        n.dirty = true;
        n.pending = pending::PUSH;
        let p = run(&[root(), n], vec![entry("new", "R", "a.txt", Kind::File)]);
        match &p.changes[..] {
            [Change::MoveAside { node: 3, name, detach: true, .. }, Change::Create { name: created, .. }] => {
                assert_eq!(name, "a (conflict PC 2026-11-10 1200).txt");
                assert_eq!(created, "a.txt");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(p.conflicts.len(), 1);
    }

    #[test]
    fn deleted_remotely() {
        let mut dirty = node(4, 1, "edited.txt", Kind::File, Some(("e", "R")));
        dirty.dirty = true;
        dirty.pending = pending::PUSH;
        let nodes = [root(), node(3, 1, "a.txt", Kind::File, Some(("f", "R"))), dirty];
        let p = run(&nodes, vec![]);
        assert!(p.changes.contains(&Change::Remove { node: 3, generation: 0 }));
        assert!(p.changes.contains(&Change::Resurrect { node: 4, generation: 0 }));
    }

    #[test]
    fn deleted_folder_with_edits_inside_is_resurrected() {
        let mut edited = node(5, 2, "new.txt", Kind::File, None);
        edited.dirty = true;
        edited.pending = pending::PUSH;
        let nodes = [
            root(),
            node(2, 1, "Docs", Kind::Folder, Some(("D", "R"))),
            node(3, 2, "clean.txt", Kind::File, Some(("c", "D"))),
            edited,
        ];
        let p = run(&nodes, vec![]);
        assert!(p.changes.contains(&Change::Resurrect { node: 2, generation: 0 }));
        assert!(p.changes.contains(&Change::Remove { node: 3, generation: 0 }));
        assert!(!p.changes.iter().any(|c| matches!(c, Change::Remove { node: 5, .. })));
    }

    #[test]
    fn duplicates_get_unique_names() {
        let p = run(&[root()], vec![entry("a", "R", "x.txt", Kind::File), entry("b", "R", "X.txt", Kind::File)]);
        let names: Vec<_> = p
            .changes
            .iter()
            .filter_map(|c| match c {
                Change::Create { name, needs_move, .. } => Some((name.clone(), *needs_move)),
                _ => None,
            })
            .collect();
        // The smaller id ("a", named "x.txt") keeps the name on every device.
        assert_eq!(names, vec![("X (conflicted copy).txt".into(), true), ("x.txt".into(), false)]);
    }

    #[test]
    fn local_folder_adopted() {
        let mut f = node(2, 1, "Photos", Kind::Folder, None);
        f.pending = pending::PUSH;
        let p = run(&[root(), f], vec![entry("P", "R", "Photos", Kind::Folder)]);
        assert!(matches!(&p.changes[..], [Change::Adopt { node: 2, .. }]));
    }

    #[test]
    fn mass_removal_is_held() {
        let mut nodes = vec![root()];
        for i in 0..150 {
            nodes.push(node(10 + i, 1, &format!("f{i}"), Kind::File, Some((&format!("r{i}"), "R"))));
        }
        let keep: Vec<_> = (0..20).map(|i| entry(&format!("r{i}"), "R", &format!("f{i}"), Kind::File)).collect();
        let p = run(&nodes, keep);
        assert_eq!(p.held_removal_files, 130);
        assert!(!p.changes.iter().any(|c| matches!(c, Change::Remove { .. })));
    }

    #[test]
    fn wrong_password_detected() {
        let tree = RemoteTree { root_id: "R".into(), entries: vec![], skipped: vec!["garbage".into()] };
        let orphans = HashSet::new();
        let r = plan(&PlanInput {
            nodes: &[root()],
            tree: &tree,
            scan_seq: 0,
            orphans: &orphans,
            valid_name: &crate::engine::local::valid_portable_name,
            device_name: "PC",
            stamp: "x",
            allow_mass_removal: false,
        });
        assert_eq!(r.unwrap_err(), PlanError::WrongPassword(1));
    }

    #[test]
    fn invalid_names_skipped() {
        let p = run(&[root()], vec![entry("a", "R", "bad:name", Kind::File)]);
        assert!(p.changes.is_empty());
        assert_eq!(p.invalid_names.len(), 1);
    }

    #[test]
    fn fresh_local_ops_are_not_undone() {
        let mut n = node(3, 1, "a.txt", Kind::File, Some(("f", "R")));
        n.op_seq = 11; // changed by our worker after the scan started
        let p = run(&[root(), n], vec![]);
        assert!(p.changes.is_empty());
    }
}
