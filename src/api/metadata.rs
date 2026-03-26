//! Metadata API endpoints
//!
//! Provides endpoints for:
//! - Getting metadata for an item
//! - Manually refreshing metadata (with 24-hour rate limit)
//! - Starting a library-wide metadata scan job
//! - Getting job status

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;

use crate::api::{AppState, AuthUser, require_admin};
use crate::models::TrackMetadata;

/// Static flag to track if a metadata job is running
static METADATA_JOB_RUNNING: AtomicBool = AtomicBool::new(false);
/// Total items when the metadata job was started
static METADATA_JOB_TOTAL: AtomicI64 = AtomicI64::new(0);

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
            "forbidden" => StatusCode::FORBIDDEN,
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
    pub total: i64,
    pub processed: i64,
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

/// Query parameters for metadata refresh
#[derive(Debug, Deserialize)]
pub struct RefreshQuery {
    /// If true, skip API lookups for items that already have artist + title + cover art
    pub skip_existing: Option<bool>,
}

/// Manually refresh metadata for an item (rate limited to once per 24 hours)
///
/// POST /api/items/:id/metadata/refresh?skip_existing=true
pub async fn refresh_item_metadata(
    _auth: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<RefreshQuery>,
) -> Result<Json<MetadataRefreshResponse>, MetadataError> {
    let skip_existing = query.skip_existing.unwrap_or(false);

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
        .refresh_metadata(id, skip_existing)
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
    require_admin(&_auth).map_err(|_| MetadataError {
        error: "Admin access required".to_string(),
        code: "forbidden".to_string(),
    })?;

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

    // Track total for progress reporting
    METADATA_JOB_TOTAL.store(total_items, Ordering::SeqCst);

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
        METADATA_JOB_TOTAL.store(0, Ordering::SeqCst);
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
    let total = METADATA_JOB_TOTAL.load(Ordering::SeqCst);

    let items_without_metadata = state
        .db
        .get_items_without_metadata()
        .await
        .map_err(|e| MetadataError {
            error: format!("Failed to get items: {}", e),
            code: "internal_error".to_string(),
        })?
        .len() as i64;

    // Processed = total at start - remaining without metadata
    let processed = if running && total > 0 {
        (total - items_without_metadata).max(0)
    } else {
        0
    };

    Ok(Json(JobStatusResponse {
        running,
        total,
        processed,
        items_without_metadata,
    }))
}

// ============================================================================
// Cover Art Backfill
// ============================================================================

/// Static flag to track if a cover backfill job is running
static COVER_BACKFILL_RUNNING: AtomicBool = AtomicBool::new(false);
/// Total items when the cover backfill job was started
static COVER_BACKFILL_TOTAL: AtomicI64 = AtomicI64::new(0);
/// Items processed so far in the cover backfill job
static COVER_BACKFILL_PROCESSED: AtomicI64 = AtomicI64::new(0);
/// Items that failed during the cover backfill job
static COVER_BACKFILL_FAILED: AtomicI64 = AtomicI64::new(0);

/// Response for starting a cover backfill job
#[derive(Debug, Serialize)]
pub struct CoverBackfillResponse {
    pub message: String,
    pub total_items: i64,
}

/// Response for cover backfill status
#[derive(Debug, Serialize)]
pub struct CoverBackfillStatusResponse {
    pub running: bool,
    pub total: i64,
    pub processed: i64,
    pub failed: i64,
}

/// Start a background job to cache cover art for all items with uncached covers
///
/// POST /api/covers/backfill
pub async fn start_cover_backfill(
    _auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<CoverBackfillResponse>, MetadataError> {
    require_admin(&_auth).map_err(|_| MetadataError {
        error: "Admin access required".to_string(),
        code: "forbidden".to_string(),
    })?;

    if COVER_BACKFILL_RUNNING.swap(true, Ordering::SeqCst) {
        return Err(MetadataError {
            error: "A cover backfill job is already running".to_string(),
            code: "job_running".to_string(),
        });
    }

    let items = state
        .db
        .get_items_with_uncached_covers()
        .await
        .map_err(|e| {
            COVER_BACKFILL_RUNNING.store(false, Ordering::SeqCst);
            MetadataError {
                error: format!("Failed to get items: {}", e),
                code: "internal_error".to_string(),
            }
        })?;

    let total_items = items.len() as i64;

    if total_items == 0 {
        COVER_BACKFILL_RUNNING.store(false, Ordering::SeqCst);
        return Ok(Json(CoverBackfillResponse {
            message: "No items need cover art caching".to_string(),
            total_items: 0,
        }));
    }

    // Track progress
    COVER_BACKFILL_TOTAL.store(total_items, Ordering::SeqCst);
    COVER_BACKFILL_PROCESSED.store(0, Ordering::SeqCst);
    COVER_BACKFILL_FAILED.store(0, Ordering::SeqCst);

    let metadata_service = Arc::clone(&state.metadata_service);
    let db = state.db.clone();

    tokio::spawn(async move {
        tracing::info!(
            "[CoverBackfill] Starting backfill for {} items",
            total_items
        );

        let mut cached = 0i64;
        let mut failed = 0i64;

        for item in items {
            if let Some(ref art_url) = item.meta_album_art_url {
                match metadata_service
                    .cover_cache()
                    .cache_cover(item.id, art_url)
                    .await
                {
                    Ok(true) => {
                        let _ = db.set_cover_cached(item.id, true).await;
                        cached += 1;
                    }
                    Ok(false) => {
                        failed += 1;
                        COVER_BACKFILL_FAILED.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) => {
                        failed += 1;
                        COVER_BACKFILL_FAILED.fetch_add(1, Ordering::SeqCst);
                        tracing::debug!(
                            "[CoverBackfill] Failed for item {}: {}",
                            item.id,
                            e
                        );
                    }
                }
            }

            COVER_BACKFILL_PROCESSED.fetch_add(1, Ordering::SeqCst);

            // Rate limit: 200ms delay between downloads
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }

        tracing::info!(
            "[CoverBackfill] Complete: {} cached, {} failed out of {} total",
            cached,
            failed,
            total_items
        );

        COVER_BACKFILL_RUNNING.store(false, Ordering::SeqCst);
    });

    Ok(Json(CoverBackfillResponse {
        message: format!("Cover backfill started for {} items", total_items),
        total_items,
    }))
}

/// Get cover backfill job status
///
/// GET /api/covers/backfill
pub async fn get_cover_backfill_status(
    _auth: AuthUser,
    State(_state): State<AppState>,
) -> Json<CoverBackfillStatusResponse> {
    Json(CoverBackfillStatusResponse {
        running: COVER_BACKFILL_RUNNING.load(Ordering::SeqCst),
        total: COVER_BACKFILL_TOTAL.load(Ordering::SeqCst),
        processed: COVER_BACKFILL_PROCESSED.load(Ordering::SeqCst),
        failed: COVER_BACKFILL_FAILED.load(Ordering::SeqCst),
    })
}
