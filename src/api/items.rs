use axum::{
    extract::{Path, State, Query},
    http::{StatusCode, HeaderMap, HeaderValue},
    Json,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncReadExt};
use tokio_util::io::ReaderStream;
use axum::body::Body;
use axum::http::header;
use std::io::SeekFrom;

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

/// Parse Range header value like "bytes=0-1023" or "bytes=1024-"
fn parse_range_header(range_header: &str, file_size: u64) -> Option<(u64, u64)> {
    let range_str = range_header.strip_prefix("bytes=")?;
    let parts: Vec<&str> = range_str.split('-').collect();

    if parts.len() != 2 {
        return None;
    }

    let start: u64 = if parts[0].is_empty() {
        // Suffix range like "-500" means last 500 bytes
        let suffix_len: u64 = parts[1].parse().ok()?;
        file_size.saturating_sub(suffix_len)
    } else {
        parts[0].parse().ok()?
    };

    let end: u64 = if parts[1].is_empty() {
        // Open-ended range like "1024-"
        file_size - 1
    } else {
        parts[1].parse().ok()?
    };

    // Validate range
    if start > end || start >= file_size {
        return None;
    }

    // Clamp end to file size
    let end = end.min(file_size - 1);

    Some((start, end))
}

/// Download an item file with Range request support for audio streaming
pub async fn download_item(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: HeaderMap,
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

    let file_size = item.file_size as u64;
    let content_type = mime_guess::from_path(&item.file_path)
        .first_or_octet_stream()
        .to_string();

    // Check for Range header
    let range_header = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok());

    if let Some(range_str) = range_header {
        // Parse range and return partial content
        if let Some((start, end)) = parse_range_header(range_str, file_size) {
            let mut file = File::open(&item.file_path)
                .await
                .map_err(|e| {
                    tracing::error!("Failed to open file: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                        error: "Failed to open file".to_string()
                    })).into_response()
                })?;

            // Seek to start position
            file.seek(SeekFrom::Start(start))
                .await
                .map_err(|e| {
                    tracing::error!("Failed to seek file: {}", e);
                    (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                        error: "Failed to seek file".to_string()
                    })).into_response()
                })?;

            let content_length = end - start + 1;

            // Create a limited reader for the range
            let limited_file = file.take(content_length);
            let stream = ReaderStream::new(limited_file);
            let body = Body::from_stream(stream);

            let content_range = format!("bytes {}-{}/{}", start, end, file_size);

            return Ok(Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, content_type)
                .header(header::CONTENT_LENGTH, content_length.to_string())
                .header(header::CONTENT_RANGE, content_range)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"{}\"", item.filename))
                .body(body)
                .unwrap()
                .into_response());
        } else {
            // Invalid range
            return Err(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{}", file_size))
                .body(Body::empty())
                .unwrap()
                .into_response());
        }
    }

    // No range header - return full file
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

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, file_size.to_string())
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_DISPOSITION, format!("inline; filename=\"{}\"", item.filename))
        .body(body)
        .unwrap()
        .into_response())
}

// =============================================================================
// LEGACY ENDPOINT - COMMENTED OUT
// Use POST /api/queue/search instead for non-blocking queue-based search
// =============================================================================
// /// Search for and download an item
// pub async fn search_item(
//     State(state): State<AppState>,
//     Json(payload): Json<SearchRequest>,
// ) -> Result<Json<Item>, Response> {
//     // For now, assume user_id = 1 (will be extracted from JWT later)
//     let user_id = 1;
//
//     tracing::info!("=== SEARCH REQUEST ===");
//     tracing::info!("Query: '{}'", payload.query);
//     tracing::info!("Format filter: {:?}", payload.format);
//     tracing::info!("User ID: {}", user_id);
//
//     let item = state.download_service.search_and_download_item(&payload.query, user_id, payload.format.as_deref())
//         .await
//         .map_err(|e| {
//             tracing::error!("=== SEARCH FAILED ===");
//             tracing::error!("Query: '{}' - Error: {}", payload.query, e);
//             (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
//                 error: e.to_string()
//             })).into_response()
//         })?;
//
//     tracing::info!("=== SEARCH SUCCESS ===");
//     tracing::info!("Downloaded: {} ({})", item.filename, item.download_status);
//
//     Ok(Json(item))
// }
