//! Queue API endpoints for managing the search queue

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};

use crate::api::{AppState, AuthUser};
use crate::models::{
    QueuedSearch, EnqueueSearchRequest, EnqueueListRequest,
    EnqueueSearchResponse, EnqueueListResponse, QueueStatusResponse,
};

/// Response for queue items list
#[derive(Debug, Serialize)]
pub struct QueueItemsResponse {
    pub items: Vec<QueuedSearch>,
    pub pending: i64,
    pub processing: i64,
}

/// Response for cancel operation
#[derive(Debug, Serialize)]
pub struct CancelResponse {
    pub cancelled: bool,
    pub message: String,
}

/// Get queue status
///
/// GET /api/queue
pub async fn get_queue_status(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<QueueStatusResponse>, (StatusCode, String)> {
    match state.queue_service.get_user_queue_status(user.id).await {
        Ok(status) => Ok(Json(status)),
        Err(e) => {
            tracing::error!("[Queue API] Failed to get queue status: {}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}

/// Get queue items for the current user
///
/// GET /api/queue/items
pub async fn get_queue_items(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<QueueItemsResponse>, (StatusCode, String)> {
    let items = state.queue_service.get_user_pending_queue(user.id).await
        .map_err(|e| {
            tracing::error!("[Queue API] Failed to get queue items: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    let (pending, processing) = state.db.get_user_queue_status(user.id).await
        .map_err(|e| {
            tracing::error!("[Queue API] Failed to get queue status: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    Ok(Json(QueueItemsResponse {
        items,
        pending,
        processing,
    }))
}

/// Enqueue a single search
///
/// POST /api/queue/search
pub async fn enqueue_search(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<EnqueueSearchRequest>,
) -> Result<Json<EnqueueSearchResponse>, (StatusCode, String)> {
    if request.track.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Track name cannot be empty".to_string()));
    }

    // Build combined query for Soulseek search
    let query = request.search_query();

    match state.queue_service.enqueue_search(
        user.id,
        &query,
        request.artist.as_deref(),
        Some(request.track.as_str()),
        request.format.as_deref(),
        request.client_id.as_deref(),
    ).await {
        Ok((queued, position)) => {
            tracing::info!(
                "[Queue API] Enqueued search: id={}, artist={:?}, track='{}', query='{}', position={}, client_id={:?}",
                queued.id, request.artist, request.track, query, position, request.client_id
            );
            Ok(Json(EnqueueSearchResponse {
                queue_id: queued.id,
                track: request.track,
                artist: request.artist,
                query,
                position,
                client_id: request.client_id,
            }))
        }
        Err(e) => {
            tracing::error!("[Queue API] Failed to enqueue search: {}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}

/// Enqueue a list of searches
///
/// POST /api/queue/list
pub async fn enqueue_list(
    State(state): State<AppState>,
    user: AuthUser,
    Json(request): Json<EnqueueListRequest>,
) -> Result<Json<EnqueueListResponse>, (StatusCode, String)> {
    if request.tracks.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Tracks list cannot be empty".to_string()));
    }

    // Convert tracks to (query, artist, track) tuples and filter out empty tracks
    let tracks: Vec<(String, Option<String>, Option<String>)> = request.tracks.into_iter()
        .filter(|t| !t.track.trim().is_empty())
        .map(|t| {
            // Build combined query
            let query = match &t.artist {
                Some(artist) if !artist.trim().is_empty() => {
                    format!("{} {}", artist.trim(), t.track.trim())
                }
                _ => t.track.trim().to_string(),
            };
            (query, t.artist, Some(t.track))
        })
        .collect();

    if tracks.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "All tracks are empty".to_string()));
    }

    match state.queue_service.enqueue_list(
        user.id,
        request.name,
        tracks.clone(),
        request.format,
    ).await {
        Ok((list, queued_searches)) => {
            tracing::info!(
                "[Queue API] Enqueued list: id={}, name='{}', items={}",
                list.id, list.name, queued_searches.len()
            );
            Ok(Json(EnqueueListResponse {
                list_id: list.id,
                list_name: list.name,
                queue_ids: queued_searches.iter().map(|q| q.id).collect(),
                total_queued: queued_searches.len() as i32,
            }))
        }
        Err(e) => {
            tracing::error!("[Queue API] Failed to enqueue list: {}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}

/// Cancel a pending search
///
/// DELETE /api/queue/:id
pub async fn cancel_search(
    State(state): State<AppState>,
    _user: AuthUser,
    Path(queue_id): Path<i64>,
) -> Result<Json<CancelResponse>, (StatusCode, String)> {
    match state.queue_service.cancel_search(queue_id).await {
        Ok(cancelled) => {
            let message = if cancelled {
                format!("Search {} cancelled", queue_id)
            } else {
                format!("Search {} not found or already processing", queue_id)
            };
            Ok(Json(CancelResponse { cancelled, message }))
        }
        Err(e) => {
            tracing::error!("[Queue API] Failed to cancel search: {}", e);
            Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}
