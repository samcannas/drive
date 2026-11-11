//! Connection to the Drive service: connects (starting the service if needed), reconnects
//! when the socket drops, and forwards service events to the webviews and the tray.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use drive_core::ipc::{Call, Event, Hello, Settings, Status, SyncState, TransferList};
use drive_core::ipc_client::{Client, ClientError};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::{Notify, RwLock, broadcast};

use crate::mock::Mock;

pub const EV_STATUS: &str = "drive://status";
pub const EV_ACTIVITY: &str = "drive://activity";
pub const EV_ISSUES: &str = "drive://issues";
pub const EV_CONNECTION: &str = "drive://connection";
pub const EV_SETTINGS: &str = "drive://settings";
/// The per-file transfer list. Sent only to the popover, and only while it is visible: the
/// service pushes it about once a second during transfers, and a hidden popover has no use
/// for it (it fetches the list once with the `transfers` command when it is shown).
pub const EV_TRANSFERS: &str = "drive://transfers";

/// Error returned to the webview. `code` is an `ipc::ErrorCode` in snake_case, or
/// `unavailable` / `timeout` when the service could not be reached.
#[derive(Debug, Clone, Serialize)]
pub struct CallError {
    pub code: String,
    pub message: String,
}

impl CallError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
    fn unavailable() -> Self {
        Self::new(
            "unavailable",
            "Drive isn't running. It will reconnect automatically.",
        )
    }
}

impl From<ClientError> for CallError {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Rpc(rpc) => {
                let code = serde_json::to_value(rpc.code)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "internal".into());
                Self {
                    code,
                    message: rpc.message,
                }
            }
            ClientError::NotRunning(_) | ClientError::Disconnected => Self::unavailable(),
            ClientError::Decode(m) => Self::new("internal", m),
        }
    }
}

/// One live connection: the real service or the development mock.
#[derive(Clone)]
pub enum Conn {
    Real(Client),
    Mock(Arc<Mock>),
}

impl Conn {
    async fn call_value(&self, call: Call) -> Result<serde_json::Value, CallError> {
        let limit = match call {
            // These wait on uloz.to and on key derivation (scrypt).
            Call::Setup(_) | Call::CheckAccount(_) | Call::ExportRecovery | Call::SignOut => {
                Duration::from_secs(180)
            }
            _ => Duration::from_secs(20),
        };
        let fut = async {
            match self {
                Conn::Real(c) => c.call_value(call).await.map_err(CallError::from),
                Conn::Mock(m) => m
                    .call(call)
                    .await
                    .map_err(|e| CallError::from(ClientError::Rpc(e))),
            }
        };
        tokio::time::timeout(limit, fut).await.unwrap_or_else(|_| {
            Err(CallError::new(
                "timeout",
                "Drive didn't respond in time. Try again.",
            ))
        })
    }

    async fn subscribe(&self) -> Result<broadcast::Receiver<Event>, CallError> {
        match self {
            Conn::Real(c) => c.subscribe().await.map_err(CallError::from),
            Conn::Mock(m) => Ok(m.subscribe()),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Link {
    /// Connecting or reconnecting to the service.
    #[default]
    Connecting,
    Connected,
    /// The service could not be started or reached for a while (still retrying).
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ConnectionInfo {
    pub state: Link,
    /// True when the development mock answers instead of the real service.
    pub mock: bool,
    pub service_version: Option<String>,
}

pub struct Backend {
    pub app: AppHandle,
    conn: RwLock<Option<Conn>>,
    info: Mutex<ConnectionInfo>,
    status: Mutex<Status>,
    settings: Mutex<Option<Settings>>,
    wake: Notify,
    /// Set while quitting or updating: don't start the service again when it goes away.
    stopping: AtomicBool,
}

impl Backend {
    pub fn new(app: AppHandle) -> Self {
        Self {
            app,
            conn: RwLock::new(None),
            info: Mutex::new(ConnectionInfo::default()),
            status: Mutex::new(Status {
                headline: "Starting…".into(),
                ..Default::default()
            }),
            settings: Mutex::new(None),
            wake: Notify::new(),
            stopping: AtomicBool::new(false),
        }
    }

    pub fn start(self: &Arc<Self>, autostarted: bool) {
        let me = self.clone();
        tauri::async_runtime::spawn(async move { me.supervise(autostarted).await });
        crate::tray::start_animation(self.app.clone());
    }

    pub fn connection(&self) -> ConnectionInfo {
        self.info.lock().unwrap().clone()
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    pub fn cached_settings(&self) -> Option<Settings> {
        self.settings.lock().unwrap().clone()
    }

    /// Stop restarting the service from now on (it is being shut down on purpose).
    pub fn stop_supervising(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    /// Start (and keep) the service running again, e.g. after an update failed to install.
    pub fn resume_supervising(&self) {
        self.stopping.store(false, Ordering::SeqCst);
        self.wake.notify_one();
    }

    /// Skip the current backoff and try to connect now.
    pub fn reconnect_now(&self) {
        self.wake.notify_one();
    }

    pub async fn call<T: DeserializeOwned>(&self, call: Call) -> Result<T, CallError> {
        let v = self.call_value(call).await?;
        serde_json::from_value(v).map_err(|e| CallError::new("internal", e.to_string()))
    }

    pub async fn call_value(&self, call: Call) -> Result<serde_json::Value, CallError> {
        let conn = self.conn.read().await.clone();
        match conn {
            Some(c) => c.call_value(call).await,
            None => Err(CallError::unavailable()),
        }
    }

    async fn supervise(self: Arc<Self>, autostarted: bool) {
        if crate::mock::forced() {
            tracing::info!("DRIVE_UI_MOCK is set: using the mock service");
            self.run_mock(autostarted).await;
            return;
        }
        let started = Instant::now();
        let mut spawned_at: Option<Instant> = None;
        let mut backoff = Duration::from_millis(200);
        let mut ever_connected = false;
        let mut first = true;
        loop {
            match Client::connect().await {
                Ok(client) => {
                    ever_connected = true;
                    backoff = Duration::from_millis(200);
                    self.session(Conn::Real(client), &mut first, autostarted)
                        .await;
                    tracing::warn!("lost connection to the Drive service; reconnecting");
                    self.set_link(Link::Connecting, None);
                    // The service may have crashed: allow starting it again right away.
                    spawned_at = None;
                }
                Err(e) => {
                    let respawn = spawned_at.is_none_or(|t| t.elapsed() > Duration::from_secs(30));
                    if respawn && !self.stopping.load(Ordering::SeqCst) {
                        spawned_at = Some(Instant::now());
                        if let Err(err) = crate::platform::spawn_daemon() {
                            tracing::warn!("could not start the Drive service: {err:#}");
                        }
                    }
                    if cfg!(debug_assertions)
                        && !ever_connected
                        && started.elapsed() > Duration::from_secs(4)
                    {
                        tracing::warn!(
                            "Drive service unreachable ({e}); debug build falls back to the mock"
                        );
                        self.run_mock(autostarted).await;
                        return;
                    }
                    if started.elapsed() > Duration::from_secs(10) || ever_connected {
                        let state =
                            if spawned_at.is_some_and(|t| t.elapsed() > Duration::from_secs(8)) {
                                Link::Unavailable
                            } else {
                                Link::Connecting
                            };
                        self.set_link(state, None);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = self.wake.notified() => { spawned_at = None; }
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                }
            }
        }
    }

    async fn run_mock(self: &Arc<Self>, autostarted: bool) {
        self.info.lock().unwrap().mock = true;
        let mock = Mock::new();
        let mut first = true;
        // The mock never disconnects, but loop for symmetry with the real service.
        loop {
            self.session(Conn::Mock(mock.clone()), &mut first, autostarted)
                .await;
        }
    }

    /// Runs one connection until it drops.
    async fn session(self: &Arc<Self>, conn: Conn, first: &mut bool, autostarted: bool) {
        let hello: Hello = match conn.call_value(Call::Hello).await.and_then(|v| {
            serde_json::from_value(v).map_err(|e| CallError::new("internal", e.to_string()))
        }) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!("handshake failed: {}", e.message);
                tokio::time::sleep(Duration::from_secs(1)).await;
                return;
            }
        };
        if hello.protocol != drive_core::ipc::PROTOCOL_VERSION {
            tracing::warn!(
                "service speaks protocol {}, app expects {}",
                hello.protocol,
                drive_core::ipc::PROTOCOL_VERSION
            );
        }
        let mut rx = match conn.subscribe().await {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!("subscribe failed: {}", e.message);
                tokio::time::sleep(Duration::from_secs(1)).await;
                return;
            }
        };
        *self.conn.write().await = Some(conn.clone());
        self.set_link(Link::Connected, Some(hello.version));
        self.refresh_status().await;
        self.refresh_settings().await;
        crate::notify::check_issues(self).await;

        if std::mem::take(first) {
            let state = self.status().state;
            if state == SyncState::NotSetUp {
                crate::windows::open_onboarding(&self.app);
            } else if !autostarted && cfg!(debug_assertions) {
                // Developers launching the app want to see it.
                crate::windows::show_main(&self.app, None);
            }
        }

        // The client keeps its event sender alive after the socket closes, so a periodic
        // status call is what notices a dead connection.
        let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
        heartbeat.tick().await;
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(ev) => self.handle_event(ev).await,
                    Err(broadcast::error::RecvError::Lagged(_)) => self.refresh_status().await,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                _ = heartbeat.tick() => {
                    match conn.call_value(Call::Status).await {
                        Ok(v) => if let Ok(s) = serde_json::from_value::<Status>(v) { self.set_status(s) },
                        Err(e) if e.code == "unavailable" || e.code == "timeout" => break,
                        Err(_) => {}
                    }
                }
            }
        }
        *self.conn.write().await = None;
    }

    async fn handle_event(self: &Arc<Self>, ev: Event) {
        match ev {
            Event::Status(s) => self.set_status(s),
            Event::Activity(a) => {
                let _ = self.app.emit(EV_ACTIVITY, a);
            }
            Event::IssuesChanged { count } => {
                let _ = self.app.emit(EV_ISSUES, count);
                crate::notify::check_issues(self).await;
            }
            Event::Transfers(list) => self.forward_transfers(&list),
        }
    }

    fn forward_transfers(&self, list: &TransferList) {
        let visible = self
            .app
            .get_webview_window(crate::windows::POPOVER)
            .is_some_and(|w| w.is_visible().unwrap_or(false));
        if visible {
            let _ = self
                .app
                .emit_to(crate::windows::POPOVER, EV_TRANSFERS, list);
        }
    }

    pub async fn refresh_status(&self) {
        if let Ok(s) = self.call::<Status>(Call::Status).await {
            self.set_status(s);
        }
    }

    pub async fn refresh_settings(&self) {
        if let Ok(s) = self.call::<Settings>(Call::GetSettings).await {
            self.set_settings(s);
        }
    }

    pub fn set_settings(&self, s: Settings) {
        let (changed_autostart, changed_auto_update) = {
            let mut cur = self.settings.lock().unwrap();
            let autostart = cur
                .as_ref()
                .is_none_or(|c| c.launch_at_login != s.launch_at_login);
            let auto_update = cur.as_ref().is_none_or(|c| c.auto_update != s.auto_update);
            *cur = Some(s.clone());
            (autostart, auto_update)
        };
        if changed_autostart && !self.connection().mock {
            crate::platform::apply_autostart(&self.app, s.launch_at_login);
        }
        if changed_auto_update {
            crate::updater::settings_changed(&self.app, s.auto_update);
        }
        let _ = self.app.emit(EV_SETTINGS, s);
    }

    fn set_status(&self, s: Status) {
        let changed = {
            let mut cur = self.status.lock().unwrap();
            let changed = serde_json::to_value(&*cur).ok() != serde_json::to_value(&s).ok();
            *cur = s.clone();
            changed
        };
        if changed {
            let _ = self.app.emit(EV_STATUS, &s);
            crate::tray::update(&self.app, &s, &self.connection());
        }
    }

    fn set_link(&self, state: Link, version: Option<String>) {
        let info = {
            let mut info = self.info.lock().unwrap();
            if info.state == state && version.is_none() {
                return;
            }
            info.state = state;
            if version.is_some() {
                info.service_version = version;
            }
            info.clone()
        };
        let _ = self.app.emit(EV_CONNECTION, &info);
        crate::tray::update(&self.app, &self.status(), &info);
    }
}
