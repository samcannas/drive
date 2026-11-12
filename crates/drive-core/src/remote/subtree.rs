//! A view of one folder of another [`Remote`], as if it were the root.
//!
//! uloz.to doesn't need this ([`crate::ulozto::UlozRemote`] takes a root folder), but the
//! in-memory and development backends only know their own root.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;

use super::resumable::{ResumableUpload, ResumeInfo, ResumeState};
use super::{ByteStream, Remote, RemoteEntry, RemoteError, RemoteTree, UploadBody};

pub struct Subtree<R: Remote> {
    inner: Arc<R>,
    root: String,
}

impl<R: Remote> Subtree<R> {
    pub fn new(inner: Arc<R>, root: impl Into<String>) -> Self {
        Self {
            inner,
            root: root.into(),
        }
    }
}

#[async_trait]
impl<R: Remote> Remote for Subtree<R> {
    async fn scan(&self) -> Result<RemoteTree, RemoteError> {
        let tree = self.inner.scan().await?;
        let mut kids: HashMap<&str, Vec<&RemoteEntry>> = HashMap::new();
        for e in &tree.entries {
            kids.entry(e.parent_id.as_str()).or_default().push(e);
        }
        if self.root != tree.root_id && !tree.entries.iter().any(|e| e.id == self.root) {
            return Err(RemoteError::NotFound(format!("folder {}", self.root)));
        }
        let mut keep: HashSet<&str> = HashSet::new();
        let mut stack = vec![self.root.as_str()];
        while let Some(id) = stack.pop() {
            for c in kids.get(id).into_iter().flatten() {
                if keep.insert(c.id.as_str()) {
                    stack.push(c.id.as_str());
                }
            }
        }
        let entries = tree
            .entries
            .iter()
            .filter(|e| keep.contains(e.id.as_str()))
            .cloned()
            .collect();
        Ok(RemoteTree {
            root_id: self.root.clone(),
            entries,
            skipped: tree.skipped,
        })
    }

    async fn list(&self, parent_id: &str) -> Result<Vec<RemoteEntry>, RemoteError> {
        self.inner.list(parent_id).await
    }

    async fn create_folder(&self, parent_id: &str, name: &str) -> Result<RemoteEntry, RemoteError> {
        self.inner.create_folder(parent_id, name).await
    }

    async fn upload(
        &self,
        parent_id: &str,
        name: &str,
        body: UploadBody,
    ) -> Result<RemoteEntry, RemoteError> {
        self.inner.upload(parent_id, name, body).await
    }

    async fn read(
        &self,
        file: &RemoteEntry,
        offset: u64,
        len: Option<u64>,
    ) -> Result<ByteStream, RemoteError> {
        self.inner.read(file, offset, len).await
    }

    async fn rename(
        &self,
        entry: &RemoteEntry,
        new_parent_id: &str,
        new_name: &str,
    ) -> Result<RemoteEntry, RemoteError> {
        self.inner.rename(entry, new_parent_id, new_name).await
    }

    async fn trash(&self, entry: &RemoteEntry) -> Result<(), RemoteError> {
        self.inner.trash(entry).await
    }

    async fn exists(&self, entry: &RemoteEntry) -> Result<bool, RemoteError> {
        self.inner.exists(entry).await
    }

    async fn upload_resumable(
        &self,
        parent_id: &str,
        name: &str,
        upload: ResumableUpload,
    ) -> Result<RemoteEntry, RemoteError> {
        self.inner.upload_resumable(parent_id, name, upload).await
    }

    async fn discard_resumable(&self, state: &ResumeState) {
        self.inner.discard_resumable(state).await
    }

    fn resumable_min_size(&self) -> Option<u64> {
        self.inner.resumable_min_size()
    }

    fn resume_info(&self, state: &ResumeState) -> Option<ResumeInfo> {
        self.inner.resume_info(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::Kind;
    use crate::remote::mem::{MemRemote, ROOT};

    #[tokio::test]
    async fn scans_only_the_subtree() {
        let mem = Arc::new(MemRemote::new());
        let a = mem.put_folder(ROOT, "a");
        let b = mem.put_folder(&a, "b");
        mem.put_file(&b, "in.txt", b"x", 0);
        mem.put_file(ROOT, "out.txt", b"y", 0);
        let sub = Subtree::new(mem.clone(), a.clone());
        let tree = sub.scan().await.unwrap();
        assert_eq!(tree.root_id, a);
        let mut names: Vec<_> = tree.entries.iter().map(|e| e.name.as_str()).collect();
        names.sort();
        assert_eq!(names, ["b", "in.txt"]);
        assert!(
            tree.entries
                .iter()
                .any(|e| e.kind == Kind::Folder && e.parent_id == a)
        );
        let gone = Subtree::new(mem, "nope");
        assert!(matches!(gone.scan().await, Err(RemoteError::NotFound(_))));
    }
}
