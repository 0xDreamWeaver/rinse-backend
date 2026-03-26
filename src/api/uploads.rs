//! Upload and sharing API endpoints

use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::api::{AppState, AuthUser};
use crate::services::upload::UploadStats;

/// Response for sharing statistics
#[derive(Debug, Serialize)]
pub struct SharingStatsResponse {
    pub folder_count: u32,
    pub file_count: u32,
    pub enabled: bool,
}

/// Response for active/queued upload entries
#[derive(Debug, Serialize)]
pub struct UploadEntryResponse {
    pub id: u64,
    pub username: String,
    pub filename: String,
    pub file_size: u64,
    pub status: String,
    pub queued_secs_ago: u64,
}

/// Response for listing uploads
#[derive(Debug, Serialize)]
pub struct UploadsListResponse {
    pub entries: Vec<UploadEntryResponse>,
}

/// Request body for updating upload config
#[derive(Debug, Deserialize)]
pub struct UpdateUploadConfigRequest {
    pub max_upload_slots: Option<usize>,
    pub max_upload_speed_kbps: Option<u64>,
    pub sharing_enabled: Option<bool>,
}

/// Upload config response
#[derive(Debug, Serialize)]
pub struct UploadConfigResponse {
    pub max_upload_slots: usize,
    pub max_upload_speed_kbps: u64,
    pub sharing_enabled: bool,
}

/// Get current upload status (active, queued, speed, share counts)
///
/// GET /api/uploads
pub async fn get_upload_status(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<UploadStats>, (StatusCode, String)> {
    let stats = state.upload_service.stats().await;
    Ok(Json(stats))
}

/// Get upload configuration
///
/// GET /api/uploads/config
pub async fn get_upload_config(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<UploadConfigResponse>, (StatusCode, String)> {
    let config = state.upload_service.get_config().await;
    Ok(Json(UploadConfigResponse {
        max_upload_slots: config.max_upload_slots,
        max_upload_speed_kbps: config.max_upload_speed_kbps,
        sharing_enabled: config.sharing_enabled,
    }))
}

/// Update upload configuration
///
/// PUT /api/uploads/config
pub async fn update_upload_config(
    State(state): State<AppState>,
    _user: AuthUser,
    Json(req): Json<UpdateUploadConfigRequest>,
) -> Result<Json<UploadConfigResponse>, (StatusCode, String)> {
    let mut config = state.upload_service.get_config().await;

    if let Some(slots) = req.max_upload_slots {
        config.max_upload_slots = slots;
    }
    if let Some(speed) = req.max_upload_speed_kbps {
        config.max_upload_speed_kbps = speed;
    }
    if let Some(enabled) = req.sharing_enabled {
        config.sharing_enabled = enabled;
    }

    state.upload_service.update_config(config.clone()).await;

    Ok(Json(UploadConfigResponse {
        max_upload_slots: config.max_upload_slots,
        max_upload_speed_kbps: config.max_upload_speed_kbps,
        sharing_enabled: config.sharing_enabled,
    }))
}

/// List active and queued uploads
///
/// GET /api/uploads/active
pub async fn get_active_uploads(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<UploadsListResponse>, (StatusCode, String)> {
    let entries = state.upload_service.get_queue_entries().await;
    let now = std::time::Instant::now();

    let entries: Vec<UploadEntryResponse> = entries.iter().map(|e| {
        let status = match &e.status {
            crate::services::upload::UploadStatus::Queued => "queued".to_string(),
            crate::services::upload::UploadStatus::Transferring { bytes_sent, .. } => {
                format!("transferring ({}B sent)", bytes_sent)
            }
            crate::services::upload::UploadStatus::Completed { bytes_sent, .. } => {
                format!("completed ({}B)", bytes_sent)
            }
            crate::services::upload::UploadStatus::Failed { error } => {
                format!("failed: {}", error)
            }
        };

        UploadEntryResponse {
            id: e.id,
            username: e.username.clone(),
            filename: e.virtual_path.clone(),
            file_size: e.file_size,
            status,
            queued_secs_ago: now.duration_since(e.queued_at).as_secs(),
        }
    }).collect();

    Ok(Json(UploadsListResponse { entries }))
}

/// Get sharing statistics
///
/// GET /api/sharing/stats
pub async fn get_sharing_stats(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<SharingStatsResponse>, (StatusCode, String)> {
    let (folder_count, file_count) = state.sharing_service.counts().await;
    let enabled = state.sharing_service.is_enabled();

    Ok(Json(SharingStatsResponse {
        folder_count,
        file_count,
        enabled,
    }))
}

/// Trigger a rescan of the storage directory
///
/// POST /api/sharing/rescan
pub async fn rescan_shares(
    State(state): State<AppState>,
    _user: AuthUser,
) -> Result<Json<SharingStatsResponse>, (StatusCode, String)> {
    match state.sharing_service.scan().await {
        Ok((folders, files)) => {
            tracing::info!("[Sharing] Rescan complete: {} files in {} folders", files, folders);
            Ok(Json(SharingStatsResponse {
                folder_count: folders,
                file_count: files,
                enabled: state.sharing_service.is_enabled(),
            }))
        }
        Err(e) => {
            tracing::error!("[Sharing] Rescan failed: {}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Rescan failed: {}", e)))
        }
    }
}
