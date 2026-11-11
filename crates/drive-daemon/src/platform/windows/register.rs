//! Sync root registration: what makes the folder a cloud folder in Explorer (navigation pane
//! entry, status icons, "Always keep on this device" / "Free up space").

use std::path::Path;

use anyhow::{Context, Result, bail};
use cloud_filter::ext::FileExt;
use cloud_filter::root::{
    HydrationPolicy, HydrationType, PopulationType, SecurityId, SupportedAttribute, SyncRootId,
    SyncRootIdBuilder, SyncRootInfo, active_roots,
};
use tracing::{debug, info, warn};
use widestring::U16CString;
use windows::Win32::Storage::CloudFilters::{
    CF_HARDLINK_POLICY_NONE, CF_HYDRATION_POLICY,
    CF_HYDRATION_POLICY_MODIFIER_AUTO_DEHYDRATION_ALLOWED, CF_HYDRATION_POLICY_PROGRESSIVE,
    CF_INSYNC_POLICY_PRESERVE_INSYNC_FOR_SYNC_ENGINE, CF_INSYNC_POLICY_TRACK_FILE_LAST_WRITE_TIME,
    CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT, CF_POPULATION_POLICY,
    CF_POPULATION_POLICY_ALWAYS_FULL, CF_REGISTER_FLAG_UPDATE, CF_SYNC_POLICIES,
    CF_SYNC_REGISTRATION, CfRegisterSyncRoot, CfUnregisterSyncRoot,
};
use windows::core::{GUID, PCWSTR};

/// First component of our sync root ids (`Drive!<SID>!<account>`).
pub const PROVIDER: &str = "Drive";
/// Bump when the registration settings change so existing roots get updated.
const REGISTRATION_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-1");
/// Stable provider GUID for the native (non-WinRT) registration fallback.
const PROVIDER_GUID: GUID = GUID::from_u128(0x6c1d1f7e_4b8a_4f0e_9a59_0d5e2b7c4a31);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Registration {
    /// Registered through `StorageProviderSyncRootManager` (Explorer integration).
    Shell,
    /// Native `CfRegisterSyncRoot` only: placeholders work, but there is no navigation-pane
    /// entry.
    Native,
}

/// The Drive folder must be on NTFS (Cloud Files placeholders are an NTFS feature).
pub fn check_ntfs(folder: &Path) -> Result<()> {
    use windows::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};
    let wide = U16CString::from_os_str(folder.as_os_str()).context("invalid folder path")?;
    let mut volume = [0u16; 1024];
    let mut fs_name = [0u16; 64];
    unsafe {
        GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut volume)
            .with_context(|| format!("finding the drive of {}", folder.display()))?;
        GetVolumeInformationW(
            PCWSTR(volume.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fs_name),
        )
        .with_context(|| format!("reading the file system of {}", folder.display()))?;
    }
    let len = fs_name
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(fs_name.len());
    let fs = String::from_utf16_lossy(&fs_name[..len]);
    if !fs.eq_ignore_ascii_case("NTFS") {
        bail!(
            "The Drive folder must be on an NTFS drive, but {} is on a {fs} drive. Choose a folder on your system drive.",
            folder.display()
        );
    }
    Ok(())
}

/// One sync root per Windows user and uloz.to account.
pub fn sync_root_id(account: &str) -> Result<SyncRootId> {
    let account: String = account
        .chars()
        .map(|c| if c == '!' || c.is_control() { '_' } else { c })
        .take(200)
        .collect();
    let account = if account.is_empty() {
        "default".to_string()
    } else {
        account
    };
    Ok(SyncRootIdBuilder::new(PROVIDER)
        .user_security_id(SecurityId::current_user().context("reading the current user's SID")?)
        .account_name(account)
        .build())
}

fn provider_of(id: &SyncRootId) -> String {
    id.to_os_string()
        .to_string_lossy()
        .split('!')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn same_path(a: &Path, b: &Path) -> bool {
    // Windows accepts both separators, and a folder that no longer exists can't be
    // canonicalized, so compare spellings: verbatim prefix, separators, case, trailing slash.
    let norm = |p: &Path| {
        super::util::strip_verbatim(p)
            .to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    norm(a) == norm(b)
}

#[cfg(test)]
mod path_tests {
    use super::same_path;
    use std::path::Path;

    #[test]
    fn spellings_of_one_folder_match() {
        let reg = Path::new(r"C:\Users\me\Drive");
        assert!(same_path(reg, Path::new("C:/Users/me/Drive")));
        assert!(same_path(reg, Path::new(r"c:\users\ME\Drive\")));
        assert!(same_path(reg, Path::new(r"\\?\C:\Users\me\Drive")));
        assert!(!same_path(reg, Path::new(r"C:\Users\me\Drive2")));
    }
}

/// Icon shown in the navigation pane: the tray app's icon when it is installed next to us
/// (drive.exe itself has no icon resource), else a system icon.
fn icon_resource() -> String {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for name in ["Drive.exe", "drive-app.exe"] {
            let app = dir.join(name);
            if app.is_file() && !same_path(&app, &exe) {
                return format!("{},0", app.display());
            }
        }
    }
    r"%SystemRoot%\System32\imageres.dll,-1043".to_string()
}

/// Makes `folder` a sync root for `account`. Idempotent; updates an outdated registration.
pub fn register(folder: &Path, account: &str, display_name: &str) -> Result<Registration> {
    check_ntfs(folder)?;
    // A sync root can't live inside another one (e.g. OneDrive).
    if let Some(parent) = folder.parent()
        && let Ok(handle) = super::util::open_meta(parent)
        && handle.in_sync_root().unwrap_or(false)
    {
        bail!(
            "{} is inside a folder managed by another cloud storage app (such as OneDrive). Choose a different folder for Drive.",
            folder.display()
        );
    }

    let id = sync_root_id(account)?;
    match SyncRootId::from_path(folder) {
        Ok(existing) if existing.as_hstring() == id.as_hstring() => {
            if let Ok(info) = existing.info()
                && same_path(&info.path(), folder)
                && info.version().to_string_lossy() == REGISTRATION_VERSION
                && info.population_type() == PopulationType::AlwaysFull
            {
                debug!("sync root {} already registered", folder.display());
                return Ok(Registration::Shell);
            }
        }
        Ok(existing) if provider_of(&existing) == PROVIDER => {
            // Left behind by another account (sign-out didn't finish).
            info!(
                "replacing a stale Drive registration of {}",
                folder.display()
            );
            let _ = existing.unregister();
        }
        Ok(existing) => {
            bail!(
                "{} is already used by another cloud storage app ({}). Choose a different folder for Drive.",
                folder.display(),
                provider_of(&existing)
            );
        }
        Err(_) => {}
    }
    // Our id registered for another folder (the folder setting changed): move it.
    if id.is_registered().unwrap_or(false)
        && let Ok(info) = id.info()
        && !same_path(&info.path(), folder)
    {
        info!(
            "moving the Drive registration from {} to {}",
            info.path().display(),
            folder.display()
        );
        let _ = id.unregister();
    }

    let info = SyncRootInfo::default()
        .with_display_name(display_name)
        .with_icon(icon_resource())
        .with_version(REGISTRATION_VERSION)
        .with_hydration_type(HydrationType::Progressive)
        .with_hydration_policy(HydrationPolicy::AutoDehydrationAllowed)
        .with_population_type(PopulationType::AlwaysFull)
        .with_supported_attribute(
            SupportedAttribute::FileLastWriteTime | SupportedAttribute::PreserveInSyncForSyncEngine,
        )
        .with_allow_pinning(true)
        .with_allow_hardlinks(false)
        .with_show_siblings_as_group(false)
        .with_path(folder);
    let shell = info
        .map_err(anyhow::Error::from)
        .and_then(|info| id.register(info).map_err(anyhow::Error::from));
    match shell {
        Ok(()) => {
            info!("registered {} as a cloud folder", folder.display());
            Ok(Registration::Shell)
        }
        Err(e) => {
            warn!(
                "Shell registration of {} failed ({e:#}); falling back to a native registration",
                folder.display()
            );
            register_native(folder).with_context(|| {
                format!(
                    "{} couldn't be set up as a cloud folder. Choose a folder that isn't inside another cloud storage folder.",
                    folder.display()
                )
            })?;
            Ok(Registration::Native)
        }
    }
}

fn register_native(folder: &Path) -> Result<()> {
    let path = U16CString::from_os_str(folder.as_os_str())?;
    let name = U16CString::from_str(PROVIDER)?;
    let version = U16CString::from_str(REGISTRATION_VERSION)?;
    let identity = PROVIDER.as_bytes();
    let reg = CF_SYNC_REGISTRATION {
        StructSize: std::mem::size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(name.as_ptr()),
        ProviderVersion: PCWSTR(version.as_ptr()),
        SyncRootIdentity: identity.as_ptr().cast(),
        SyncRootIdentityLength: identity.len() as u32,
        FileIdentity: std::ptr::null(),
        FileIdentityLength: 0,
        ProviderId: PROVIDER_GUID,
    };
    let policies = CF_SYNC_POLICIES {
        StructSize: std::mem::size_of::<CF_SYNC_POLICIES>() as u32,
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_PROGRESSIVE,
            Modifier: CF_HYDRATION_POLICY_MODIFIER_AUTO_DEHYDRATION_ALLOWED,
        },
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_ALWAYS_FULL,
            ..Default::default()
        },
        InSync: CF_INSYNC_POLICY_TRACK_FILE_LAST_WRITE_TIME
            | CF_INSYNC_POLICY_PRESERVE_INSYNC_FOR_SYNC_ENGINE,
        HardLink: CF_HARDLINK_POLICY_NONE,
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
    };
    unsafe {
        CfRegisterSyncRoot(
            PCWSTR(path.as_ptr()),
            &reg,
            &policies,
            CF_REGISTER_FLAG_UPDATE,
        )
    }?;
    info!(
        "registered {} natively (no Explorer navigation-pane entry)",
        folder.display()
    );
    Ok(())
}

/// Removes every Drive registration of `folder` (Shell and native). Best effort.
pub fn unregister(folder: &Path) {
    let mut ids: Vec<SyncRootId> = Vec::new();
    if let Ok(id) = SyncRootId::from_path(folder) {
        ids.push(id);
    }
    if let Ok(roots) = active_roots() {
        for r in roots {
            if same_path(&r.path(), folder) {
                ids.push(r.id());
            }
        }
    }
    for id in ids {
        if provider_of(&id) != PROVIDER {
            continue;
        }
        match id.unregister() {
            Ok(()) => info!("unregistered sync root {}", folder.display()),
            Err(e) => debug!("unregistering {} via the Shell: {e}", folder.display()),
        }
    }
    if let Ok(path) = U16CString::from_os_str(folder.as_os_str()) {
        let _ = unsafe { CfUnregisterSyncRoot(PCWSTR(path.as_ptr())) };
    }
}

/// Whether `folder` is currently a sync root (by any registration).
#[cfg(test)]
pub fn is_sync_root(folder: &Path) -> bool {
    super::util::open_meta(folder)
        .ok()
        .and_then(|f| f.sync_root_info().ok().flatten())
        .is_some()
}
