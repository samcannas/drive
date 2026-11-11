//! uloz.to REST API types (subset of <https://apis.uloz.to/v5/open-api/public>).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ApiError {
    #[serde(default, rename = "error")]
    pub error_code: i64,
    #[serde(default, rename = "code")]
    pub status_code: i64,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ListMetadata {
    #[serde(default)]
    pub offset: i64,
    #[serde(default)]
    pub limit: i64,
    #[serde(default)]
    pub items_count: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Folder {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub parent_folder_slug: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub last_user_modified: Option<DateTime<Utc>>,
    #[serde(default)]
    pub has_subfolder: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct File {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub filesize: i64,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub folder_slug: Option<String>,
    #[serde(default)]
    pub last_user_modified: Option<DateTime<Utc>>,
    #[serde(default)]
    pub upload_time: Option<DateTime<Utc>>,
    #[serde(default)]
    pub is_in_trash: bool,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub is_incomplete: bool,
    #[serde(default)]
    pub content_status: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListFoldersResponse {
    #[serde(default)]
    pub metadata: ListMetadata,
    #[serde(default)]
    pub subfolders: Vec<Folder>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListFilesResponse {
    #[serde(default)]
    pub metadata: ListMetadata,
    #[serde(default)]
    pub items: Vec<File>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuthenticateRequest<'a> {
    pub login: &'a str,
    pub password: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthenticateResponse {
    pub token_id: String,
    #[serde(default)]
    pub token_validity_interval: i64,
    pub session: SessionInfo,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionInfo {
    pub user: SessionUser,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SessionUser {
    pub login: String,
    #[serde(default)]
    pub root_folder_slug: String,
    #[serde(default)]
    pub credit: i64,
    #[serde(default)]
    pub has_cloud: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateFolderRequest<'a> {
    pub name: &'a str,
    pub parent_folder_slug: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateUploadUrlRequest<'a> {
    pub user_login: &'a str,
    pub realm: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateUploadUrlResponse {
    pub upload_url: String,
    pub private_slug: String,
}

// ---- chunked (resumable) uploads: https://uloz.to/upload-resumable-api-beta ----------------

#[derive(Debug, Clone, Serialize)]
pub struct CreateChunkedLinkRequest<'a> {
    pub user_login: &'a str,
    /// Renews the link of an existing batch (new signature, same batch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_slug: Option<&'a str>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateChunkedLinkResponse {
    pub private_slug: String,
    pub upload_signature: String,
    pub upload_resumable_base_url: String,
    /// Seconds until the signature expires.
    #[serde(default)]
    pub validity_interval: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RegisterChunkedRequest<'a> {
    pub batch_file_id: u32,
    pub upload_signature: &'a str,
    pub name: &'a str,
    pub filesize: u64,
    pub chunksize: u64,
    pub crc32: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtendChunkedRequest<'a> {
    pub upload_signature: &'a str,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ChunkedStatus {
    /// `pending` / `processing` (still assembling), `finished`, `bad-crc32`, or another failure.
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub uploaded_chunks: Vec<u32>,
    #[serde(default)]
    pub missing_chunks: Vec<u32>,
    #[serde(default)]
    pub irpc: Option<ChunkedResult>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChunkedResult {
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HashVerifyResponse {
    #[serde(default)]
    pub md5: Option<bool>,
    #[serde(default)]
    pub crc32: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendFilePayloadResponse {
    #[serde(default)]
    pub size: i64,
    #[serde(default)]
    pub md5: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BatchUpdateFilePropertiesRequest<'a> {
    pub name: &'a str,
    pub folder_slug: &'a str,
    pub description: &'a str,
    pub slugs: Vec<&'a str>,
    pub upload_tokens: std::collections::HashMap<&'a str, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommitUploadBatchRequest<'a> {
    pub status: &'a str,
    pub owner_login: &'a str,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateFileRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_slug: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RenameFolderRequest<'a> {
    pub name: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct MoveFolderRequest<'a> {
    pub slugs: Vec<&'a str>,
    pub parent_folder_slug: &'a str,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeleteFoldersRequest<'a> {
    pub slugs: Vec<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DownloadLinkRequest<'a> {
    pub file_slug: &'a str,
    pub user_login: &'a str,
    pub device_id: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DownloadLinkResponse {
    pub link: String,
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub download_url_valid_until: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FolderSize {
    #[serde(default)]
    pub files_size: i64,
    #[serde(default)]
    pub files_count: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct FolderSizes {
    #[serde(default)]
    pub recursive: FolderSize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file_list() {
        let json = r#"{"metadata":{"RunAt":"2026-11-10T10:00:00+01:00","offset":0,"limit":500,"items_count":1},
          "items":[{"slug":"AbC123","name":"x.bin","filesize":49,"description":"","folder_slug":"F1",
          "last_user_modified":"2026-11-09T08:30:00+01:00","upload_time":"2026-11-09T08:30:00+01:00",
          "is_in_trash":false,"is_incomplete":false,"content_status":"normal","format":{},"processing":{}}]}"#;
        let r: ListFilesResponse = serde_json::from_str(json).unwrap();
        assert_eq!(r.items[0].filesize, 49);
        assert_eq!(r.items[0].folder_slug.as_deref(), Some("F1"));
    }

    #[test]
    fn parses_error() {
        let e: ApiError =
            serde_json::from_str(r#"{"error":70001,"code":401,"message":"Invalid user token."}"#)
                .unwrap();
        assert_eq!(e.error_code, 70001);
    }
}
