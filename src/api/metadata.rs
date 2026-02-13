//! Metadata API endpoints
//!
//! Provides endpoints for:
//! - Getting metadata for an item
//! - Manually refreshing metadata (with 24-hour rate limit)
//! - Starting a library-wide metadata scan job
//! - Getting job status

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::api::{AppState, AuthUser};
use crate::models::TrackMetadata;

/// Static flag to track if a metadata job is running
static METADATA_JOB_RUNNING: AtomicBool = AtomicBool::new(false);

/// Error response for metadata endpoints
#[derive(Debug, Serialize)]
pub struct MetadataError {
    pub error: String,
    pub code: String,
}

impl IntoResponse for MetadataError {
    fn into_response(self) -> Response {
        let status = match self.code.as_str() {
            "not_found" => StatusCode::NOT_FOUND,
            "rate_limited" => StatusCode::TOO_MANY_REQUESTS,
            "job_running" => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };

        (status, Json(self)).into_response()
    }
}

/// Response for metadata refresh
#[derive(Debug, Serialize)]
pub struct MetadataRefreshResponse {
    pub item_id: i64,
    pub metadata: TrackMetadata,
    pub message: String,
}

/// Response for starting a metadata job
#[derive(Debug, Serialize)]
pub struct JobStartedResponse {
    pub message: String,
    pub total_items: i64,
}

/// Response for job status
#[derive(Debug, Serialize)]
pub struct JobStatusResponse {
    pub running: bool,
    pub items_without_metadata: i64,
}

/// Get metadata for an item
///
/// GET /api/items/:id/metadata
pub async fn get_item_metadata(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<TrackMetadata>, MetadataError> {
    // Check if item exists
    let item = state
        .db
        .get_item(id)
        .await
        .map_err(|e| MetadataError {
            error: format!("Database error: {}", e),
            code: "internal_error".to_string(),
        })?
        .ok_or_else(|| MetadataError {
            error: format!("Item not found: {}", id),
            code: "not_found".to_string(),
        })?;

    // Get metadata from database
    let metadata = state
        .metadata_service
        .get_metadata(id)
        .await
        .map_err(|e| MetadataError {
            error: format!("Failed to get metadata: {}", e),
            code: "internal_error".to_string(),
        })?
        .unwrap_or_else(|| {
            // Return empty metadata with just the filename as title
            let mut empty = TrackMetadata::default();
            empty.title = Some(item.filename.clone());
            empty
        });

    Ok(Json(metadata))
}

/// Manually refresh metadata for an item (rate limited to once per 24 hours)
///
/// POST /api/items/:id/metadata/refresh
pub async fn refresh_item_metadata(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<MetadataRefreshResponse>, MetadataError> {
    // Check if item exists
    let _item = state
        .db
        .get_item(id)
        .await
        .map_err(|e| MetadataError {
            error: format!("Database error: {}", e),
            code: "internal_error".to_string(),
        })?
        .ok_or_else(|| MetadataError {
            error: format!("Item not found: {}", id),
            code: "not_found".to_string(),
        })?;

    // Check rate limit
    let can_refresh = state
        .metadata_service
        .can_refresh(id)
        .await
        .map_err(|e| MetadataError {
            error: format!("Failed to check rate limit: {}", e),
            code: "internal_error".to_string(),
        })?;

    if !can_refresh {
        return Err(MetadataError {
            error: "Metadata can only be refreshed once per 24 hours".to_string(),
            code: "rate_limited".to_string(),
        });
    }

    // Perform refresh
    let metadata = state
        .metadata_service
        .refresh_metadata(id)
        .await
        .map_err(|e| MetadataError {
            error: format!("Failed to refresh metadata: {}", e),
            code: "internal_error".to_string(),
        })?;

    Ok(Json(MetadataRefreshResponse {
        item_id: id,
        metadata,
        message: "Metadata refreshed successfully".to_string(),
    }))
}

/// Response for clearing metadata
#[derive(Debug, Serialize)]
pub struct MetadataClearResponse {
    pub item_id: i64,
    pub message: String,
}

/// Clear all metadata for an item
///
/// DELETE /api/items/:id/metadata
pub async fn clear_item_metadata(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<MetadataClearResponse>, MetadataError> {
    // Check if item exists
    let _item = state
        .db
        .get_item(id)
        .await
        .map_err(|e| MetadataError {
            error: format!("Database error: {}", e),
            code: "internal_error".to_string(),
        })?
        .ok_or_else(|| MetadataError {
            error: format!("Item not found: {}", id),
            code: "not_found".to_string(),
        })?;

    // Clear metadata
    state
        .db
        .clear_item_metadata(id)
        .await
        .map_err(|e| MetadataError {
            error: format!("Failed to clear metadata: {}", e),
            code: "internal_error".to_string(),
        })?;

    tracing::info!("[Metadata] Cleared metadata for item {}", id);

    Ok(Json(MetadataClearResponse {
        item_id: id,
        message: "Metadata cleared successfully".to_string(),
    }))
}

/// Start a library-wide metadata scan job
///
/// POST /api/metadata/job
pub async fn start_metadata_job(
    _auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<JobStartedResponse>, MetadataError> {
    // Check if a job is already running
    if METADATA_JOB_RUNNING.swap(true, Ordering::SeqCst) {
        return Err(MetadataError {
            error: "A metadata job is already running".to_string(),
            code: "job_running".to_string(),
        });
    }

    // Count items without metadata
    let items = state
        .db
        .get_items_without_metadata()
        .await
        .map_err(|e| {
            METADATA_JOB_RUNNING.store(false, Ordering::SeqCst);
            MetadataError {
                error: format!("Failed to get items: {}", e),
                code: "internal_error".to_string(),
            }
        })?;

    let total_items = items.len() as i64;

    if total_items == 0 {
        METADATA_JOB_RUNNING.store(false, Ordering::SeqCst);
        return Ok(Json(JobStartedResponse {
            message: "No items need metadata lookup".to_string(),
            total_items: 0,
        }));
    }

    // Spawn background job
    let metadata_service: Arc<crate::services::MetadataService> = Arc::clone(&state.metadata_service);
    tokio::spawn(async move {
        tracing::info!(
            "[MetadataJob] Starting library scan for {} items",
            total_items
        );

        let result = metadata_service.run_library_scan().await;

        match result {
            Ok(scan_result) => {
                tracing::info!(
                    "[MetadataJob] Completed: total={}, completed={}, failed={}",
                    scan_result.total,
                    scan_result.completed,
                    scan_result.failed
                );
            }
            Err(e) => {
                tracing::error!("[MetadataJob] Failed: {}", e);
            }
        }

        METADATA_JOB_RUNNING.store(false, Ordering::SeqCst);
    });

    Ok(Json(JobStartedResponse {
        message: format!("Metadata job started for {} items", total_items),
        total_items,
    }))
}

/// Get metadata job status
///
/// GET /api/metadata/job
pub async fn get_job_status(
    _auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<JobStatusResponse>, MetadataError> {
    let running = METADATA_JOB_RUNNING.load(Ordering::SeqCst);

    let items_without_metadata = state
        .db
        .get_items_without_metadata()
        .await
        .map_err(|e| MetadataError {
            error: format!("Failed to get items: {}", e),
            code: "internal_error".to_string(),
        })?
        .len() as i64;

    Ok(Json(JobStatusResponse {
        running,
        items_without_metadata,
    }))
}
