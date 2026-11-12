//! "Drive - Backups": a folder next to the Drive folder with a link to each backed-up folder
//! (a junction on Windows, a symbolic link on Linux). Opening one opens the real folder, so
//! it's fully usable and takes no space; Explorer shows the folder as its own sidebar entry.
//!
//! Safety: only links are ever created or removed here. Anything else in the folder is left
//! alone, and removing a link never touches the folder it points to.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{info, warn};

/// The folder's name, also the sidebar entry's name.
pub const FOLDER_NAME: &str = "Drive - Backups";

/// Where the folder goes: next to the Drive folder.
pub fn folder_for(drive_folder: &Path) -> PathBuf {
    drive_folder
        .parent()
        .unwrap_or(drive_folder)
        .join(FOLDER_NAME)
}

/// Makes `dir` hold exactly one link per `(name, target)`. With no links, our links are
/// removed and so is `dir` if nothing else is in it.
pub fn sync(dir: &Path, links: &[(String, PathBuf)]) -> Result<()> {
    let wanted: HashMap<String, &PathBuf> =
        links.iter().map(|(n, t)| (n.to_lowercase(), t)).collect();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)?.flatten() {
            let path = entry.path();
            if !is_link(&path) {
                continue; // not ours
            }
            let name = entry.file_name().to_string_lossy().to_lowercase();
            let keep = wanted
                .get(&name)
                .is_some_and(|t| link_target(&path).is_some_and(|cur| same_path(&cur, t)));
            if !keep {
                remove_link(&path)
                    .with_context(|| format!("removing the link {}", path.display()))?;
            }
        }
    }
    if links.is_empty() {
        let _ = std::fs::remove_dir(dir); // only if empty
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    for (name, target) in links {
        let link = dir.join(name);
        if std::fs::symlink_metadata(&link).is_ok() {
            if !is_link(&link) {
                warn!(
                    "{} isn't a link Drive made; leaving it alone",
                    link.display()
                );
            }
            continue;
        }
        if !target.is_dir() {
            continue; // unplugged or missing: the backup reports it
        }
        create_link(target, &link)
            .with_context(|| format!("linking {} to {}", link.display(), target.display()))?;
        info!("{} now opens {}", link.display(), target.display());
    }
    Ok(())
}

/// A junction or symbolic link (never followed).
pub fn is_link(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())
        || cfg!(windows) && junction_target(p).is_some()
}

fn link_target(p: &Path) -> Option<PathBuf> {
    junction_target(p).or_else(|| std::fs::read_link(p).ok())
}

#[cfg(windows)]
fn junction_target(p: &Path) -> Option<PathBuf> {
    junction::get_target(p).ok()
}

#[cfg(not(windows))]
fn junction_target(_p: &Path) -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn create_link(target: &Path, link: &Path) -> std::io::Result<()> {
    // Junctions need no special rights (symbolic links do).
    junction::create(target, link)
}

#[cfg(not(windows))]
fn create_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn remove_link(link: &Path) -> std::io::Result<()> {
    // Removing a junction (a directory entry) removes only the link.
    std::fs::remove_dir(link).or_else(|_| std::fs::remove_file(link))
}

#[cfg(not(windows))]
fn remove_link(link: &Path) -> std::io::Result<()> {
    std::fs::remove_file(link)
}

fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        let s = s
            .strip_prefix("//?/")
            .map(str::to_string)
            .unwrap_or(s.clone());
        let s = s.trim_end_matches('/').to_string();
        if cfg!(windows) { s.to_lowercase() } else { s }
    };
    norm(a) == norm(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("drive-links-{name}-{}", rand::random::<u32>()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn links_open_the_real_folders_and_are_kept_in_step() {
        let base = tmp("sync");
        let music = base.join("Music");
        let videos = base.join("Videos");
        for d in [&music, &videos] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(music.join("song.mp3"), b"la").unwrap();
        let dir = base.join(FOLDER_NAME);

        sync(
            &dir,
            &[
                ("Music".into(), music.clone()),
                ("Videos".into(), videos.clone()),
            ],
        )
        .unwrap();
        assert!(is_link(&dir.join("Music")));
        // Usable: reading and writing through the link reaches the real folder.
        assert_eq!(std::fs::read(dir.join("Music/song.mp3")).unwrap(), b"la");
        std::fs::write(dir.join("Videos/clip.mp4"), b"v").unwrap();
        assert_eq!(std::fs::read(videos.join("clip.mp4")).unwrap(), b"v");

        // Something the user put there is never touched.
        std::fs::write(dir.join("notes.txt"), b"mine").unwrap();

        // Videos stops being backed up: its link goes, the folder itself stays.
        sync(&dir, &[("Music".into(), music.clone())]).unwrap();
        assert!(std::fs::symlink_metadata(dir.join("Videos")).is_err());
        assert_eq!(std::fs::read(videos.join("clip.mp4")).unwrap(), b"v");
        assert!(dir.join("notes.txt").exists());

        // A link pointing elsewhere is replaced.
        let music2 = base.join("Music2");
        std::fs::create_dir_all(&music2).unwrap();
        sync(&dir, &[("Music".into(), music2.clone())]).unwrap();
        assert!(same_path(
            &link_target(&dir.join("Music")).unwrap(),
            &music2
        ));
        assert_eq!(std::fs::read(music.join("song.mp3")).unwrap(), b"la");

        // Nothing backed up: our links go; the folder stays because of notes.txt.
        sync(&dir, &[]).unwrap();
        assert!(std::fs::symlink_metadata(dir.join("Music")).is_err());
        assert!(dir.join("notes.txt").exists());
        std::fs::remove_file(dir.join("notes.txt")).unwrap();
        sync(&dir, &[]).unwrap();
        assert!(!dir.exists());

        assert_eq!(std::fs::read(music.join("song.mp3")).unwrap(), b"la");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
