//! Placeholder until the platform integration lands.

use std::path::Path;

use anyhow::{Result, bail};

use super::{Platform, PlatformContext};

pub fn prepare(_ctx: PlatformContext) -> Result<Box<dyn Platform>> {
    bail!("this platform integration is not implemented yet")
}

pub fn teardown(_folder: &Path, _cache_dir: &Path) -> Result<()> {
    Ok(())
}
