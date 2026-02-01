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
    /// Optional format filter: mp3, flac, m4a, wav
    pub format: Option<String>,
}

/// Search list request
#[derive(Debug, Deserialize)]
pub struct SearchListRequest {
    pub name: Option<String>,
    pub queries: Vec<String>,
    /// Optional format filter: mp3, flac, m4a, wav
    pub format: Option<String>,
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

// ============================================================================
// Search Queue Models
// ============================================================================

/// Queue status enum for search queue entries
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QueueStatus {
    /// Waiting to be processed (first attempt)
    Pending,
    /// Currently being searched
    Processing,
    /// Search succeeded, item created, download initiated
    Completed,
    /// Search failed (no results, network error, etc.)
    Failed,
    /// Waiting for retry (previous attempt failed)
    Retry,
}

impl QueueStatus {
    pub fn as_str(&self) -> &str {
        match self {
            QueueStatus::Pending => "pending",
            QueueStatus::Processing => "processing",
            QueueStatus::Completed => "completed",
            QueueStatus::Failed => "failed",
            QueueStatus::Retry => "retry",
        }
    }
}

impl From<String> for QueueStatus {
    fn from(s: String) -> Self {
        match s.as_str() {
            "pending" => QueueStatus::Pending,
            "processing" => QueueStatus::Processing,
            "completed" => QueueStatus::Completed,
            "failed" => QueueStatus::Failed,
            "retry" => QueueStatus::Retry,
            _ => QueueStatus::Pending,
        }
    }
}

/// Queued search entry
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct QueuedSearch {
    pub id: i64,
    pub user_id: i64,
    pub query: String,
    pub format: Option<String>,
    pub list_id: Option<i64>,
    pub list_position: Option<i32>,
    pub status: String,
    pub item_id: Option<i64>,
    pub error_message: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Number of retry attempts (0 = first attempt, 1+ = retry)
    pub retry_count: i32,
    /// Client-generated ID for frontend tracking (prevents orphaned popups)
    pub client_id: Option<String>,
}

impl QueuedSearch {
    /// Get the status as an enum
    pub fn queue_status(&self) -> QueueStatus {
        QueueStatus::from(self.status.clone())
    }
}

/// Request to enqueue a single search
#[derive(Debug, Deserialize)]
pub struct EnqueueSearchRequest {
    pub query: String,
    pub format: Option<String>,
    /// Client-generated ID for frontend tracking (prevents orphaned popups)
    pub client_id: Option<String>,
}

/// Request to enqueue multiple searches (a list)
#[derive(Debug, Deserialize)]
pub struct EnqueueListRequest {
    pub name: Option<String>,
    pub queries: Vec<String>,
    pub format: Option<String>,
}

/// Response after enqueueing a search
#[derive(Debug, Serialize)]
pub struct EnqueueSearchResponse {
    pub queue_id: i64,
    pub query: String,
    pub position: i64,  // Position in the overall queue
    /// Client-generated ID echoed back for confirmation
    pub client_id: Option<String>,
}

/// Response after enqueueing a list
#[derive(Debug, Serialize)]
pub struct EnqueueListResponse {
    pub list_id: i64,
    pub list_name: String,
    pub queue_ids: Vec<i64>,
    pub total_queued: i32,
}

/// Queue status summary
#[derive(Debug, Serialize)]
pub struct QueueStatusResponse {
    pub pending: i64,
    pub processing: i64,
    pub active_downloads: i64,  // Items with status 'downloading'
    pub user_pending: i64,      // This user's pending items
    pub user_processing: i64,   // This user's processing items
}
