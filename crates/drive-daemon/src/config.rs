//! Persistent configuration (`config.json`) and secrets (`secrets.bin`).
//!
//! Secrets — the uloz.to password and the vault encryption password — are protected with
//! DPAPI on Windows (bound to the user account) and stored in a `0600` file on Linux, the
//! same protection SSH keys get.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use drive_core::ipc::Settings;
use serde::{Deserialize, Serialize};

pub const APP_DIR: &str = "Drive";

/// Where config, the state database and logs live.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn detect() -> Result<Self> {
        if let Some(dir) = std::env::var_os("DRIVE_HOME") {
            // Used by tests and for running several isolated instances.
            let base = PathBuf::from(dir);
            return Ok(Self {
                config_dir: base.join("config"),
                data_dir: base.join("data"),
                cache_dir: base.join("cache"),
            });
        }
        let config_dir = dirs::config_dir()
            .context("no config directory")?
            .join(APP_DIR);
        let data_dir = dirs::data_local_dir()
            .context("no data directory")?
            .join(APP_DIR);
        let cache_dir = dirs::cache_dir()
            .context("no cache directory")?
            .join(APP_DIR);
        Ok(Self {
            config_dir,
            data_dir,
            cache_dir,
        })
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [&self.config_dir, &self.data_dir, &self.cache_dir] {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }
    pub fn secrets_file(&self) -> PathBuf {
        self.config_dir.join("secrets.bin")
    }
    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("state.db")
    }
    pub fn log_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub device_id: String,
    pub device_name: String,
    /// Set once an account is connected.
    pub account: Option<Account>,
    pub folder: Option<PathBuf>,
    pub cache_limit_bytes: u64,
    pub poll_interval_secs: u64,
    pub upload_limit_kbps: u64,
    pub download_limit_kbps: u64,
    pub max_parallel_transfers: u32,
    pub launch_at_login: bool,
    pub notifications: bool,
    /// A dedicated uloz.to API key; `None` uses the shared default.
    pub app_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub login: String,
    /// Slug of the vault folder in the uloz.to account.
    pub vault_slug: String,
    /// Encryption salt (rclone `password2`); derived from the login when the vault was made.
    pub salt: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            device_id: format!("drive-{:016x}", rand::random::<u64>()),
            device_name: default_device_name(),
            account: None,
            folder: None,
            cache_limit_bytes: default_cache_limit(),
            poll_interval_secs: 120,
            upload_limit_kbps: 0,
            download_limit_kbps: 0,
            max_parallel_transfers: 3,
            launch_at_login: true,
            notifications: true,
            app_token: None,
        }
    }
}

fn default_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "This PC".into())
}

/// 10 GiB, but never more than 10% of the disk holding the user's home.
fn default_cache_limit() -> u64 {
    let ten_gib = 10 * 1024 * 1024 * 1024u64;
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    match fs_total_space(&home) {
        Some(total) => ten_gib.min(total / 10).max(1024 * 1024 * 1024),
        None => ten_gib,
    }
}

fn fs_total_space(p: &Path) -> Option<u64> {
    fs4::total_space(p).ok()
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let file = paths.config_file();
        match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", file.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let cfg = Config::default();
                cfg.save(paths)?;
                Ok(cfg)
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let file = paths.config_file();
        let tmp = file.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &file).with_context(|| format!("writing {}", file.display()))?;
        Ok(())
    }

    pub fn folder_or_default(&self) -> PathBuf {
        self.folder
            .clone()
            .unwrap_or_else(crate::platform::default_folder)
    }

    pub fn settings(&self) -> Settings {
        Settings {
            folder: self.folder_or_default().display().to_string(),
            cache_limit_bytes: self.cache_limit_bytes,
            poll_interval_secs: self.poll_interval_secs,
            upload_limit_kbps: self.upload_limit_kbps,
            download_limit_kbps: self.download_limit_kbps,
            max_parallel_transfers: self.max_parallel_transfers,
            launch_at_login: self.launch_at_login,
            notifications: self.notifications,
            device_name: self.device_name.clone(),
            app_token_custom: self.app_token.is_some(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Secrets {
    pub uloz_password: String,
    pub encryption_password: String,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(..)")
    }
}

impl Secrets {
    pub fn load(paths: &Paths) -> Result<Option<Self>> {
        let file = paths.secrets_file();
        let data = match std::fs::read(&file) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).context("reading secrets"),
        };
        let plain = protect::unprotect(&data).context("decrypting secrets")?;
        Ok(Some(serde_json::from_slice(&plain)?))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let file = paths.secrets_file();
        let data = protect::protect(&serde_json::to_vec(self)?)?;
        write_private(&file, &data)
    }

    pub fn delete(paths: &Paths) {
        let _ = std::fs::remove_file(paths.secrets_file());
    }
}

/// Writes a file readable only by the current user.
fn write_private(file: &Path, data: &[u8]) -> Result<()> {
    let tmp = file.with_extension("tmp");
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, file)?;
    Ok(())
}

#[cfg(windows)]
mod protect {
    use anyhow::{Result, bail};
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    const ENTROPY: &[u8] = b"com.samcannas.drive/secrets/v1";

    fn blob(data: &[u8]) -> CRYPT_INTEGER_BLOB {
        CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        }
    }

    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )?;
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            Ok(bytes)
        }
    }

    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        let input = blob(data);
        let entropy = blob(ENTROPY);
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            if CryptUnprotectData(
                &input,
                None,
                Some(&entropy),
                None,
                None,
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut out,
            )
            .is_err()
            {
                bail!("secrets were saved by another Windows user or machine");
            }
            let bytes = std::slice::from_raw_parts(out.pbData, out.cbData as usize).to_vec();
            let _ = LocalFree(Some(HLOCAL(out.pbData as _)));
            Ok(bytes)
        }
    }
}

#[cfg(not(windows))]
mod protect {
    use anyhow::Result;

    // The file itself is 0600; there is no user keyring we can rely on headless.
    pub fn protect(data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }
    pub fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
        Ok(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().into(),
            data_dir: dir.path().into(),
            cache_dir: dir.path().into(),
        };
        assert!(Secrets::load(&paths).unwrap().is_none());
        let s = Secrets {
            uloz_password: "a".into(),
            encryption_password: "b".into(),
        };
        s.save(&paths).unwrap();
        let raw = std::fs::read(paths.secrets_file()).unwrap();
        if cfg!(windows) {
            assert!(
                !String::from_utf8_lossy(&raw).contains("\"b\""),
                "secrets must not be stored in plain text on Windows"
            );
        }
        let back = Secrets::load(&paths).unwrap().unwrap();
        assert_eq!(back.encryption_password, "b");
    }

    #[test]
    fn config_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: dir.path().into(),
            data_dir: dir.path().into(),
            cache_dir: dir.path().into(),
        };
        let mut c = Config::load(&paths).unwrap();
        c.cache_limit_bytes = 123;
        c.save(&paths).unwrap();
        assert_eq!(Config::load(&paths).unwrap().cache_limit_bytes, 123);
    }
}
