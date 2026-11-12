//! Platforms without a file-system integration (e.g. macOS) can still use the CLI.

use std::path::Path;

use anyhow::{Result, bail};

use super::{Platform, PlatformContext};

pub fn prepare(_ctx: PlatformContext) -> Result<Box<dyn Platform>> {
    bail!("Drive does not support this operating system yet")
}

pub fn teardown(_folder: &Path, _cache_dir: &Path) -> Result<()> {
    Ok(())
}

/// Folders can't be made on-demand in place on this platform.
pub fn cloud_only(_folder: &Path) -> Result<(u64, u64)> {
    Ok((0, 0))
}

pub fn release(_folder: &Path, _account: &str) -> Result<()> {
    Ok(())
}
