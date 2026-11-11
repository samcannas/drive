//! Core of Drive: rclone-compatible encryption, the uloz.to client, the local state database
//! and the sync engine. Platform file-system integration (Windows Cloud Files, Linux FUSE)
//! lives in `drive-daemon` and talks to the engine through [`engine::LocalSide`].

pub mod crypt;
pub mod db;
pub mod engine;
pub mod ipc;
pub mod ipc_client;
pub mod remote;
pub mod ulozto;
