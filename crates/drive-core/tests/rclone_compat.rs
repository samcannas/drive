//! Cross-checks our crypt implementation against a real rclone binary.
//!
//! Runs only when `RCLONE_BIN` points at an rclone executable (CI downloads one).

use std::path::Path;
use std::process::Command;

use drive_core::crypt::{self, Cipher, Nonce, obscure};

fn rclone() -> Option<String> {
    std::env::var("RCLONE_BIN").ok().filter(|p| Path::new(p).exists())
}

fn run(bin: &str, base: &Path, password: &str, salt: &str, args: &[&str]) -> String {
    let out = Command::new(bin)
        .args(args)
        .env("RCLONE_CONFIG", base.join("none.conf"))
        .env("RCLONE_CONFIG_SECRET_TYPE", "crypt")
        .env("RCLONE_CONFIG_SECRET_REMOTE", base.join("vault"))
        .env("RCLONE_CONFIG_SECRET_PASSWORD", obscure::obscure(password))
        .env("RCLONE_CONFIG_SECRET_PASSWORD2", obscure::obscure(salt))
        .output()
        .expect("run rclone");
    assert!(out.status.success(), "rclone {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn rclone_reads_what_we_write_and_vice_versa() {
    let Some(bin) = rclone() else {
        eprintln!("RCLONE_BIN not set; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path();
    std::fs::create_dir_all(base.join("vault")).unwrap();
    let (password, salt) = ("s3cret pass", "pepper-salt");
    let cipher = Cipher::new(password, salt).unwrap();

    // 1. We encrypt, rclone decrypts.
    let plain: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    let name = "Folder Ü/report 2026.pdf";
    let enc_path = cipher.encrypt_path(name).unwrap();
    let target = base.join("vault").join(enc_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, crypt::encrypt_bytes(&cipher, Nonce::random(), &plain)).unwrap();
    let got = Command::new(&bin)
        .args(["cat", &format!("secret:{name}")])
        .env("RCLONE_CONFIG", base.join("none.conf"))
        .env("RCLONE_CONFIG_SECRET_TYPE", "crypt")
        .env("RCLONE_CONFIG_SECRET_REMOTE", base.join("vault"))
        .env("RCLONE_CONFIG_SECRET_PASSWORD", obscure::obscure(password))
        .env("RCLONE_CONFIG_SECRET_PASSWORD2", obscure::obscure(salt))
        .output()
        .unwrap();
    assert!(got.status.success(), "{}", String::from_utf8_lossy(&got.stderr));
    assert_eq!(got.stdout, plain, "rclone decrypted different bytes");

    // 2. rclone encrypts, we decrypt.
    let src = base.join("src.bin");
    let plain2: Vec<u8> = (0..70_000u32).map(|i| (i * 7 % 256) as u8).collect();
    std::fs::write(&src, &plain2).unwrap();
    run(&bin, base, password, salt, &["copyto", src.to_str().unwrap(), "secret:a/b/from rclone.bin"]);
    let enc_name = cipher.encrypt_path("a/b/from rclone.bin").unwrap();
    let stored = std::fs::read(base.join("vault").join(enc_name.replace('/', std::path::MAIN_SEPARATOR_STR)))
        .expect("rclone stored the file under the name we computed");
    assert_eq!(crypt::decrypt_bytes(&cipher, &stored).unwrap(), plain2);

    // 3. rclone lists names we encrypted.
    let listing = run(&bin, base, password, salt, &["lsf", "-R", "secret:"]);
    assert!(listing.contains("Folder Ü/report 2026.pdf"), "listing: {listing}");
}
