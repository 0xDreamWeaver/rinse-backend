use axum::{
    extract::{Path, State, Query},
    http::StatusCode,
    Json,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use axum::body::Body;
use axum::http::header;

use crate::api::AppState;
use crate::models::{Item, SearchRequest};

/// Error response
#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Batch delete request
#[derive(Deserialize)]
pub struct BatchDeleteRequest {
    ids: Vec<i64>,
}

/// List all items
pub async fn list_items(
    State(state): State<AppState>,
) -> Result<Json<Vec<Item>>, Response> {
    let items = state.download_service.get_all_items()
        .await
        .map_err(|e| {
            tracing::error!("Failed to get items: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to get items".to_string()
            })).into_response()
        })?;

    Ok(Json(items))
}

/// Get a single item
pub async fn get_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Item>, Response> {
    let item = state.download_service.get_item(id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get item: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to get item".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::NOT_FOUND, Json(ErrorResponse {
                error: "Item not found".to_string()
            })).into_response()
        })?;

    Ok(Json(item))
}

/// Delete an item
pub async fn delete_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Response> {
    tracing::info!("=== DELETE ITEM REQUEST ===");
    tracing::info!("Item ID: {}", id);

    state.download_service.delete_item(id)
        .await
        .map_err(|e| {
            tracing::error!("=== DELETE ITEM FAILED ===");
            tracing::error!("Item ID: {} - Error: {}", id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to delete item".to_string()
            })).into_response()
        })?;

    tracing::info!("=== DELETE ITEM SUCCESS ===");
    tracing::info!("Deleted item ID: {}", id);

    Ok(StatusCode::NO_CONTENT)
}

/// Batch delete items
pub async fn batch_delete_items(
    State(state): State<AppState>,
    Json(payload): Json<BatchDeleteRequest>,
) -> Result<StatusCode, Response> {
    tracing::info!("=== BATCH DELETE ITEMS REQUEST ===");
    tracing::info!("Item IDs: {:?}", payload.ids);
    tracing::info!("Count: {}", payload.ids.len());

    state.download_service.delete_items(payload.ids.clone())
        .await
        .map_err(|e| {
            tracing::error!("=== BATCH DELETE ITEMS FAILED ===");
            tracing::error!("Item IDs: {:?} - Error: {}", payload.ids, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to batch delete items".to_string()
            })).into_response()
        })?;

    tracing::info!("=== BATCH DELETE ITEMS SUCCESS ===");
    tracing::info!("Deleted {} items", payload.ids.len());

    Ok(StatusCode::NO_CONTENT)
}

/// Download an item file
pub async fn download_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let item = state.download_service.get_item(id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get item: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to get item".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::NOT_FOUND, Json(ErrorResponse {
                error: "Item not found".to_string()
            })).into_response()
        })?;

    // Check if file is completed
    if item.download_status != "completed" {
        return Err((StatusCode::CONFLICT, Json(ErrorResponse {
            error: format!("Item not ready for download. Status: {}", item.download_status)
        })).into_response());
    }

    // Open file and stream it
    let file = File::open(&item.file_path)
        .await
        .map_err(|e| {
            tracing::error!("Failed to open file: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to open file".to_string()
            })).into_response()
        })?;

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let content_type = mime_guess::from_path(&item.file_path)
        .first_or_octet_stream()
        .to_string();

    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", item.filename)),
        ],
        body
    ).into_response())
}

/// Search for and download an item
pub async fn search_item(
    State(state): State<AppState>,
    Json(payload): Json<SearchRequest>,
) -> Result<Json<Item>, Response> {
    // For now, assume user_id = 1 (will be extracted from JWT later)
    let user_id = 1;

    tracing::info!("=== SEARCH REQUEST ===");
    tracing::info!("Query: '{}'", payload.query);
    tracing::info!("Format filter: {:?}", payload.format);
    tracing::info!("User ID: {}", user_id);

    let item = state.download_service.search_and_download_item(&payload.query, user_id, payload.format.as_deref())
        .await
        .map_err(|e| {
            tracing::error!("=== SEARCH FAILED ===");
            tracing::error!("Query: '{}' - Error: {}", payload.query, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: e.to_string()
            })).into_response()
        })?;

    tracing::info!("=== SEARCH SUCCESS ===");
    tracing::info!("Downloaded: {} ({})", item.filename, item.download_status);

    Ok(Json(item))
}
