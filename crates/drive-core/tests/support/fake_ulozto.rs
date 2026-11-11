//! A fake uloz.to (API + resumable upload host + download CDN) on localhost, with fault
//! injection. Implements the subset of the protocol Drive uses, following
//! https://uloz.to/upload-resumable-api-beta and the public OpenAPI spec.

#![allow(dead_code, clippy::result_large_err)]

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use md5::Digest as _;
use serde_json::{Value, json};

pub const LOGIN: &str = "me";
pub const TOKEN: &str = "user-token";
pub const ROOT: &str = "root";

#[derive(Default, Debug, Clone)]
pub struct Faults {
    /// Next N chunk posts answer 500.
    pub fail_chunks: u32,
    /// Next N chunk posts answer 429.
    pub throttle_chunks: u32,
    /// The first post of this chunk number never answers.
    pub stall_chunk: Option<u32>,
    /// Every chunk post waits this long before answering.
    pub chunk_delay: Duration,
    /// The assembled file's CRC32 won't match (server reports bad-crc32).
    pub corrupt_assembly: bool,
    /// Status stays "processing" this many polls after the last chunk.
    pub processing_polls: u32,
    /// `/v6/upload/link` answers 404 (chunked uploads unavailable).
    pub chunked_disabled: bool,
    /// Renewing an existing batch fails (it expired for good).
    pub expire_batches: bool,
    /// The next commit is applied but its reply is lost (500).
    pub lose_commit_reply: bool,
}

#[derive(Default, Debug, Clone)]
pub struct Stats {
    pub chunk_posts: u32,
    pub chunk_posts_by_number: BTreeMap<u32, u32>,
    pub concurrent: usize,
    pub max_concurrent: usize,
    pub registers: u32,
    pub single_uploads: u32,
    pub hash_verifies: u32,
    pub discards: u32,
    pub new_batches: u32,
    pub missing_content_length: u32,
}

#[derive(Debug, Clone)]
pub struct FileRec {
    pub name: String,
    pub folder: String,
    pub description: String,
    pub data: Bytes,
    pub committed: bool,
    pub batch: String,
}

#[derive(Debug, Default)]
struct Upload {
    name: String,
    filesize: u64,
    chunksize: u64,
    crc32: Option<u32>,
    chunks: BTreeMap<u32, Bytes>,
    polls_after_complete: u32,
    result: Option<Result<String, String>>,
}

#[derive(Debug, Default)]
struct BatchRec {
    signature: String,
    uploads: HashMap<u32, Upload>,
    committed: bool,
}

#[derive(Default)]
pub struct FakeState {
    pub faults: Faults,
    pub stats: Stats,
    pub files: HashMap<String, FileRec>,
    batches: HashMap<String, BatchRec>,
    next: u64,
    stalled_once: bool,
    url: String,
}

impl FakeState {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{}", self.next)
    }
}

type S = Arc<Mutex<FakeState>>;

pub struct Fake {
    pub url: String,
    pub state: S,
}

impl Fake {
    pub async fn start() -> Self {
        let state: S = Arc::new(Mutex::new(FakeState::default()));
        let app = Router::new()
            .route("/v6/session", put(session))
            .route("/v5/upload/link", post(single_link))
            .route("/single", post(single_upload))
            .route("/v6/upload/link", post(chunked_link))
            .route("/r/v1/chunked-file-upload", post(register))
            .route("/r/v1/chunked-file-upload/{slug}", post(extend))
            .route(
                "/r/v1/chunked-file-upload/{slug}/{bfid}",
                axum::routing::delete(discard),
            )
            .route(
                "/r/v1/chunked-file-upload/{slug}/{bfid}/status",
                get(status),
            )
            .route("/r/v1/chunked-file-upload/{slug}/{bfid}/{n}", post(chunk))
            .route("/v5/file/{slug}/hash-verify", get(hash_verify))
            .route("/v8/file-list/private", patch(set_props))
            .route("/v8/upload-batch/private/{slug}", patch(commit))
            .route("/v7/file/{slug}/private", get(file_detail))
            .route("/v5/file/download-link/vipdata", post(download_link))
            .route("/dl/{slug}", get(download))
            .layer(axum::extract::DefaultBodyLimit::max(512 * 1024 * 1024))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        state.lock().unwrap().url = url.clone();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Fake { url, state }
    }

    pub fn faults(&self, f: impl FnOnce(&mut Faults)) {
        f(&mut self.state.lock().unwrap().faults);
    }

    pub fn stats(&self) -> Stats {
        self.state.lock().unwrap().stats.clone()
    }

    pub fn committed_files(&self) -> Vec<FileRec> {
        self.state
            .lock()
            .unwrap()
            .files
            .values()
            .filter(|f| f.committed)
            .cloned()
            .collect()
    }
}

fn authed(h: &HeaderMap) -> Result<(), Response> {
    if h.get("X-User-Token").and_then(|v| v.to_str().ok()) == Some(TOKEN) {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": 70001, "code": 401, "message": "Invalid user token."})),
        )
            .into_response())
    }
}

fn file_json(slug: &str, f: &FileRec) -> Value {
    json!({
        "slug": slug, "name": f.name, "filesize": f.data.len(), "description": f.description,
        "folder_slug": f.folder, "is_in_trash": false, "status": "ok",
    })
}

async fn session() -> Json<Value> {
    Json(json!({
        "token_id": TOKEN, "token_validity_interval": 3600,
        "session": {"user": {"login": LOGIN, "root_folder_slug": ROOT}},
    }))
}

async fn single_link(State(s): State<S>, h: HeaderMap) -> Response {
    if let Err(r) = authed(&h) {
        return r;
    }
    let mut st = s.lock().unwrap();
    let batch = st.id("batch");
    st.batches.insert(batch.clone(), BatchRec::default());
    let url = format!("{}/single?batch={batch}", st.url);
    Json(json!({"upload_url": url, "private_slug": batch})).into_response()
}

async fn single_upload(
    State(s): State<S>,
    Query(q): Query<HashMap<String, String>>,
    mut mp: Multipart,
) -> Response {
    let batch = q.get("batch").cloned().unwrap_or_default();
    let mut data = Bytes::new();
    let mut name = String::new();
    while let Ok(Some(field)) = mp.next_field().await {
        if field.name() == Some("file") {
            name = field.file_name().unwrap_or("").to_string();
            data = field.bytes().await.unwrap();
        }
    }
    let mut st = s.lock().unwrap();
    st.stats.single_uploads += 1;
    let slug = st.id("f");
    let md5 = hex::encode(md5::Md5::digest(&data));
    st.files.insert(
        slug.clone(),
        FileRec {
            name,
            folder: String::new(),
            description: String::new(),
            data,
            committed: false,
            batch,
        },
    );
    Json(json!({"md5": md5, "slug": slug})).into_response()
}

async fn chunked_link(State(s): State<S>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = authed(&h) {
        return r;
    }
    let mut st = s.lock().unwrap();
    if st.faults.chunked_disabled {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": 404, "code": 404, "message": "Not found"})),
        )
            .into_response();
    }
    let sig = st.id("sig");
    let slug = match body.get("private_slug").and_then(|v| v.as_str()) {
        Some(existing) => {
            if st.faults.expire_batches || !st.batches.contains_key(existing) {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": 404, "code": 404, "message": "Batch not found"})),
                )
                    .into_response();
            }
            st.batches.get_mut(existing).unwrap().signature = sig.clone();
            existing.to_string()
        }
        None => {
            let slug = st.id("batch");
            st.stats.new_batches += 1;
            st.batches.insert(
                slug.clone(),
                BatchRec {
                    signature: sig.clone(),
                    ..Default::default()
                },
            );
            slug
        }
    };
    let base = format!("{}/r/", st.url);
    Json(json!({
        "private_slug": slug, "upload_signature": sig, "upload_resumable_base_url": base,
        "upload_url": "unused", "validity_interval": 3600,
    }))
    .into_response()
}

async fn register(State(s): State<S>, Json(body): Json<Value>) -> Response {
    let mut st = s.lock().unwrap();
    st.stats.registers += 1;
    let sig = body["upload_signature"].as_str().unwrap_or("").to_string();
    let Some((slug, batch)) = st.batches.iter_mut().find(|(_, b)| b.signature == sig) else {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"message": "bad signature", "return_code": 403})),
        )
            .into_response();
    };
    let slug = slug.clone();
    let bfid = body["batch_file_id"].as_u64().unwrap_or(0) as u32;
    batch.uploads.insert(
        bfid,
        Upload {
            name: body["name"].as_str().unwrap_or("").into(),
            filesize: body["filesize"].as_u64().unwrap_or(0),
            chunksize: body["chunksize"].as_u64().unwrap_or(1),
            crc32: body["crc32"].as_u64().map(|v| v as u32),
            ..Default::default()
        },
    );
    let url = format!("{}/r/v1/chunked-file-upload/{slug}/{bfid}/1", st.url);
    Json(json!({"upload_url": url, "return_code": 200})).into_response()
}

async fn extend(State(s): State<S>, Path(slug): Path<String>, Json(body): Json<Value>) -> Response {
    let st = s.lock().unwrap();
    match st.batches.get(&slug) {
        Some(b) if body["upload_signature"].as_str() == Some(b.signature.as_str()) => {
            Json(json!({"message": "Done", "return_code": 200})).into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "cannot extend", "return_code": 400})),
        )
            .into_response(),
    }
}

async fn discard(State(s): State<S>, Path((slug, bfid)): Path<(String, u32)>) -> StatusCode {
    let mut st = s.lock().unwrap();
    st.stats.discards += 1;
    if let Some(b) = st.batches.get_mut(&slug) {
        b.uploads.remove(&bfid);
    }
    StatusCode::NO_CONTENT
}

struct ConcurrencyGuard(S);
impl Drop for ConcurrencyGuard {
    fn drop(&mut self) {
        self.0.lock().unwrap().stats.concurrent -= 1;
    }
}

async fn chunk(
    State(s): State<S>,
    Path((slug, bfid, n)): Path<(String, u32, u32)>,
    h: HeaderMap,
    body: Body,
) -> Response {
    let (delay, stall, fail, throttle) = {
        let mut st = s.lock().unwrap();
        st.stats.chunk_posts += 1;
        *st.stats.chunk_posts_by_number.entry(n).or_default() += 1;
        if !h.contains_key(header::CONTENT_LENGTH) {
            st.stats.missing_content_length += 1;
        }
        st.stats.concurrent += 1;
        st.stats.max_concurrent = st.stats.max_concurrent.max(st.stats.concurrent);
        let stall = st.faults.stall_chunk == Some(n) && !st.stalled_once;
        if stall {
            st.stalled_once = true;
        }
        let fail = st.faults.fail_chunks > 0;
        if fail {
            st.faults.fail_chunks -= 1;
        }
        let throttle = !fail && st.faults.throttle_chunks > 0;
        if throttle {
            st.faults.throttle_chunks -= 1;
        }
        (st.faults.chunk_delay, stall, fail, throttle)
    };
    let _guard = ConcurrencyGuard(s.clone());
    if stall {
        // Accept nothing and never answer (the client's stall watchdog must give up).
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    let data = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(d) => d,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    tokio::time::sleep(delay).await;
    if fail {
        return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
    }
    if throttle {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": 429, "code": 429, "message": "slow down"})),
        )
            .into_response();
    }
    let mut st = s.lock().unwrap();
    let Some(up) = st
        .batches
        .get_mut(&slug)
        .and_then(|b| b.uploads.get_mut(&bfid))
    else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "not registered", "return_code": 400})),
        )
            .into_response();
    };
    let count = up.filesize.div_ceil(up.chunksize).max(1) as u32;
    let expected = if n < count {
        up.chunksize
    } else {
        up.filesize - (count as u64 - 1) * up.chunksize
    };
    if n == 0 || n > count || data.len() as u64 != expected || up.result.is_some() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "bad chunk", "return_code": 400})),
        )
            .into_response();
    }
    up.chunks.insert(n, data);
    Json(json!({"message": "Done", "return_code": 201})).into_response()
}

async fn status(State(s): State<S>, Path((slug, bfid)): Path<(String, u32)>) -> Response {
    let mut guard = s.lock().unwrap();
    let st = &mut *guard;
    let corrupt = st.faults.corrupt_assembly;
    let processing_polls = st.faults.processing_polls;
    let Some(batch) = st.batches.get_mut(&slug) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "unknown", "return_code": 400})),
        )
            .into_response();
    };
    let Some(up) = batch.uploads.get_mut(&bfid) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"message": "not registered", "return_code": 400})),
        )
            .into_response();
    };
    let count = up.filesize.div_ceil(up.chunksize).max(1) as u32;
    let uploaded: Vec<u32> = up.chunks.keys().copied().collect();
    let missing: Vec<u32> = (1..=count).filter(|n| !up.chunks.contains_key(n)).collect();
    if up.result.is_none() && missing.is_empty() {
        if up.polls_after_complete < processing_polls {
            up.polls_after_complete += 1;
        } else {
            let mut data: Vec<u8> = up.chunks.values().flat_map(|c| c.iter().copied()).collect();
            if corrupt && !data.is_empty() {
                data[0] ^= 1;
            }
            let crc_ok = up.crc32.is_none_or(|c| crc32fast::hash(&data) == c);
            if crc_ok {
                let name = up.name.clone();
                st.next += 1;
                let file_slug = format!("f{}", st.next);
                st.files.insert(
                    file_slug.clone(),
                    FileRec {
                        name,
                        folder: String::new(),
                        description: String::new(),
                        data: Bytes::from(data),
                        committed: false,
                        batch: slug.clone(),
                    },
                );
                let up = st
                    .batches
                    .get_mut(&slug)
                    .unwrap()
                    .uploads
                    .get_mut(&bfid)
                    .unwrap();
                up.result = Some(Ok(file_slug));
            } else {
                up.result = Some(Err("bad-crc32".into()));
            }
        }
    }
    let up = st
        .batches
        .get_mut(&slug)
        .unwrap()
        .uploads
        .get_mut(&bfid)
        .unwrap();
    let body = match &up.result {
        Some(Ok(slug)) => {
            json!({"status": "finished", "uploaded_chunks": uploaded, "missing_chunks": [], "irpc": {"slug": slug}, "return_code": 200})
        }
        Some(Err(e)) => {
            json!({"status": e, "uploaded_chunks": uploaded, "missing_chunks": missing, "return_code": 200})
        }
        None => {
            json!({"status": if missing.is_empty() { "processing" } else { "pending" }, "uploaded_chunks": uploaded, "missing_chunks": missing, "return_code": 200})
        }
    };
    Json(body).into_response()
}

async fn hash_verify(
    State(s): State<S>,
    Path(slug): Path<String>,
    h: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Err(r) = authed(&h) {
        return r;
    }
    let mut st = s.lock().unwrap();
    st.stats.hash_verifies += 1;
    let Some(f) = st.files.get(&slug) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let md5 = q
        .get("md5")
        .map(|m| *m == hex::encode(md5::Md5::digest(&f.data)));
    let crc = q
        .get("crc32")
        .map(|c| *c == crc32fast::hash(&f.data).to_string());
    Json(json!({"md5": md5, "crc32": crc})).into_response()
}

async fn set_props(State(s): State<S>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = authed(&h) {
        return r;
    }
    let mut st = s.lock().unwrap();
    let mut out = vec![];
    for slug in body["slugs"].as_array().cloned().unwrap_or_default() {
        let slug = slug.as_str().unwrap_or("").to_string();
        let token = body["upload_tokens"][&slug]
            .as_str()
            .unwrap_or("")
            .to_string();
        let Some(f) = st.files.get_mut(&slug) else {
            return StatusCode::NOT_FOUND.into_response();
        };
        if !token.starts_with(&format!("{}:", f.batch)) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": 1, "code": 403, "message": "bad upload token"})),
            )
                .into_response();
        }
        if f.committed {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": 2, "code": 409, "message": "already committed"})),
            )
                .into_response();
        }
        f.name = body["name"].as_str().unwrap_or(&f.name).to_string();
        f.folder = body["folder_slug"].as_str().unwrap_or("").to_string();
        f.description = body["description"].as_str().unwrap_or("").to_string();
        out.push(file_json(&slug, f));
    }
    Json(Value::Array(out)).into_response()
}

async fn commit(State(s): State<S>, Path(slug): Path<String>, h: HeaderMap) -> Response {
    if let Err(r) = authed(&h) {
        return r;
    }
    let mut st = s.lock().unwrap();
    let lose = std::mem::take(&mut st.faults.lose_commit_reply);
    let Some(b) = st.batches.get_mut(&slug) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if b.committed {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error": 2, "code": 409, "message": "already committed"})),
        )
            .into_response();
    }
    b.committed = true;
    for f in st.files.values_mut().filter(|f| f.batch == slug) {
        f.committed = true;
    }
    if lose {
        return (StatusCode::INTERNAL_SERVER_ERROR, "reply lost").into_response();
    }
    Json(json!({"private_slug": slug, "status": "confirmed"})).into_response()
}

async fn file_detail(State(s): State<S>, Path(slug): Path<String>, h: HeaderMap) -> Response {
    if let Err(r) = authed(&h) {
        return r;
    }
    let st = s.lock().unwrap();
    match st.files.get(&slug).filter(|f| f.committed) {
        Some(f) => Json(file_json(&slug, f)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": 3, "code": 404, "message": "not found"})),
        )
            .into_response(),
    }
}

async fn download_link(State(s): State<S>, h: HeaderMap, Json(body): Json<Value>) -> Response {
    if let Err(r) = authed(&h) {
        return r;
    }
    let st = s.lock().unwrap();
    let slug = body["file_slug"].as_str().unwrap_or("");
    Json(json!({"link": format!("{}/dl/{slug}", st.url)})).into_response()
}

async fn download(State(s): State<S>, Path(slug): Path<String>, h: HeaderMap) -> Response {
    let data = match s.lock().unwrap().files.get(&slug) {
        Some(f) => f.data.clone(),
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    if let Some(range) = h
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|r| r.strip_prefix("bytes="))
    {
        let (a, b) = range.split_once('-').unwrap();
        let start: usize = a.parse().unwrap();
        let end: usize = if b.is_empty() {
            data.len() - 1
        } else {
            b.parse::<usize>().unwrap().min(data.len() - 1)
        };
        return (
            StatusCode::PARTIAL_CONTENT,
            [(header::CONTENT_TYPE, "application/octet-stream")],
            data.slice(start..=end),
        )
            .into_response();
    }
    ([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response()
}
