//! File-system integration per platform.
//!
//! * Windows — `windows/`: a Cloud Files API sync root (placeholders, Explorer status icons,
//!   "Always keep on this device" / "Free up space").
//! * Linux — `linux/`: a FUSE mount backed by an on-disk content cache.
//!
//! Lifecycle, driven by the service:
//! 1. [`prepare`] builds the platform object for an engine and folder (nothing visible yet).
//! 2. The service calls `engine.set_local(platform.local_side())` and `engine.start()`.
//! 3. [`Platform::start`] makes the folder live (connects the sync root / mounts).
//! 4. [`Platform::stop`] on shutdown; [`teardown`] on sign-out removes all traces.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use drive_core::engine::Engine;
use drive_core::engine::local::LocalSide;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as imp;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use self::linux as imp;

#[cfg(not(any(windows, target_os = "linux")))]
mod unsupported;
#[cfg(not(any(windows, target_os = "linux")))]
use self::unsupported as imp;

/// Platform integration for one connected account.
pub trait Platform: Send + Sync {
    /// The engine's view of the local side. Called once, before the engine starts.
    fn local_side(&self) -> Arc<dyn LocalSide>;
    /// Makes the folder live. Called after the engine has started.
    fn start(&self) -> Result<()>;
    /// Stops serving the folder (disconnect / unmount). Idempotent.
    fn stop(&self);
}

/// Everything a platform implementation gets to work with.
pub struct PlatformContext {
    pub engine: Arc<Engine>,
    /// Windows sync root, or Linux mount point.
    pub folder: PathBuf,
    /// Directory the platform may use for its own data (e.g. the Linux content cache).
    pub cache_dir: PathBuf,
    /// Shown in Explorer's navigation pane / mount name.
    pub display_name: String,
    /// The uloz.to login, used to make the Windows sync root id unique per account.
    pub account: String,
}

pub fn prepare(ctx: PlatformContext) -> Result<Box<dyn Platform>> {
    imp::prepare(ctx)
}

/// Removes everything the integration created for `folder` (sign-out). Files that were never
/// uploaded are left in place.
pub fn teardown(folder: &Path, cache_dir: &Path) -> Result<()> {
    imp::teardown(folder, cache_dir)
}

/// `~/Drive` (Windows: `%USERPROFILE%\Drive`).
pub fn default_folder() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("Drive")
}
