use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use chrono::{DateTime, Utc};

/// User model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: Option<String>,
    #[serde(skip_serializing)]
    pub password_hash: String,
    pub email_verified: bool,
    #[serde(skip_serializing)]
    pub verification_token: Option<String>,
    #[serde(skip_serializing)]
    pub verification_token_expires_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Item (downloaded file) model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Item {
    pub id: i64,
    pub filename: String,
    pub original_query: String,
    pub file_path: String,
    pub file_size: i64,
    pub bitrate: Option<i32>,
    pub duration: Option<i32>,
    pub extension: String,
    pub source_username: String,
    pub download_status: String,
    pub download_progress: f64,
    pub error_message: Option<String>,
    pub metadata: Option<String>, // JSON
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// Download status enum
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadStatus {
    Pending,
    Downloading,
    Completed,
    Failed,
}

impl DownloadStatus {
    pub fn as_str(&self) -> &str {
        match self {
            DownloadStatus::Pending => "pending",
            DownloadStatus::Downloading => "downloading",
            DownloadStatus::Completed => "completed",
            DownloadStatus::Failed => "failed",
        }
    }
}

impl From<String> for DownloadStatus {
    fn from(s: String) -> Self {
        match s.as_str() {
            "pending" => DownloadStatus::Pending,
            "downloading" => DownloadStatus::Downloading,
            "completed" => DownloadStatus::Completed,
            "failed" => DownloadStatus::Failed,
            _ => DownloadStatus::Pending,
        }
    }
}

/// List model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct List {
    pub id: i64,
    pub name: String,
    pub user_id: i64,
    pub status: String,
    pub total_items: i32,
    pub completed_items: i32,
    pub failed_items: i32,
    pub created_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// List with items
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListWithItems {
    #[serde(flatten)]
    pub list: List,
    pub items: Vec<Item>,
}

/// List item junction model
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct ListItem {
    pub id: i64,
    pub list_id: i64,
    pub item_id: i64,
    pub position: i32,
    pub created_at: DateTime<Utc>,
}

/// Create user request
#[derive(Debug, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub email: String,
    pub password: String,
}

/// Login request - identifier can be email or username
#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub identifier: String,
    pub password: String,
}

/// Login response
#[derive(Debug, Serialize)]
pub struct LoginResponse {
    pub token: String,
    pub user: UserResponse,
}

/// User response (without password hash)
#[derive(Debug, Serialize)]
pub struct UserResponse {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub email_verified: bool,
    pub created_at: DateTime<Utc>,
}

/// Search request
#[derive(Debug, Deserialize)]
pub struct SearchRequest {
    pub query: String,
}

/// Search list request
#[derive(Debug, Deserialize)]
pub struct SearchListRequest {
    pub name: Option<String>,
    pub queries: Vec<String>,
}

/// Item metadata for additional file information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ItemMetadata {
    pub artist: Option<String>,
    pub album: Option<String>,
    pub title: Option<String>,
    pub year: Option<i32>,
    pub genre: Option<String>,
}

/// Email verification request
#[derive(Debug, Deserialize)]
pub struct VerifyEmailRequest {
    pub token: String,
}

/// Resend verification email request
#[derive(Debug, Deserialize)]
pub struct ResendVerificationRequest {
    pub email: String,
}

/// Generic message response
#[derive(Debug, Serialize)]
pub struct MessageResponse {
    pub message: String,
}
