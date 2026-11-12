use std::path::PathBuf;

use drive_core::ipc::{KnownFolder, SpaceAction, SpaceCategory, SpaceItem};

use super::scan::Node;
use super::*;

const G: u64 = 1 << 30;

/// A folder whose size is its own files plus its children.
fn n(name: &str, own: u64, children: Vec<Node>) -> Node {
    let bytes = own + children.iter().map(|c| c.bytes).sum::<u64>();
    let skipped = children.iter().map(|c| c.skipped).sum();
    let has_git = children.iter().any(|c| c.has_git);
    let git_bytes = children.iter().map(|c| c.git_bytes).sum();
    let program_bytes = children.iter().map(|c| c.program_bytes).sum();
    let mut children = children;
    children.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    Node {
        name: name.into(),
        bytes,
        files: 10,
        skipped,
        git: false,
        has_git,
        git_bytes,
        programs: false,
        program_bytes,
        unreadable: false,
        children,
    }
}

fn git(mut node: Node) -> Node {
    node.git = true;
    node.has_git = true;
    node.git_bytes = node.bytes;
    node
}

fn programs(mut node: Node) -> Node {
    node.programs = true;
    node.program_bytes = node.bytes;
    node
}

fn skipped(mut node: Node, bytes: u64) -> Node {
    node.skipped = bytes;
    node
}

fn find<'a>(items: &'a [SpaceItem], name: &str) -> &'a SpaceItem {
    items.iter().find(|i| i.name == name).unwrap_or_else(|| {
        panic!(
            "no item {name} in {:?}",
            items.iter().map(|i| &i.name).collect::<Vec<_>>()
        )
    })
}

#[cfg(windows)]
fn sample() -> (Scanned, Context) {
    let me = n(
        "me",
        G,
        vec![
            n("Videos", 182 * G, vec![]),
            n("Downloads", 64 * G, vec![]),
            n(
                "Documents",
                10 * G,
                vec![skipped(
                    n("GitHub", 0, vec![git(n("drive", 30 * G, vec![]))]),
                    8 * G,
                )],
            ),
            n("Drive", 9 * G, vec![]),
            n(
                "AppData",
                0,
                vec![
                    n(
                        "Local",
                        4 * G,
                        vec![
                            n("Docker", 61 * G, vec![]),
                            n("Temp", 5 * G, vec![]),
                            n("Drive", G / 2, vec![]),
                            programs(n("Programs", 0, vec![n("cursor", 12 * G, vec![])])),
                        ],
                    ),
                    n("Roaming", 7 * G, vec![n("Code", 3 * G, vec![])]),
                ],
            ),
            n(".cargo", 3 * G, vec![]),
        ],
    );
    let users = n("Users", 0, vec![me, n("Public", G, vec![])]);
    let tree = n(
        "C:\\",
        19 * G, // hiberfil.sys + pagefile.sys
        vec![
            users,
            n(
                "Program Files",
                0,
                vec![n("Steam", 20 * G, vec![]), n("Adobe", 10 * G, vec![])],
            ),
            n("Program Files (x86)", 5 * G, vec![]),
            n("ProgramData", 8 * G, vec![]),
            n("$Recycle.Bin", 2 * G, vec![]),
            programs(n("Games", 100 * G, vec![])),
            n("Media", 50 * G, vec![]),
            n("tiny", 100 << 20, vec![]),
        ],
    );
    let s = Scanned {
        root: PathBuf::from("C:\\"),
        total_bytes: 953 * G,
        free_bytes: 51 * G,
        root_files: vec![
            ("hiberfil.sys".into(), 13 * G),
            ("pagefile.sys".into(), 6 * G),
        ],
        tree,
        started_ms: 0,
        finished_ms: 1,
    };
    let home = PathBuf::from("C:\\Users\\me");
    let ctx = Context {
        home: home.clone(),
        drive_folder: home.join("Drive"),
        own_dirs: vec![home.join("AppData\\Local\\Drive")],
        backups: vec![BackupRef {
            id: "dl".into(),
            path: home.join("Downloads"),
            on_demand: false,
            keep_bytes: 5 * G,
            cloud_only_bytes: 0,
        }],
        other_clouds: vec![],
        known: vec![
            (KnownFolder::Videos, "Videos".into(), home.join("Videos")),
            (
                KnownFolder::Downloads,
                "Downloads".into(),
                home.join("Downloads"),
            ),
            (
                KnownFolder::Documents,
                "Documents".into(),
                home.join("Documents"),
            ),
        ],
        can_save_space: true,
        drive_cache_bytes: 7 * G,
        drive_local_bytes: 9 * G,
        drive_cloud_only_bytes: 120 * G,
        drive_content_dir: None,
        temp_dir: Some(home.join("AppData\\Local\\Temp")),
        keep_bytes: 5 * G,
    };
    (s, ctx)
}

#[cfg(windows)]
#[test]
fn windows_report_names_actions_and_adds_up() {
    let (s, ctx) = sample();
    let (items, can_free) = build_items(&s, &ctx);

    let videos = find(&items, "Videos");
    assert_eq!(videos.category, SpaceCategory::UserFolder);
    assert_eq!(videos.known, Some(KnownFolder::Videos));
    assert_eq!(videos.action, Some(SpaceAction::SaveSpace));
    assert_eq!(videos.frees_bytes, 177 * G);
    assert_eq!(videos.path, "C:\\Users\\me\\Videos");

    let downloads = find(&items, "Downloads");
    assert_eq!(downloads.category, SpaceCategory::Backup);
    assert_eq!(downloads.backup_id.as_deref(), Some("dl"));
    assert_eq!(downloads.action, Some(SpaceAction::TurnOnSaveSpace));
    assert_eq!(downloads.frees_bytes, 59 * G);

    // Documents holds code: still offered, with a caution; skipped bytes aren't counted.
    let docs = find(&items, "Documents");
    assert_eq!(docs.action, Some(SpaceAction::SaveSpace));
    assert!(
        docs.caution
            .as_deref()
            .unwrap()
            .contains("code projects (30 GB)")
    );
    assert_eq!(docs.frees_bytes, (40 - 8 - 5) * G);
    let github = find(&docs.children, "GitHub");
    assert_eq!(github.category, SpaceCategory::Developer);
    assert_eq!(github.action, None);

    let drive = find(&items, "Drive");
    assert_eq!(drive.category, SpaceCategory::DriveFolder);
    assert_eq!(drive.action, Some(SpaceAction::FreeDriveCache));
    assert_eq!(drive.frees_bytes, 7 * G);

    let app_data = find(&items, "App data");
    assert_eq!(app_data.category, SpaceCategory::AppData);
    assert_eq!(app_data.action, None);
    assert_eq!(app_data.bytes, (4 + 61 + 5 + 7 + 3) * G + G / 2 - 5 * G);
    assert_eq!(app_data.children[0].name, "Docker");
    assert!(
        app_data
            .children
            .iter()
            .any(|c| c.name == "Drive (Drive's own data)")
    );
    let temp = find(&items, "Temporary files");
    assert_eq!(temp.bytes, 5 * G);
    assert_eq!(temp.action, Some(SpaceAction::OpenSystemCleanup));

    assert_eq!(find(&items, "Hidden app folders").bytes, 3 * G);
    assert_eq!(find(&items, "Files in your user folder").bytes, G);
    assert_eq!(find(&items, "Other accounts and shared files").bytes, G);

    let apps = find(&items, "Installed apps");
    assert_eq!(apps.category, SpaceCategory::Apps);
    assert_eq!(apps.bytes, (35 + 12) * G);
    assert_eq!(apps.children[0].name, "Steam");
    assert!(
        apps.children
            .iter()
            .any(|c| c.name == "cursor" && c.path.ends_with(r"AppData\Local\Programs\cursor"))
    );
    let games = find(&items, "Games");
    assert_eq!(games.category, SpaceCategory::Apps);
    assert_eq!(games.action, None);

    let media = find(&items, "Media");
    assert_eq!(media.action, Some(SpaceAction::SaveSpace));
    assert_eq!(media.frees_bytes, 45 * G);

    let bin = find(&items, "Recycle Bin");
    assert_eq!(bin.action, Some(SpaceAction::OpenSystemCleanup));
    let pages = find(&items, "Page and hibernation files");
    assert!(pages.note.as_deref().unwrap().contains("13 GB"));

    // Everything adds up to the space used on the disk.
    let used = s.total_bytes - s.free_bytes;
    assert_eq!(items.iter().map(|i| i.bytes).sum::<u64>(), used);
    let system = find(&items, "Windows and system files");
    assert_eq!(system.bytes, used - s.tree.bytes);
    let smaller = find(&items, "Smaller folders");
    assert_eq!(smaller.bytes, 100 << 20); // "tiny" isn't listed on its own…
    assert!(items.iter().all(|i| i.name != "tiny"));
    // …and the list is sorted.
    assert!(items.windows(2).all(|w| w[0].bytes >= w[1].bytes));

    assert_eq!(can_free, (177 + 59 + 27 + 7 + 45) * G);
}

#[cfg(windows)]
#[test]
fn windows_report_follows_backups_and_other_clouds() {
    let (s, mut ctx) = sample();
    ctx.backups.push(BackupRef {
        id: "vid".into(),
        path: ctx.home.join("Videos"),
        on_demand: true,
        keep_bytes: 5 * G,
        cloud_only_bytes: 170 * G,
    });
    ctx.other_clouds
        .push((PathBuf::from("C:\\Media"), "OneDrive".into()));
    ctx.can_save_space = true;
    let (items, can_free) = build_items(&s, &ctx);
    let videos = find(&items, "Videos");
    assert!(videos.on_demand);
    assert_eq!(videos.action, None);
    assert!(videos.note.as_deref().unwrap().contains("5.0 GB"));
    let media = find(&items, "Media");
    assert_eq!(media.category, SpaceCategory::OtherCloud);
    assert!(media.note.as_deref().unwrap().contains("OneDrive"));
    assert_eq!(can_free, (59 + 27 + 7) * G);
}

#[cfg(windows)]
#[test]
fn windows_report_without_save_space_offers_backups() {
    let (s, mut ctx) = sample();
    ctx.can_save_space = false;
    ctx.drive_cache_bytes = 0;
    let (items, can_free) = build_items(&s, &ctx);
    let videos = find(&items, "Videos");
    assert_eq!(videos.action, Some(SpaceAction::BackUp));
    assert_eq!(videos.frees_bytes, 0);
    assert_eq!(find(&items, "Drive").action, None);
    assert_eq!(can_free, 0);
}

#[cfg(unix)]
#[test]
fn linux_report_names_actions_and_adds_up() {
    let me = n(
        "me",
        G,
        vec![
            n("Videos", 40 * G, vec![]),
            n("code", 0, vec![git(n("app", 12 * G, vec![]))]),
            n(
                ".cache",
                3 * G,
                vec![n("Drive", 0, vec![n("content", 6 * G, vec![])])],
            ),
            n(".local", 4 * G, vec![]),
        ],
    );
    let tree = n(
        "/",
        0,
        vec![
            n("home", 0, vec![me, n("guest", G, vec![])]),
            n("usr", 15 * G, vec![]),
            n("var", 0, vec![n("lib", 9 * G, vec![]), n("log", G, vec![])]),
            n("tmp", G, vec![]),
            n("etc", G / 2, vec![]),
        ],
    );
    let s = Scanned {
        root: PathBuf::from("/"),
        total_bytes: 500 * G,
        free_bytes: 300 * G,
        root_files: vec![],
        tree,
        started_ms: 0,
        finished_ms: 1,
    };
    let home = PathBuf::from("/home/me");
    let ctx = Context {
        home: home.clone(),
        drive_folder: home.join("Drive"),
        own_dirs: vec![home.join(".cache/Drive")],
        backups: vec![],
        other_clouds: vec![],
        known: vec![(KnownFolder::Videos, "Videos".into(), home.join("Videos"))],
        can_save_space: false,
        drive_cache_bytes: 6 * G,
        drive_local_bytes: 6 * G,
        drive_cloud_only_bytes: 0,
        drive_content_dir: Some(home.join(".cache/Drive/content")),
        temp_dir: None,
        keep_bytes: 5 * G,
    };
    let (items, can_free) = build_items(&s, &ctx);
    let videos = find(&items, "Videos");
    assert_eq!(videos.action, Some(SpaceAction::BackUp));
    assert_eq!(find(&items, "code").category, SpaceCategory::Developer);
    assert_eq!(find(&items, "Caches").bytes, 3 * G);
    let drive = find(&items, "Drive");
    assert_eq!(drive.action, Some(SpaceAction::FreeDriveCache));
    assert_eq!(drive.bytes, 6 * G);
    assert_eq!(find(&items, "Installed software").bytes, 15 * G);
    assert_eq!(find(&items, "System data and logs").bytes, 10 * G);
    assert_eq!(find(&items, "Other accounts").bytes, G);
    assert_eq!(can_free, 6 * G);
    let used = s.total_bytes - s.free_bytes;
    assert_eq!(items.iter().map(|i| i.bytes).sum::<u64>(), used);
}

#[test]
fn sizes_read_well() {
    assert_eq!(gb(182 * G), "182 GB");
    assert_eq!(gb(5 * G), "5.0 GB");
    assert_eq!(gb(300 << 20), "300 MB");
}

#[test]
fn advisor_scans_saves_and_reloads() {
    let data = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("Videos")).unwrap();
    std::fs::write(home.path().join("Videos/clip.mp4"), vec![1u8; 4096]).unwrap();
    let advisor = Advisor::new(data.path());
    assert!(!advisor.has_report());
    // Measuring the whole disk is too slow for a unit test: check the bookkeeping only.
    let p = advisor.begin().unwrap();
    assert!(advisor.is_scanning());
    assert!(advisor.begin().is_none(), "one scan at a time");
    p.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    advisor.run(home.path(), &p);
    assert!(!advisor.is_scanning());
    assert!(
        !advisor.has_report(),
        "a cancelled first scan leaves no report"
    );
    let report = advisor.report(&Context {
        home: home.path().into(),
        ..Default::default()
    });
    assert_eq!(report.scan.state, drive_core::ipc::SpaceScanState::Idle);
    assert!(!report.disks.is_empty());
}

/// Measures this computer's real disk (read-only) and prints the report. Slow; run by hand:
/// `cargo test -p drive-daemon --bin drive real_disk -- --ignored --nocapture`.
#[test]
#[ignore]
fn real_disk() {
    let home = dirs::home_dir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let advisor = Advisor::new(data.path());
    let p = advisor.begin().unwrap();
    let t = std::time::Instant::now();
    advisor.run(&home, &p);
    let took = t.elapsed();
    let ctx = Context {
        home: home.clone(),
        drive_folder: home.join("Drive"),
        known: crate::backup::known_folders()
            .into_iter()
            .map(|(k, n, p)| (k, n.to_string(), p))
            .collect(),
        can_save_space: cfg!(windows),
        temp_dir: Some(crate::space::long_path(&std::env::temp_dir())),
        keep_bytes: crate::backup::DEFAULT_KEEP_BYTES,
        ..Default::default()
    };
    let r = advisor.report(&ctx);
    println!(
        "scan: {:?} in {took:?}; {} files, {}",
        r.scan.state,
        r.scan.scanned_files,
        gb(r.scan.scanned_bytes)
    );
    for d in &r.disks {
        println!(
            "disk {} {}: {} free of {} (scanned {})",
            d.path,
            d.name,
            gb(d.free_bytes),
            gb(d.total_bytes),
            d.scanned
        );
    }
    for i in &r.items {
        println!(
            "{:>8}  {:<36} {:?} {:?} frees {}  {}",
            gb(i.bytes),
            i.name,
            i.category,
            i.action,
            gb(i.frees_bytes),
            i.note
                .clone()
                .or(i.caution.clone())
                .unwrap_or_default()
                .chars()
                .take(60)
                .collect::<String>()
        );
        for c in i.children.iter().take(4) {
            println!(
                "          · {:>8} {:<30} {:?} {:?}",
                gb(c.bytes),
                c.name,
                c.category,
                c.action
            );
        }
    }
    println!("can free up to {}", gb(r.can_free_bytes));
    let used: u64 = r
        .disks
        .iter()
        .find(|d| d.scanned)
        .map(|d| d.total_bytes - d.free_bytes)
        .unwrap();
    let sum: u64 = r.items.iter().map(|i| i.bytes).sum();
    println!("items add up to {} of {} used", gb(sum), gb(used));
}

#[cfg(windows)]
#[test]
fn long_paths_resolve_short_names() {
    let t = long_path(&std::env::temp_dir());
    assert!(!t.to_string_lossy().contains('~'), "{}", t.display());
    assert!(!t.to_string_lossy().starts_with(r"\\?\"), "{}", t.display());
    assert!(t.is_dir());
}
