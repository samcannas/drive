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
