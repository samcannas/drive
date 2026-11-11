//! Live checks against apis.uloz.to. Ignored by default; run with `cargo test -- --ignored`.
//! With ULOZTO_LOGIN / ULOZTO_PASSWORD set, also exercises a real account (creates and
//! trashes a temporary folder named `drive-selftest-*`).

use std::sync::Arc;

use drive_core::remote::{Remote, RemoteError, UploadBody, read_all};
use drive_core::ulozto::{UlozClient, UlozConfig, UlozRemote};

#[tokio::test]
#[ignore]
async fn bad_credentials_are_reported_as_auth_error() {
    let c = UlozClient::new(UlozConfig::new("drive-selftest-nonexistent-user", "wrong-password", "selftest")).unwrap();
    match c.login().await {
        Err(RemoteError::Auth(msg)) => eprintln!("auth error as expected: {msg}"),
        other => panic!("expected auth error, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn real_account_roundtrip() {
    let (Ok(login), Ok(password)) = (std::env::var("ULOZTO_LOGIN"), std::env::var("ULOZTO_PASSWORD")) else {
        eprintln!("ULOZTO_LOGIN/ULOZTO_PASSWORD not set; skipping");
        return;
    };
    let client = Arc::new(UlozClient::new(UlozConfig::new(login, password, "drive-selftest")).unwrap());
    let session = client.login().await.unwrap();
    let name = format!("drive-selftest-{}", std::process::id());
    let folder = client.create_folder(&session.root_folder_slug, &name).await.unwrap();
    let remote = UlozRemote::new(client.clone(), folder.slug.clone());
    let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    let sub = remote.create_folder(&folder.slug, "sub").await.unwrap();
    let f = remote.upload(&sub.id, "file.bin", UploadBody::from_bytes(data.clone(), 1_700_000_000_000)).await.unwrap();
    let tree = remote.scan().await.unwrap();
    let listed = tree.entries.iter().find(|e| e.id == f.id).expect("uploaded file listed");
    assert_eq!(listed.size, data.len() as u64);
    assert_eq!(listed.mtime_ms, 1_700_000_000_000);
    assert_eq!(read_all(remote.read(listed, 0, None).await.unwrap()).await.unwrap(), data);
    let part = read_all(remote.read(listed, 100_000, Some(1000)).await.unwrap()).await.unwrap();
    assert_eq!(part, &data[100_000..101_000]);
    let renamed = remote.rename(listed, &folder.slug, "moved.bin").await.unwrap();
    assert_eq!(renamed.name, "moved.bin");
    remote.trash(&renamed).await.unwrap();
    client.delete_folder(&folder.slug).await.unwrap();
}
