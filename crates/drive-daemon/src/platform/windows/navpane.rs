//! Explorer's navigation pane (the left sidebar).
//!
//! * Every Cloud Files sync root gets its own entry there automatically: Windows creates a
//!   per-root shell folder (`NamespaceCLSID` in the root's registration). Backed-up folders
//!   that save space are sync roots too, but shouldn't each show up as another "drive": their
//!   entry is unpinned with `System.IsPinnedToNameSpaceTree = 0`.
//! * "Drive - Backups" is an entry of our own that opens an ordinary folder (of links to the
//!   backed-up folders). It's the classic way cloud apps add a sidebar entry: a shell folder
//!   that delegates to a file-system folder, registered per user (no admin rights needed).
//!
//! Everything here is best effort: Explorer works without it.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::{debug, info};
use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};

const SYNC_ROOTS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
const CLASSES_CLSID: &str = r"Software\Classes\CLSID";
const NAMESPACE: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Desktop\NameSpace";
const HIDE_ON_DESKTOP: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\HideDesktopIcons\NewStartPanel";
/// The shell's "folder that shows a file-system folder" implementation.
const DELEGATE_FOLDER: &str = "{0E5AAE11-A475-4c5b-AB00-C66DE400274E}";
const PINNED: &str = "System.IsPinnedToNameSpaceTree";

/// Shows or hides the navigation-pane entry Windows made for a sync root (by its id, e.g.
/// `Drive!S-1-5-…!login~folder`).
pub fn set_sync_root_entry_visible(sync_root_id: &str, visible: bool) -> Result<()> {
    let root = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey_with_flags(format!(r"{SYNC_ROOTS}\{sync_root_id}"), KEY_READ)
        .context("reading the folder's registration")?;
    let clsid: String = root
        .get_value("NamespaceCLSID")
        .context("the folder has no Explorer entry")?;
    let key = RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey_with_flags(
            format!(r"{CLASSES_CLSID}\{clsid}"),
            winreg::enums::KEY_READ | winreg::enums::KEY_SET_VALUE,
        )
        .context("opening the folder's Explorer entry")?;
    let want = u32::from(visible);
    if key.get_value::<u32, _>(PINNED).ok() != Some(want) {
        key.set_value(PINNED, &want)?;
        debug!(
            "{} the Explorer entry of {sync_root_id}",
            if visible { "showed" } else { "hid" }
        );
        notify_shell();
    }
    Ok(())
}

/// Adds (or updates) a navigation-pane entry `name` that opens `folder`. `clsid` identifies
/// the entry (`{…}`); `icon` is an icon resource (`file.exe,0`).
pub fn add_folder_entry(clsid: &str, name: &str, folder: &Path, icon: &str) -> Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let target = folder.to_string_lossy().into_owned();
    let (key, _) = hkcu.create_subkey(format!(r"{CLASSES_CLSID}\{clsid}"))?;
    let current: Option<String> = key
        .open_subkey(r"Instance\InitPropertyBag")
        .and_then(|k| k.get_value("TargetFolderPath"))
        .ok();
    key.set_value("", &name)?;
    key.set_value(PINNED, &1u32)?;
    // Right below the cloud folders' default position.
    key.set_value("SortOrderIndex", &0x42u32)?;
    key.create_subkey("DefaultIcon")?.0.set_value("", &icon)?;
    key.create_subkey("InProcServer32")?
        .0
        .set_value("", &r"%SystemRoot%\system32\shell32.dll")?;
    let (instance, _) = key.create_subkey("Instance")?;
    instance.set_value("CLSID", &DELEGATE_FOLDER)?;
    let (bag, _) = instance.create_subkey("InitPropertyBag")?;
    // FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_READONLY: a file-system folder.
    bag.set_value("Attributes", &0x11u32)?;
    bag.set_value("TargetFolderPath", &target)?;
    let (shell_folder, _) = key.create_subkey("ShellFolder")?;
    shell_folder.set_value("FolderValueFlags", &0x28u32)?;
    // SFGAO flags of a browsable, renamable-free file-system folder with subfolders.
    shell_folder.set_value("Attributes", &0xF080_004Du32)?;
    hkcu.create_subkey(format!(r"{NAMESPACE}\{clsid}"))?
        .0
        .set_value("", &name)?;
    // Only in the navigation pane, not as a desktop icon.
    hkcu.create_subkey(HIDE_ON_DESKTOP)?
        .0
        .set_value(clsid, &1u32)?;
    if current.as_deref() != Some(target.as_str()) {
        info!("added \"{name}\" to Explorer's navigation pane");
        notify_shell();
    }
    Ok(())
}

/// Removes an entry made by [`add_folder_entry`]. Nothing happens if it doesn't exist.
pub fn remove_folder_entry(clsid: &str) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let existed = hkcu
        .open_subkey(format!(r"{CLASSES_CLSID}\{clsid}"))
        .is_ok();
    let _ = hkcu.delete_subkey_all(format!(r"{NAMESPACE}\{clsid}"));
    let _ = hkcu.delete_subkey_all(format!(r"{CLASSES_CLSID}\{clsid}"));
    if let Ok(k) = hkcu.open_subkey_with_flags(HIDE_ON_DESKTOP, winreg::enums::KEY_SET_VALUE) {
        let _ = k.delete_value(clsid);
    }
    if existed {
        info!("removed an entry from Explorer's navigation pane");
        notify_shell();
    }
}

/// Whether an entry made by [`add_folder_entry`] exists, and the folder it opens.
#[cfg(test)]
pub fn folder_entry(clsid: &str) -> Option<String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!(r"{CLASSES_CLSID}\{clsid}\Instance\InitPropertyBag"))
        .and_then(|k| k.get_value("TargetFolderPath"))
        .ok()
}

/// Whether a sync root's own entry is shown.
#[cfg(test)]
pub fn sync_root_entry_visible(sync_root_id: &str) -> Option<bool> {
    let clsid: String = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey(format!(r"{SYNC_ROOTS}\{sync_root_id}"))
        .and_then(|k| k.get_value("NamespaceCLSID"))
        .ok()?;
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!(r"{CLASSES_CLSID}\{clsid}"))
        .and_then(|k| k.get_value::<u32, _>(PINNED))
        .ok()
        .map(|v| v != 0)
}

/// Tells Explorer to reload its navigation pane.
fn notify_shell() {
    use windows::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

/// A stable entry id (`{…}`) derived from `seed` (FNV-1a, so it never changes between builds).
pub fn clsid_for(seed: &str) -> String {
    let fnv = |basis: u64| {
        seed.bytes().fold(basis, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3)
        })
    };
    let a = fnv(0xcbf2_9ce4_8422_2325);
    let b = fnv(0x6c62_272e_07bb_0142);
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
        (a >> 32) as u32,
        (a >> 16) as u16,
        // Version 4 / variant bits, so it reads as an ordinary GUID.
        ((a as u16) & 0x0FFF) | 0x4000,
        (((b >> 48) as u16) & 0x3FFF) | 0x8000,
        b & 0xFFFF_FFFF_FFFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsids_are_stable_and_well_formed() {
        let a = clsid_for("drive-backups:me");
        assert_eq!(a, clsid_for("drive-backups:me"));
        assert_ne!(a, clsid_for("drive-backups:other"));
        assert_eq!(a.len(), 38);
        assert!(a.starts_with('{') && a.ends_with('}'));
        assert_eq!(a.as_bytes()[15], b'4');
    }
}
