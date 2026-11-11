//! uloz.to API client and the raw (unencrypted) [`Remote`] built on it.
//!
//! Modeled on rclone's `ulozto` backend (the only other client known to work), with:
//! * session tokens refreshed by logging in again on `401 / 70001`,
//! * a pacer that backs off on 429/5xx and limits concurrent API calls,
//! * recursive listings (`depth=-1`) with a per-folder fallback,
//! * rclone-compatible metadata (mtime + hashes) in the file description,
//! * MD5 verification of every upload, and auth headers never sent off uloz.to hosts.

pub mod api;
pub mod chunked;
pub mod meta;

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use md5::Digest as _;
use reqwest::{Method, StatusCode, header};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, warn};

use crate::remote::{ByteStream, Kind, Remote, RemoteEntry, RemoteError, RemoteTree, UploadBody};

pub const API_ROOT: &str = "https://apis.uloz.to";

/// The shared testing API key published at <https://uloz.to/apidoc/public>. uloz.to says it may
/// change at any time and asks real applications to request a dedicated key from support;
/// users can set their own key in Settings.
pub const DEFAULT_APP_TOKEN: &str = r#";HG%7jW6@6/8vx">R;f("#;

const PAGE: usize = 500;
const USER_AGENT: &str = concat!(
    "Drive/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/samcannas/drive)"
);

#[derive(Debug, Clone)]
pub struct UlozConfig {
    pub login: String,
    pub password: String,
    pub app_token: String,
    pub device_id: String,
    pub api_root: String,
    /// An upload fails if no bytes go out for this long…
    pub stall_timeout: Duration,
    /// …or if the server hasn't answered this long after the last byte (it hashes big files).
    pub response_wait: Duration,
    /// Files at least this large (as stored, i.e. encrypted) use resumable chunked uploads.
    pub chunked_min_size: u64,
    /// Fixed chunk size (tests); `None` picks 4/32/128 MiB from the file size.
    pub chunk_size: Option<u64>,
    /// Most chunk requests in flight at once (across all uploads); adapts below this.
    pub max_chunk_streams: usize,
    /// How often to ask whether an uploaded file has been assembled.
    pub status_poll: Duration,
}

impl UlozConfig {
    pub fn new(
        login: impl Into<String>,
        password: impl Into<String>,
        device_id: impl Into<String>,
    ) -> Self {
        Self {
            login: login.into(),
            password: password.into(),
            app_token: DEFAULT_APP_TOKEN.into(),
            device_id: device_id.into(),
            api_root: API_ROOT.into(),
            stall_timeout: Duration::from_secs(120),
            response_wait: Duration::from_secs(600),
            chunked_min_size: 16 * 1024 * 1024,
            chunk_size: None,
            max_chunk_streams: 4,
            status_poll: Duration::from_secs(2),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Session {
    pub token: String,
    pub login: String,
    pub root_folder_slug: String,
}

/// Rate limiter: bounded concurrency plus an adaptive delay that grows on throttling and
/// decays on success (like rclone's pacer: 10 ms … 2 s, and up to 30 s on HTTP 429).
struct Pacer {
    permits: Semaphore,
    sleep_ms: AtomicU64,
}

impl Pacer {
    const MIN: u64 = 10;
    const MAX: u64 = 2_000;
    const MAX_THROTTLED: u64 = 30_000;

    fn new(concurrency: usize) -> Self {
        Self {
            permits: Semaphore::new(concurrency),
            sleep_ms: AtomicU64::new(Self::MIN),
        }
    }
    fn delay(&self) -> Duration {
        Duration::from_millis(self.sleep_ms.load(Ordering::Relaxed))
    }
    fn success(&self) {
        let cur = self.sleep_ms.load(Ordering::Relaxed);
        self.sleep_ms
            .store((cur / 2).max(Self::MIN), Ordering::Relaxed);
    }
    fn failure(&self, throttled: bool) {
        let cur = self.sleep_ms.load(Ordering::Relaxed);
        let cap = if throttled {
            Self::MAX_THROTTLED
        } else {
            Self::MAX
        };
        self.sleep_ms
            .store((cur * 2).clamp(500, cap), Ordering::Relaxed);
    }
}

pub struct UlozClient {
    cfg: UlozConfig,
    api: reqwest::Client,
    cdn: reqwest::Client,
    /// Uploads: no read timeout (the server says nothing until the whole body has arrived,
    /// which takes minutes for big files); a progress watchdog guards against stalls instead.
    upload: reqwest::Client,
    session: Mutex<Option<Session>>,
    pacer: Pacer,
    links: std::sync::Mutex<HashMap<String, (String, std::time::Instant)>>,
    /// Chunk streams in flight, adapted to how the server copes (AIMD).
    chunk_limit: chunked::AdaptiveLimit,
    /// Set when the chunked API refused us; single-request uploads are used until then.
    chunked_unavailable_until: std::sync::Mutex<Option<std::time::Instant>>,
}

/// Error from a single API call, before retry policy is applied.
#[derive(Debug)]
enum CallError {
    /// Session token expired: log in again and retry.
    Reauth,
    Remote(RemoteError),
}

fn net(e: reqwest::Error) -> RemoteError {
    RemoteError::Network(e.to_string())
}

/// Errors during a long transfer: only failing to connect means we're offline; a transfer
/// cut off midway is a transient failure to retry.
fn transfer_err(e: reqwest::Error) -> RemoteError {
    if e.is_connect() {
        RemoteError::Network(e.to_string())
    } else {
        RemoteError::Server(format!("transfer interrupted: {e}"))
    }
}

fn map_status(status: StatusCode, err: &api::ApiError, body: &str) -> RemoteError {
    let msg = if err.message.is_empty() {
        body.chars().take(200).collect()
    } else {
        err.message.clone()
    };
    let detail = format!("HTTP {} (error {}): {msg}", status.as_u16(), err.error_code);
    match status.as_u16() {
        401 => RemoteError::Auth(detail),
        404 | 410 => RemoteError::NotFound(detail),
        429 => RemoteError::RateLimited,
        402 | 507 => RemoteError::Quota,
        500..=599 => RemoteError::Server(detail),
        _ if msg.to_lowercase().contains("quota") || msg.to_lowercase().contains("space") => {
            RemoteError::Quota
        }
        _ => RemoteError::Other(detail),
    }
}

fn is_uloz_host(url: &reqwest::Url) -> bool {
    url.host_str()
        .is_some_and(|h| h == "uloz.to" || h.ends_with(".uloz.to"))
}

impl UlozClient {
    pub fn new(cfg: UlozConfig) -> Result<Self, RemoteError> {
        let cfg_max_streams = cfg.max_chunk_streams.max(1);
        let api = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .timeout(Duration::from_secs(120))
            // API calls never need redirects; refusing them keeps tokens on apis.uloz.to.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(net)?;
        let cdn = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(net)?;
        let upload = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(20))
            .tcp_keepalive(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(net)?;
        Ok(Self {
            cfg,
            api,
            cdn,
            upload,
            session: Mutex::new(None),
            pacer: Pacer::new(4),
            links: std::sync::Mutex::new(HashMap::new()),
            chunk_limit: chunked::AdaptiveLimit::new(2, cfg_max_streams),
            chunked_unavailable_until: std::sync::Mutex::new(None),
        })
    }

    pub fn config(&self) -> &UlozConfig {
        &self.cfg
    }

    /// Logs in (always makes a request) and stores the session.
    pub async fn login(&self) -> Result<Session, RemoteError> {
        let mut guard = self.session.lock().await;
        let s = self.do_login().await?;
        *guard = Some(s.clone());
        Ok(s)
    }

    async fn do_login(&self) -> Result<Session, RemoteError> {
        let url = format!("{}/v6/session", self.cfg.api_root);
        let body = api::AuthenticateRequest {
            login: &self.cfg.login,
            password: &self.cfg.password,
            device_id: Some(&self.cfg.device_id),
        };
        for attempt in 0..5 {
            tokio::time::sleep(self.pacer.delay()).await;
            let resp = self
                .api
                .put(&url)
                .header("X-Auth-Token", &self.cfg.app_token)
                .json(&body)
                .send()
                .await;
            let resp = match resp {
                Ok(r) => r,
                Err(e) if attempt < 4 => {
                    debug!("login network error: {e}");
                    self.pacer.failure(false);
                    continue;
                }
                Err(e) => return Err(net(e)),
            };
            let status = resp.status();
            let text = resp.text().await.map_err(net)?;
            if status.is_success() {
                self.pacer.success();
                let a: api::AuthenticateResponse = serde_json::from_str(&text)
                    .map_err(|e| RemoteError::Other(format!("bad login response: {e}")))?;
                return Ok(Session {
                    token: a.token_id,
                    login: a.session.user.login,
                    root_folder_slug: a.session.user.root_folder_slug,
                });
            }
            let err: api::ApiError = serde_json::from_str(&text).unwrap_or_default();
            match status.as_u16() {
                429 | 500..=599 if attempt < 4 => {
                    self.pacer.failure(status == StatusCode::TOO_MANY_REQUESTS);
                    continue;
                }
                400 | 401 | 403 | 404 | 422 => {
                    let msg = if err.message.is_empty() {
                        "wrong login or password".into()
                    } else {
                        err.message
                    };
                    return Err(RemoteError::Auth(msg));
                }
                _ => return Err(map_status(status, &err, &text)),
            }
        }
        Err(RemoteError::Network("login failed repeatedly".into()))
    }

    pub async fn session(&self) -> Result<Session, RemoteError> {
        let mut guard = self.session.lock().await;
        if let Some(s) = guard.as_ref() {
            return Ok(s.clone());
        }
        let s = self.do_login().await?;
        *guard = Some(s.clone());
        Ok(s)
    }

    async fn reauth(&self, stale_token: &str) -> Result<(), RemoteError> {
        let mut guard = self.session.lock().await;
        if guard.as_ref().is_some_and(|s| s.token != stale_token) {
            return Ok(()); // someone else already refreshed it
        }
        *guard = Some(self.do_login().await?);
        Ok(())
    }

    async fn once<B: Serialize + ?Sized>(
        &self,
        method: &Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&B>,
        token: &str,
    ) -> Result<String, CallError> {
        let url = format!("{}{}", self.cfg.api_root, path);
        let mut req = self
            .api
            .request(method.clone(), &url)
            .header("X-Auth-Token", &self.cfg.app_token)
            .header("X-User-Token", token)
            .query(query);
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| CallError::Remote(net(e)))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| CallError::Remote(net(e)))?;
        if status.is_success() {
            return Ok(text);
        }
        let err: api::ApiError = serde_json::from_str(&text).unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED && err.error_code == 70001 {
            return Err(CallError::Reauth);
        }
        Err(CallError::Remote(map_status(status, &err, &text)))
    }

    /// Calls a JSON endpoint with pacing, retries and transparent re-login.
    async fn call<T: DeserializeOwned, B: Serialize + ?Sized>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&B>,
    ) -> Result<T, RemoteError> {
        let mut reauthed = false;
        let mut attempt = 0;
        loop {
            attempt += 1;
            let session = self.session().await?;
            let _permit = self
                .pacer
                .permits
                .acquire()
                .await
                .expect("semaphore never closed");
            tokio::time::sleep(self.pacer.delay()).await;
            match self.once(&method, path, query, body, &session.token).await {
                Ok(text) => {
                    self.pacer.success();
                    let text = if text.trim().is_empty() {
                        "null"
                    } else {
                        text.as_str()
                    };
                    return serde_json::from_str(text).map_err(|e| {
                        RemoteError::Other(format!("unexpected response from {path}: {e}"))
                    });
                }
                Err(CallError::Reauth) if !reauthed => {
                    reauthed = true;
                    drop(_permit);
                    self.reauth(&session.token).await?;
                }
                Err(CallError::Reauth) => {
                    return Err(RemoteError::Auth("session rejected after re-login".into()));
                }
                Err(CallError::Remote(e)) if e.is_transient() && attempt < 6 => {
                    self.pacer.failure(e == RemoteError::RateLimited);
                    warn!("uloz.to {path}: {e}; retrying ({attempt}/5)");
                }
                Err(CallError::Remote(e)) => return Err(e),
            }
        }
    }

    // ---- listing ---------------------------------------------------------------------

    async fn folder_page(
        &self,
        folder: &str,
        depth: i32,
        offset: usize,
    ) -> Result<Vec<serde_json::Value>, RemoteError> {
        let s = self.session().await?;
        let path = format!("/v9/user/{}/folder/{}/folder-list", s.login, folder);
        let q = [
            ("status", "ok".to_string()),
            ("limit", PAGE.to_string()),
            ("offset", offset.to_string()),
            ("sort", "created".to_string()),
            ("depth", depth.to_string()),
        ];
        let v: serde_json::Value = self.call(Method::GET, &path, &q, None::<&()>).await?;
        Ok(v.get("subfolders")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default())
    }

    async fn file_page(
        &self,
        folder: &str,
        depth: i32,
        offset: usize,
    ) -> Result<Vec<api::File>, RemoteError> {
        let s = self.session().await?;
        let path = format!("/v8/user/{}/folder/{}/file-list", s.login, folder);
        let q = [
            ("status", "ok".to_string()),
            ("limit", PAGE.to_string()),
            ("offset", offset.to_string()),
            ("sort", "created".to_string()),
            ("depth", depth.to_string()),
        ];
        let r: api::ListFilesResponse = self.call(Method::GET, &path, &q, None::<&()>).await?;
        Ok(r.items)
    }

    /// Folders below `folder` (`recursive` uses `depth=-1`). Nested `subfolders` are flattened.
    pub async fn list_folders(
        &self,
        folder: &str,
        recursive: bool,
    ) -> Result<Vec<api::Folder>, RemoteError> {
        let mut out = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .folder_page(folder, if recursive { -1 } else { 0 }, offset)
                .await?;
            let n = page.len();
            let mut stack: Vec<(serde_json::Value, String)> =
                page.into_iter().map(|v| (v, folder.to_string())).collect();
            while let Some((mut v, parent)) = stack.pop() {
                let children = v.as_object_mut().and_then(|o| o.remove("subfolders"));
                let mut f: api::Folder = serde_json::from_value(v)
                    .map_err(|e| RemoteError::Other(format!("bad folder entry: {e}")))?;
                if f.parent_folder_slug.as_deref().unwrap_or("").is_empty() {
                    f.parent_folder_slug = Some(parent);
                }
                if let Some(serde_json::Value::Array(kids)) = children {
                    stack.extend(kids.into_iter().map(|k| (k, f.slug.clone())));
                }
                out.push(f);
            }
            if n < PAGE {
                break;
            }
            offset += n;
        }
        Ok(out)
    }

    pub async fn list_files(
        &self,
        folder: &str,
        recursive: bool,
    ) -> Result<Vec<api::File>, RemoteError> {
        let mut out = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .file_page(folder, if recursive { -1 } else { 0 }, offset)
                .await?;
            let n = page.len();
            out.extend(page.into_iter().map(|mut f| {
                if !recursive && f.folder_slug.is_none() {
                    f.folder_slug = Some(folder.to_string());
                }
                f
            }));
            if n < PAGE {
                break;
            }
            offset += n;
        }
        Ok(out)
    }

    /// Lists the whole tree under `root`: two recursive calls when the API supports them,
    /// otherwise one pair of calls per folder.
    pub async fn list_tree(
        &self,
        root: &str,
    ) -> Result<(Vec<api::Folder>, Vec<api::File>), RemoteError> {
        match self.list_tree_recursive(root).await {
            Ok(Some(t)) => return Ok(t),
            Ok(None) => debug!("recursive listing inconsistent; walking folders"),
            Err(
                e @ (RemoteError::Auth(_) | RemoteError::Network(_) | RemoteError::RateLimited),
            ) => return Err(e),
            Err(e) => debug!("recursive listing failed ({e}); walking folders"),
        }
        let mut folders = Vec::new();
        let mut files = Vec::new();
        let mut queue = VecDeque::from([root.to_string()]);
        while let Some(slug) = queue.pop_front() {
            let subs = self.list_folders(&slug, false).await?;
            queue.extend(subs.iter().map(|f| f.slug.clone()));
            files.extend(self.list_files(&slug, false).await?);
            folders.extend(subs);
        }
        Ok((folders, files))
    }

    async fn list_tree_recursive(
        &self,
        root: &str,
    ) -> Result<Option<(Vec<api::Folder>, Vec<api::File>)>, RemoteError> {
        let folders = self.list_folders(root, true).await?;
        let known: HashSet<&str> = folders
            .iter()
            .map(|f| f.slug.as_str())
            .chain([root])
            .collect();
        if folders
            .iter()
            .any(|f| !known.contains(f.parent_folder_slug.as_deref().unwrap_or("")))
        {
            return Ok(None);
        }
        // If `depth` were ignored we'd only see direct children: every folder that claims to
        // have subfolders must have at least one in the listing.
        let parents: HashSet<&str> = folders
            .iter()
            .filter_map(|f| f.parent_folder_slug.as_deref())
            .collect();
        if folders
            .iter()
            .any(|f| f.has_subfolder == Some(true) && !parents.contains(f.slug.as_str()))
        {
            return Ok(None);
        }
        let files = self.list_files(root, true).await?;
        if files
            .iter()
            .any(|f| !known.contains(f.folder_slug.as_deref().unwrap_or("")))
        {
            return Ok(None);
        }
        // Cross-check the file count against the server's recursive folder statistics, so a
        // partial listing can never be mistaken for deletions.
        match self.folder_stats(root).await {
            Ok(stats) if stats.files_count >= 0 && stats.files_count as usize != files.len() => {
                debug!(
                    "recursive listing found {} files, server reports {}",
                    files.len(),
                    stats.files_count
                );
                return Ok(None);
            }
            Ok(_) => {}
            Err(e) => debug!("folder statistics unavailable: {e}"),
        }
        Ok(Some((folders, files)))
    }

    async fn folder_stats(&self, folder: &str) -> Result<api::FolderSize, RemoteError> {
        let s = self.session().await?;
        let sizes: HashMap<String, api::FolderSizes> = self
            .call(
                Method::GET,
                &format!("/v6/user/{}/folder/{folder}/folder-sizes", s.login),
                &[("recursive", "true".into())],
                None::<&()>,
            )
            .await?;
        Ok(sizes
            .get(folder)
            .map(|s| s.recursive.clone())
            .unwrap_or_default())
    }

    // ---- mutations -------------------------------------------------------------------

    pub async fn create_folder(
        &self,
        parent: &str,
        name: &str,
    ) -> Result<api::Folder, RemoteError> {
        let s = self.session().await?;
        let path = format!("/v6/user/{}/folder", s.login);
        let mut f: api::Folder = self
            .call(
                Method::POST,
                &path,
                &[],
                Some(&api::CreateFolderRequest {
                    name,
                    parent_folder_slug: parent,
                }),
            )
            .await?;
        if f.parent_folder_slug.is_none() {
            f.parent_folder_slug = Some(parent.to_string());
        }
        Ok(f)
    }

    /// Whether a file exists and is not in the trash.
    pub async fn file_exists(&self, slug: &str) -> Result<bool, RemoteError> {
        match self
            .call::<api::File, ()>(Method::GET, &format!("/v7/file/{slug}/private"), &[], None)
            .await
        {
            Ok(f) => Ok(!f.is_in_trash && f.status.as_deref().is_none_or(|s| s == "ok")),
            Err(RemoteError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub async fn update_file(
        &self,
        slug: &str,
        req: &api::UpdateFileRequest<'_>,
    ) -> Result<api::File, RemoteError> {
        self.call(
            Method::PATCH,
            &format!("/v8/file/{slug}/private"),
            &[],
            Some(req),
        )
        .await
    }

    pub async fn rename_folder(&self, slug: &str, name: &str) -> Result<(), RemoteError> {
        let s = self.session().await?;
        let _: serde::de::IgnoredAny = self
            .call(
                Method::PATCH,
                &format!("/v7/user/{}/folder/{slug}", s.login),
                &[],
                Some(&api::RenameFolderRequest { name }),
            )
            .await?;
        Ok(())
    }

    pub async fn move_folder(&self, slug: &str, parent: &str) -> Result<(), RemoteError> {
        let s = self.session().await?;
        let _: serde::de::IgnoredAny = self
            .call(
                Method::PATCH,
                &format!("/v6/user/{}/folder-list/parent-folder", s.login),
                &[],
                Some(&api::MoveFolderRequest {
                    slugs: vec![slug],
                    parent_folder_slug: parent,
                }),
            )
            .await?;
        Ok(())
    }

    /// Moves a file to the uloz.to trash (one DELETE; a second would delete it for good).
    pub async fn trash_file(&self, slug: &str) -> Result<(), RemoteError> {
        let _: serde::de::IgnoredAny = self
            .call(
                Method::DELETE,
                &format!("/v6/file/{slug}/private"),
                &[],
                None::<&()>,
            )
            .await?;
        Ok(())
    }

    /// Removes a folder; uloz.to moves the files inside it to the trash.
    pub async fn delete_folder(&self, slug: &str) -> Result<(), RemoteError> {
        let s = self.session().await?;
        let _: serde::de::IgnoredAny = self
            .call(
                Method::DELETE,
                &format!("/v5/user/{}/folder-list", s.login),
                &[],
                Some(&api::DeleteFoldersRequest { slugs: vec![slug] }),
            )
            .await?;
        Ok(())
    }

    pub async fn used_bytes(&self, folder: &str) -> Result<u64, RemoteError> {
        Ok(self.folder_stats(folder).await?.files_size.max(0) as u64)
    }

    /// Uploads `body` as a new file named `name` in `folder`, verifying the server's MD5.
    pub async fn upload(
        &self,
        folder: &str,
        name: &str,
        body: UploadBody,
    ) -> Result<api::File, RemoteError> {
        let s = self.session().await?;
        let link: api::CreateUploadUrlResponse = self
            .call(
                Method::POST,
                "/v5/upload/link",
                &[],
                Some(&api::CreateUploadUrlRequest {
                    user_login: &s.login,
                    realm: "ulozto",
                }),
            )
            .await?;

        let hashes = Arc::new(std::sync::Mutex::new((
            md5::Md5::new(),
            sha2::Sha256::new(),
        )));
        let h = hashes.clone();
        let counted = Arc::new(AtomicU64::new(0));
        let c = counted.clone();
        let stream = body.stream.map(move |chunk| {
            if let Ok(bytes) = &chunk {
                let mut g = h.lock().unwrap();
                g.0.update(bytes);
                g.1.update(bytes);
                c.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            }
            chunk
        });
        let part = reqwest::multipart::Part::stream_with_length(
            reqwest::Body::wrap_stream(stream),
            body.size,
        )
        .file_name(name.to_string())
        .mime_str("application/octet-stream")
        .map_err(net)?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let url = format!("{}&batch_file_id=1&is_porn=false", link.upload_url);
        let send = self.upload.post(&url).multipart(form).send();
        tokio::pin!(send);
        let (mut seen, mut since) = (0u64, std::time::Instant::now());
        let resp = loop {
            tokio::select! {
                r = &mut send => break r.map_err(transfer_err)?,
                _ = tokio::time::sleep(Duration::from_secs(10)) => {
                    let sent = counted.load(Ordering::Relaxed);
                    if sent != seen {
                        (seen, since) = (sent, std::time::Instant::now());
                    }
                    let limit = if sent >= body.size { self.cfg.response_wait } else { self.cfg.stall_timeout };
                    if since.elapsed() > limit {
                        return Err(RemoteError::Server(format!(
                            "upload stalled ({sent} of {} bytes sent, no progress for {} s)",
                            body.size,
                            limit.as_secs()
                        )));
                    }
                }
            }
        };
        let status = resp.status();
        let text = tokio::time::timeout(self.cfg.response_wait, resp.text())
            .await
            .map_err(|_| RemoteError::Server("no response after the upload finished".into()))?
            .map_err(transfer_err)?;
        if !status.is_success() {
            let err: api::ApiError = serde_json::from_str(&text).unwrap_or_default();
            return Err(map_status(status, &err, &text));
        }
        let sent: api::SendFilePayloadResponse = serde_json::from_str(&text)
            .map_err(|e| RemoteError::Other(format!("bad upload response: {e}")))?;
        let (md5, sha) = {
            let g = hashes.lock().unwrap();
            (
                g.0.clone().finalize().to_vec(),
                g.1.clone().finalize().to_vec(),
            )
        };
        if counted.load(Ordering::Relaxed) != body.size {
            return Err(RemoteError::Integrity(
                "upload stream length changed while sending".into(),
            ));
        }
        if !sent.md5.eq_ignore_ascii_case(&hex::encode(&md5)) {
            return Err(RemoteError::Integrity(format!(
                "MD5 mismatch (server {}, local {})",
                sent.md5,
                hex::encode(&md5)
            )));
        }

        let description = meta::encode(&meta::RcloneMeta {
            md5,
            sha256: sha,
            mtime_micros: body.mtime_ms.saturating_mul(1000),
        });
        self.finalize(
            &link.private_slug,
            1,
            &sent.slug,
            folder,
            name,
            &description,
        )
        .await
    }

    /// Gets (and caches) a download link. Tries the premium endpoint first, then the free one.
    pub async fn download_link(&self, slug: &str) -> Result<String, RemoteError> {
        if let Some((url, until)) = self.links.lock().unwrap().get(slug)
            && *until > std::time::Instant::now()
        {
            return Ok(url.clone());
        }
        let s = self.session().await?;
        let req = api::DownloadLinkRequest {
            file_slug: slug,
            user_login: &s.login,
            device_id: &self.cfg.device_id,
        };
        let resp: api::DownloadLinkResponse = match self
            .call(
                Method::POST,
                "/v5/file/download-link/vipdata",
                &[],
                Some(&req),
            )
            .await
        {
            Ok(r) => r,
            Err(
                e @ (RemoteError::Network(_) | RemoteError::RateLimited | RemoteError::NotFound(_)),
            ) => return Err(e),
            Err(vip_err) => {
                debug!("vipdata link failed ({vip_err}); trying free link");
                self.call(Method::POST, "/v5/file/download-link/free", &[], Some(&req))
                    .await
                    .map_err(|e| {
                        RemoteError::DownloadNotAllowed(format!("{vip_err}; free download: {e}"))
                    })?
            }
        };
        let mut url = resp.link;
        if let Some(h) = resp.hash.filter(|h| !h.is_empty()) {
            url.push(if url.contains('?') { '&' } else { '?' });
            url.push_str("hash=");
            url.push_str(&url::form_urlencoded::byte_serialize(h.as_bytes()).collect::<String>());
        }
        let ttl = resp
            .download_url_valid_until
            .map(|t| (t - chrono::Utc::now()).num_seconds() - 60)
            .filter(|s| *s > 0)
            .unwrap_or(300) as u64;
        self.links.lock().unwrap().insert(
            slug.to_string(),
            (
                url.clone(),
                std::time::Instant::now() + Duration::from_secs(ttl.min(3600)),
            ),
        );
        Ok(url)
    }

    /// Streams bytes `[offset, offset+len)` of a file (to EOF if `len` is `None`).
    pub async fn read(
        &self,
        slug: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<ByteStream, RemoteError> {
        let mut last_err = RemoteError::Network("no attempt made".into());
        for attempt in 0..4 {
            if attempt > 0 {
                self.links.lock().unwrap().remove(slug);
                tokio::time::sleep(Duration::from_millis(500 * (1 << attempt))).await;
            }
            let url = self.download_link(slug).await?;
            match self.fetch(&url, offset, len).await {
                Ok(s) => return Ok(s),
                Err(e)
                    if e.is_transient()
                        || matches!(e, RemoteError::NotFound(_) | RemoteError::Other(_)) =>
                {
                    last_err = e
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_err)
    }

    async fn fetch(
        &self,
        url: &str,
        offset: u64,
        len: Option<u64>,
    ) -> Result<ByteStream, RemoteError> {
        let session = self.session().await?;
        let mut url = reqwest::Url::parse(url)
            .map_err(|e| RemoteError::Other(format!("bad download link: {e}")))?;
        let range = match len {
            Some(l) => format!("bytes={}-{}", offset, offset + l - 1),
            None => format!("bytes={offset}-"),
        };
        let mut resp = None;
        for _ in 0..6 {
            let mut req = self.cdn.get(url.clone()).header(header::RANGE, &range);
            // Like rclone, the session headers go to uloz.to's own hosts only, never across
            // a redirect to a third-party host.
            if is_uloz_host(&url) {
                req = req
                    .header("X-Auth-Token", &self.cfg.app_token)
                    .header("X-User-Token", &session.token);
            }
            let r = req.send().await.map_err(transfer_err)?;
            if r.status().is_redirection() {
                let loc = r
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .ok_or_else(|| RemoteError::Other("redirect without location".into()))?;
                let next = url
                    .join(loc)
                    .map_err(|e| RemoteError::Other(e.to_string()))?;
                if url.scheme() == "https" && next.scheme() != "https" {
                    return Err(RemoteError::Other(
                        "refusing HTTPS to HTTP download redirect".into(),
                    ));
                }
                url = next;
                continue;
            }
            resp = Some(r);
            break;
        }
        let resp = resp.ok_or_else(|| RemoteError::Other("too many download redirects".into()))?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(map_status(status, &api::ApiError::default(), &text));
        }
        if let Some(ct) = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            && ct.starts_with("text/html")
        {
            // uloz.to answers with its "slow download" web page instead of the file when the
            // account isn't allowed to download at full speed (rclone issue #8951).
            return Err(RemoteError::DownloadNotAllowed(
                "uloz.to returned a web page instead of the file".into(),
            ));
        }
        // If the server ignored the Range header, skip and truncate ourselves.
        let mut skip = if status == StatusCode::PARTIAL_CONTENT {
            0
        } else {
            offset
        };
        let mut remaining = len;
        let body = resp
            .bytes_stream()
            .map(|r| r.map_err(std::io::Error::other));
        let stream = body.filter_map(move |chunk| {
            let out = match chunk {
                Err(e) => Some(Err(e)),
                Ok(mut b) => {
                    if skip > 0 {
                        let d = skip.min(b.len() as u64);
                        b = b.slice(d as usize..);
                        skip -= d;
                    }
                    if let Some(rem) = remaining.as_mut() {
                        if b.len() as u64 > *rem {
                            b = b.slice(..*rem as usize);
                        }
                        *rem -= b.len() as u64;
                    }
                    if b.is_empty() {
                        None
                    } else {
                        Some(Ok::<Bytes, std::io::Error>(b))
                    }
                }
            };
            futures::future::ready(out)
        });
        Ok(Box::pin(stream))
    }
}

/// The raw uloz.to remote rooted at one folder (the vault). Names and bytes are stored as given.
pub struct UlozRemote {
    client: Arc<UlozClient>,
    root: String,
}

impl UlozRemote {
    pub fn new(client: Arc<UlozClient>, root_folder_slug: String) -> Self {
        Self {
            client,
            root: root_folder_slug,
        }
    }

    pub fn client(&self) -> &Arc<UlozClient> {
        &self.client
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    fn file_entry(f: api::File) -> RemoteEntry {
        let meta = f.description.as_deref().and_then(meta::decode);
        let mtime_ms = match meta {
            Some(m) if m.mtime_micros != 0 => m.mtime_micros / 1000,
            _ => f
                .last_user_modified
                .or(f.upload_time)
                .map(|t| t.timestamp_millis())
                .unwrap_or(0),
        };
        RemoteEntry {
            id: f.slug,
            parent_id: f.folder_slug.unwrap_or_default(),
            kind: Kind::File,
            name: f.name,
            size: f.filesize.max(0) as u64,
            mtime_ms,
        }
    }

    fn folder_entry(f: api::Folder) -> RemoteEntry {
        RemoteEntry {
            id: f.slug,
            parent_id: f.parent_folder_slug.unwrap_or_default(),
            kind: Kind::Folder,
            name: f.name,
            size: 0,
            mtime_ms: f
                .last_user_modified
                .map(|t| t.timestamp_millis())
                .unwrap_or(0),
        }
    }

    /// Finds the named folder directly under the account root, creating it if `create`.
    pub async fn find_or_create_vault(
        client: &UlozClient,
        name: &str,
        create: bool,
    ) -> Result<Option<String>, RemoteError> {
        let s = client.session().await?;
        let folders = client.list_folders(&s.root_folder_slug, false).await?;
        if let Some(f) = folders.into_iter().find(|f| f.name == name) {
            return Ok(Some(f.slug));
        }
        if !create {
            return Ok(None);
        }
        Ok(Some(
            client.create_folder(&s.root_folder_slug, name).await?.slug,
        ))
    }
}

#[async_trait]
impl Remote for UlozRemote {
    async fn scan(&self) -> Result<RemoteTree, RemoteError> {
        let (folders, files) = self.client.list_tree(&self.root).await?;
        let mut entries: Vec<RemoteEntry> = folders.into_iter().map(Self::folder_entry).collect();
        entries.extend(
            files
                .into_iter()
                .filter(|f| !f.is_in_trash && !f.is_incomplete)
                .map(Self::file_entry),
        );
        Ok(RemoteTree {
            root_id: self.root.clone(),
            entries,
            skipped: vec![],
        })
    }

    async fn create_folder(&self, parent_id: &str, name: &str) -> Result<RemoteEntry, RemoteError> {
        Ok(Self::folder_entry(
            self.client.create_folder(parent_id, name).await?,
        ))
    }

    async fn upload(
        &self,
        parent_id: &str,
        name: &str,
        body: UploadBody,
    ) -> Result<RemoteEntry, RemoteError> {
        let mtime = body.mtime_ms;
        let mut e = Self::file_entry(self.client.upload(parent_id, name, body).await?);
        e.mtime_ms = mtime;
        Ok(e)
    }

    async fn read(
        &self,
        file: &RemoteEntry,
        offset: u64,
        len: Option<u64>,
    ) -> Result<ByteStream, RemoteError> {
        if len == Some(0) || (file.size > 0 && offset >= file.size) {
            return Ok(Box::pin(futures::stream::empty()));
        }
        self.client.read(&file.id, offset, len).await
    }

    async fn rename(
        &self,
        entry: &RemoteEntry,
        new_parent_id: &str,
        new_name: &str,
    ) -> Result<RemoteEntry, RemoteError> {
        let mut out = entry.clone();
        match entry.kind {
            Kind::File => {
                let req = api::UpdateFileRequest {
                    name: (new_name != entry.name).then_some(new_name),
                    folder_slug: (new_parent_id != entry.parent_id).then_some(new_parent_id),
                    description: None,
                };
                if req.name.is_some() || req.folder_slug.is_some() {
                    self.client.update_file(&entry.id, &req).await?;
                }
            }
            Kind::Folder => {
                if new_parent_id != entry.parent_id {
                    self.client.move_folder(&entry.id, new_parent_id).await?;
                }
                if new_name != entry.name {
                    self.client.rename_folder(&entry.id, new_name).await?;
                }
            }
        }
        out.parent_id = new_parent_id.to_string();
        out.name = new_name.to_string();
        Ok(out)
    }

    async fn trash(&self, entry: &RemoteEntry) -> Result<(), RemoteError> {
        let r = match entry.kind {
            Kind::File => self.client.trash_file(&entry.id).await,
            Kind::Folder => self.client.delete_folder(&entry.id).await,
        };
        match r {
            Err(RemoteError::NotFound(_)) => Ok(()), // already gone
            other => other,
        }
    }

    async fn exists(&self, entry: &RemoteEntry) -> Result<bool, RemoteError> {
        match entry.kind {
            Kind::File => self.client.file_exists(&entry.id).await,
            Kind::Folder => Ok(true),
        }
    }

    async fn upload_resumable(
        &self,
        parent_id: &str,
        name: &str,
        mut up: crate::remote::resumable::ResumableUpload,
    ) -> Result<RemoteEntry, RemoteError> {
        let mtime = up.mtime_ms;
        if up.source.len() >= self.client.cfg.chunked_min_size && self.client.chunked_available() {
            match self.client.upload_chunked(parent_id, name, &up).await {
                Ok(f) => {
                    let mut e = Self::file_entry(f);
                    e.mtime_ms = mtime;
                    return Ok(e);
                }
                Err(RemoteError::Other(m)) if m.starts_with("chunked uploads unavailable") => {
                    debug!("{m}; uploading {name} in one request");
                }
                Err(e) => return Err(e),
            }
        }
        up.state.remove(chunked::LAYER);
        crate::remote::resumable::upload_whole(self, parent_id, name, up).await
    }

    async fn discard_resumable(&self, state: &crate::remote::resumable::ResumeState) {
        if let Some(st) = state.get::<chunked::ChunkedState>(chunked::LAYER) {
            self.client.discard_batch(&st).await;
        }
    }

    fn resumable_min_size(&self) -> Option<u64> {
        self.client
            .chunked_available()
            .then_some(self.client.cfg.chunked_min_size)
    }

    fn resume_info(
        &self,
        state: &crate::remote::resumable::ResumeState,
    ) -> Option<crate::remote::resumable::ResumeInfo> {
        let (confirmed, total) = chunked::confirmed_bytes(state).unwrap_or((0, 0));
        let assembled_id = chunked::finished_slug(state);
        (total > 0 || assembled_id.is_some()).then_some(crate::remote::resumable::ResumeInfo {
            confirmed,
            total,
            assembled_id,
        })
    }
}
