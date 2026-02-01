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
    if request.query.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Query cannot be empty".to_string()));
    }

    match state.queue_service.enqueue_search(
        user.id,
        &request.query,
        request.format.as_deref(),
        request.client_id.as_deref(),
    ).await {
        Ok((queued, position)) => {
            tracing::info!(
                "[Queue API] Enqueued search: id={}, query='{}', position={}, client_id={:?}",
                queued.id, request.query, position, request.client_id
            );
            Ok(Json(EnqueueSearchResponse {
                queue_id: queued.id,
                query: request.query,
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
    if request.queries.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Queries list cannot be empty".to_string()));
    }

    // Filter out empty queries
    let queries: Vec<String> = request.queries.into_iter()
        .filter(|q| !q.trim().is_empty())
        .collect();

    if queries.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "All queries are empty".to_string()));
    }

    match state.queue_service.enqueue_list(
        user.id,
        request.name,
        queries.clone(),
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
