//! Local-socket transport for the [`crate::ipc`] protocol, used by the tray app and the CLI.

// Split halves are deprecated upstream in favour of sharing the stream, but they keep the
// reader task and writer independent, which is exactly what we want here.
#![allow(deprecated)]

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use interprocess::local_socket::tokio::{RecvHalf, SendHalf, Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, Name, ToFsName, ToNsName};
use serde::de::DeserializeOwned;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast, oneshot};

use crate::ipc::{Call, ErrorCode, Event, Request, Response, RpcError};

/// Filesystem path of the socket on Unix (inside `$XDG_RUNTIME_DIR`, which only the user can read).
pub fn unix_socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join(crate::ipc::socket_name())
}

/// Platform socket name: a named pipe on Windows, a Unix socket path elsewhere.
pub fn socket_name() -> io::Result<Name<'static>> {
    if cfg!(windows) {
        crate::ipc::socket_name().to_ns_name::<GenericNamespaced>().map(|n| n.into_owned())
    } else {
        unix_socket_path().to_fs_name::<GenericFilePath>().map(|n| n.into_owned())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("the Drive service is not running")]
    NotRunning(#[source] io::Error),
    #[error("connection to the Drive service was lost")]
    Disconnected,
    #[error("{}", .0.message)]
    Rpc(RpcError),
    #[error("unexpected reply from the Drive service: {0}")]
    Decode(String),
}

impl ClientError {
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            ClientError::Rpc(e) => Some(e.code),
            _ => None,
        }
    }
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>;

/// A connection to the service. Cheap to clone; all clones share one socket.
#[derive(Clone)]
pub struct Client {
    writer: Arc<Mutex<SendHalf>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    events: broadcast::Sender<Event>,
    closed: Arc<tokio::sync::watch::Sender<bool>>,
}

impl Client {
    pub async fn connect() -> Result<Self, ClientError> {
        let stream = Stream::connect(socket_name().map_err(ClientError::NotRunning)?)
            .await
            .map_err(ClientError::NotRunning)?;
        let (recv, send) = stream.split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(256);
        let closed = Arc::new(tokio::sync::watch::Sender::new(false));
        tokio::spawn(read_loop(recv, pending.clone(), events.clone(), closed.clone()));
        Ok(Self { writer: Arc::new(Mutex::new(send)), pending, next_id: Arc::new(AtomicU64::new(1)), events, closed })
    }

    /// Whether the connection to the service has been lost. Calls fail fast afterwards.
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    /// Resolves once the connection to the service is lost.
    pub async fn closed(&self) {
        let mut rx = self.closed.subscribe();
        let _ = rx.wait_for(|c| *c).await;
    }

    /// Sends a call and waits for its result.
    pub async fn call_value(&self, call: Call) -> Result<serde_json::Value, ClientError> {
        if self.is_closed() {
            return Err(ClientError::Disconnected);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let mut line = serde_json::to_vec(&Request { id, call }).map_err(|e| ClientError::Decode(e.to_string()))?;
        line.push(b'\n');
        {
            let mut w = self.writer.lock().await;
            if w.write_all(&line).await.is_err() {
                self.pending.lock().await.remove(&id);
                return Err(ClientError::Disconnected);
            }
        }
        let resp = rx.await.map_err(|_| ClientError::Disconnected)?;
        match (resp.result, resp.error) {
            (_, Some(err)) => Err(ClientError::Rpc(err)),
            (Some(v), None) => Ok(v),
            (None, None) => Ok(serde_json::Value::Null),
        }
    }

    pub async fn call<T: DeserializeOwned>(&self, call: Call) -> Result<T, ClientError> {
        let v = self.call_value(call).await?;
        serde_json::from_value(v).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// Asks the service to stream events and returns a receiver for them.
    pub async fn subscribe(&self) -> Result<broadcast::Receiver<Event>, ClientError> {
        let rx = self.events.subscribe();
        self.call_value(Call::Subscribe).await?;
        Ok(rx)
    }
}

async fn read_loop(
    recv: RecvHalf,
    pending: Pending,
    events: broadcast::Sender<Event>,
    closed: Arc<tokio::sync::watch::Sender<bool>>,
) {
    let mut lines = BufReader::new(recv).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("event").is_some() {
            if let Ok(ev) = serde_json::from_value::<Event>(value) {
                let _ = events.send(ev);
            }
        } else if let Ok(resp) = serde_json::from_value::<Response>(value)
            && let Some(tx) = pending.lock().await.remove(&resp.id)
        {
            let _ = tx.send(resp);
        }
    }
    // Connection closed: fail every waiter and every future call.
    closed.send_replace(true);
    pending.lock().await.clear();
}
