use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio_util::io::ReaderStream;
use axum::body::Body;
use axum::http::header;
use std::io::Write;

use crate::api::{AppState, AuthUser};
use crate::models::{List, ListWithItems, SearchListRequest};

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

/// Batch remove items from list request
#[derive(Deserialize)]
pub struct BatchRemoveItemsRequest {
    item_ids: Vec<i64>,
}

/// List all lists for a user
pub async fn list_lists(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<List>>, Response> {
    let lists = state.download_service.get_user_lists(user.id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get lists: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to get lists".to_string()
            })).into_response()
        })?;

    Ok(Json(lists))
}

/// Get a single list with its items
pub async fn get_list(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<ListWithItems>, Response> {
    let (list, items) = state.download_service.get_list_with_items(id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to get list".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::NOT_FOUND, Json(ErrorResponse {
                error: "List not found".to_string()
            })).into_response()
        })?;

    Ok(Json(ListWithItems { list, items }))
}

/// Delete a list
pub async fn delete_list(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Response> {
    tracing::info!("=== DELETE LIST REQUEST ===");
    tracing::info!("List ID: {}", id);

    state.download_service.delete_list(id)
        .await
        .map_err(|e| {
            tracing::error!("=== DELETE LIST FAILED ===");
            tracing::error!("List ID: {} - Error: {}", id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to delete list".to_string()
            })).into_response()
        })?;

    tracing::info!("=== DELETE LIST SUCCESS ===");
    tracing::info!("Deleted list ID: {}", id);

    Ok(StatusCode::NO_CONTENT)
}

/// Batch delete lists
pub async fn batch_delete_lists(
    State(state): State<AppState>,
    Json(payload): Json<BatchDeleteRequest>,
) -> Result<StatusCode, Response> {
    tracing::info!("=== BATCH DELETE LISTS REQUEST ===");
    tracing::info!("List IDs: {:?}", payload.ids);
    tracing::info!("Count: {}", payload.ids.len());

    state.download_service.delete_lists(payload.ids.clone())
        .await
        .map_err(|e| {
            tracing::error!("=== BATCH DELETE LISTS FAILED ===");
            tracing::error!("List IDs: {:?} - Error: {}", payload.ids, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to batch delete lists".to_string()
            })).into_response()
        })?;

    tracing::info!("=== BATCH DELETE LISTS SUCCESS ===");
    tracing::info!("Deleted {} lists", payload.ids.len());

    Ok(StatusCode::NO_CONTENT)
}

/// Rename list request
#[derive(Deserialize)]
pub struct RenameListRequest {
    pub name: String,
}

/// Rename a list
pub async fn rename_list(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(payload): Json<RenameListRequest>,
) -> Result<StatusCode, Response> {
    tracing::info!("=== RENAME LIST REQUEST ===");
    tracing::info!("List ID: {}, New name: {}", id, payload.name);

    state.db.rename_list(id, &payload.name)
        .await
        .map_err(|e| {
            tracing::error!("=== RENAME LIST FAILED ===");
            tracing::error!("List ID: {} - Error: {}", id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to rename list".to_string()
            })).into_response()
        })?;

    tracing::info!("=== RENAME LIST SUCCESS ===");
    Ok(StatusCode::NO_CONTENT)
}

/// Delete a list and all its items (hard delete)
pub async fn delete_list_with_items(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Response> {
    tracing::info!("=== DELETE LIST WITH ITEMS REQUEST ===");
    tracing::info!("List ID: {}", id);

    // Get item IDs and delete list
    let item_ids = state.db.delete_list_with_items(id)
        .await
        .map_err(|e| {
            tracing::error!("=== DELETE LIST WITH ITEMS FAILED ===");
            tracing::error!("List ID: {} - Error: {}", id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to delete list".to_string()
            })).into_response()
        })?;

    // Hard delete items and their files
    for item_id in &item_ids {
        // Get item to find file path
        if let Ok(Some(item)) = state.db.get_item(*item_id).await {
            // Delete the file
            if !item.file_path.is_empty() {
                if let Err(e) = tokio::fs::remove_file(&item.file_path).await {
                    tracing::warn!("Failed to delete file {}: {}", item.file_path, e);
                }
            }
        }
        // Hard delete the item record
        if let Err(e) = state.db.hard_delete_item(*item_id).await {
            tracing::warn!("Failed to hard delete item {}: {}", item_id, e);
        }
    }

    tracing::info!("=== DELETE LIST WITH ITEMS SUCCESS ===");
    tracing::info!("Deleted list {} and {} items", id, item_ids.len());

    Ok(StatusCode::NO_CONTENT)
}

/// Remove a single item from a list (just removes the association, doesn't delete the item)
pub async fn remove_item_from_list(
    State(state): State<AppState>,
    Path((list_id, item_id)): Path<(i64, i64)>,
) -> Result<StatusCode, Response> {
    tracing::info!("=== REMOVE ITEM FROM LIST REQUEST ===");
    tracing::info!("List ID: {}, Item ID: {}", list_id, item_id);

    state.download_service.remove_item_from_list(list_id, item_id)
        .await
        .map_err(|e| {
            tracing::error!("=== REMOVE ITEM FROM LIST FAILED ===");
            tracing::error!("List ID: {}, Item ID: {} - Error: {}", list_id, item_id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to remove item from list".to_string()
            })).into_response()
        })?;

    tracing::info!("=== REMOVE ITEM FROM LIST SUCCESS ===");
    Ok(StatusCode::NO_CONTENT)
}

/// Batch remove items from a list
pub async fn batch_remove_items_from_list(
    State(state): State<AppState>,
    Path(list_id): Path<i64>,
    Json(payload): Json<BatchRemoveItemsRequest>,
) -> Result<StatusCode, Response> {
    tracing::info!("=== BATCH REMOVE ITEMS FROM LIST REQUEST ===");
    tracing::info!("List ID: {}, Item IDs: {:?}", list_id, payload.item_ids);

    state.download_service.remove_items_from_list(list_id, payload.item_ids.clone())
        .await
        .map_err(|e| {
            tracing::error!("=== BATCH REMOVE ITEMS FROM LIST FAILED ===");
            tracing::error!("List ID: {}, Item IDs: {:?} - Error: {}", list_id, payload.item_ids, e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to remove items from list".to_string()
            })).into_response()
        })?;

    tracing::info!("=== BATCH REMOVE ITEMS FROM LIST SUCCESS ===");
    tracing::info!("Removed {} items from list {}", payload.item_ids.len(), list_id);

    Ok(StatusCode::NO_CONTENT)
}

/// Download a list as a zip file
pub async fn download_list(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Response, Response> {
    let (list, items) = state.download_service.get_list_with_items(id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to get list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to get list".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::NOT_FOUND, Json(ErrorResponse {
                error: "List not found".to_string()
            })).into_response()
        })?;

    // Check if all items are completed
    let incomplete_count = items.iter()
        .filter(|item| item.download_status != "completed")
        .count();

    if incomplete_count > 0 {
        return Err((StatusCode::CONFLICT, Json(ErrorResponse {
            error: format!("{} items not yet downloaded", incomplete_count)
        })).into_response());
    }

    // Create a temporary zip file
    let temp_path = std::env::temp_dir().join(format!("list_{}.zip", id));
    let file = std::fs::File::create(&temp_path)
        .map_err(|e| {
            tracing::error!("Failed to create temp file: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to create zip file".to_string()
            })).into_response()
        })?;

    let mut zip = zip::ZipWriter::new(file);

    for item in &items {
        let options = zip::write::FileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);

        zip.start_file(&item.filename, options)
            .map_err(|e| {
                tracing::error!("Failed to add file to zip: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                    error: "Failed to create zip file".to_string()
                })).into_response()
            })?;

        let file_data = std::fs::read(&item.file_path)
            .map_err(|e| {
                tracing::error!("Failed to read file: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                    error: "Failed to read file".to_string()
                })).into_response()
            })?;

        zip.write_all(&file_data)
            .map_err(|e| {
                tracing::error!("Failed to write to zip: {}", e);
                (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                    error: "Failed to create zip file".to_string()
                })).into_response()
            })?;
    }

    zip.finish()
        .map_err(|e| {
            tracing::error!("Failed to finish zip: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to create zip file".to_string()
            })).into_response()
        })?;

    // Stream the zip file
    let file = File::open(&temp_path)
        .await
        .map_err(|e| {
            tracing::error!("Failed to open zip file: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to open zip file".to_string()
            })).into_response()
        })?;

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    // Clean up temp file after a delay (spawn a task)
    let temp_path_clone = temp_path.clone();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
        let _ = tokio::fs::remove_file(temp_path_clone).await;
    });

    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}.zip\"", list.name)),
        ],
        body
    ).into_response())
}

// =============================================================================
// LEGACY ENDPOINT - COMMENTED OUT
// Use POST /api/queue/list instead for non-blocking queue-based list search
// =============================================================================
// /// Search for and download a list of items
// pub async fn search_list(
//     State(state): State<AppState>,
//     Json(payload): Json<SearchListRequest>,
// ) -> Result<Json<List>, Response> {
//     // For now, assume user_id = 1 (will be extracted from JWT later)
//     let user_id = 1;
//
//     tracing::info!("=== LIST SEARCH REQUEST ===");
//     tracing::info!("List name: {:?}", payload.name);
//     tracing::info!("Query count: {}", payload.queries.len());
//     tracing::info!("Queries: {:?}", payload.queries);
//     tracing::info!("Format filter: {:?}", payload.format);
//     tracing::info!("User ID: {}", user_id);
//
//     let list = state.download_service.search_and_download_list(
//         payload.queries.clone(),
//         payload.name.clone(),
//         user_id,
//         payload.format.clone(),
//     )
//     .await
//     .map_err(|e| {
//         tracing::error!("=== LIST SEARCH FAILED ===");
//         tracing::error!("List name: {:?} - Error: {}", payload.name, e);
//         (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
//             error: e.to_string()
//         })).into_response()
//     })?;
//
//     tracing::info!("=== LIST SEARCH COMPLETED ===");
//     tracing::info!("List ID: {}", list.id);
//     tracing::info!("List name: {}", list.name);
//     tracing::info!("Status: {}", list.status);
//     tracing::info!("Completed: {}/{}", list.completed_items, list.total_items);
//     tracing::info!("Failed: {}", list.failed_items);
//
//     Ok(Json(list))
// }
