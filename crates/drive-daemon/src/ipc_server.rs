//! Local-socket server for the IPC protocol (one task per client connection).

#![allow(deprecated)] // split halves; see drive_core::ipc_client

use std::io;
use std::sync::Arc;

use anyhow::{Context, Result};
use drive_core::ipc::{ErrorCode, Request, Response};
use drive_core::ipc_client::socket_name;
use interprocess::local_socket::ListenerOptions;
use interprocess::local_socket::tokio::{Listener, SendHalf, Stream, prelude::*};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Mutex;
use tracing::{debug, warn};

use crate::service::Service;

/// Binds the socket. Fails with `AddrInUse` if another service instance is running.
pub async fn bind() -> Result<Listener> {
    // A live service answers; a stale Unix socket file doesn't and can be replaced.
    if Stream::connect(socket_name()?).await.is_ok() {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "the Drive service is already running").into());
    }
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(drive_core::ipc_client::unix_socket_path());
    }
    let listener = ListenerOptions::new().name(socket_name()?).create_tokio().context("binding the service socket")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(drive_core::ipc_client::unix_socket_path(), std::fs::Permissions::from_mode(0o600));
    }
    Ok(listener)
}

pub async fn serve(listener: Listener, service: Arc<Service>) {
    loop {
        match listener.accept().await {
            Ok(stream) => {
                let svc = service.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_conn(stream, svc).await {
                        debug!("client disconnected: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("accepting a client failed: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

async fn write_line(w: &Mutex<SendHalf>, value: &impl serde::Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    w.lock().await.write_all(&line).await
}

async fn handle_conn(stream: Stream, service: Arc<Service>) -> io::Result<()> {
    let (recv, send) = stream.split();
    let writer = Arc::new(Mutex::new(send));
    let mut lines = BufReader::new(recv).lines();
    let mut forwarder: Option<tokio::task::JoinHandle<()>> = None;
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let id = serde_json::from_str::<serde_json::Value>(&line).ok().and_then(|v| v.get("id")?.as_u64()).unwrap_or(0);
                write_line(&writer, &Response::err(id, ErrorCode::BadRequest, format!("bad request: {e}"))).await?;
                continue;
            }
        };
        if matches!(req.call, drive_core::ipc::Call::Subscribe) && forwarder.is_none() {
            let mut rx = service.subscribe();
            let w = writer.clone();
            forwarder = Some(tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(ev) => {
                            if write_line(&w, &ev).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
            }));
        }
        // Handle each request concurrently so a slow call (setup) doesn't block status polls.
        let svc = service.clone();
        let w = writer.clone();
        tokio::spawn(async move {
            let id = req.id;
            let resp = match svc.handle(req.call).await {
                Ok(v) => Response { id, result: Some(v), error: None },
                Err(e) => Response { id, result: None, error: Some(e) },
            };
            let _ = write_line(&w, &resp).await;
        });
    }
    if let Some(f) = forwarder {
        f.abort();
    }
    Ok(())
}
