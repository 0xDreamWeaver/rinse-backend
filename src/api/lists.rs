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

use crate::api::AppState;
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

/// List all lists for a user
pub async fn list_lists(
    State(state): State<AppState>,
) -> Result<Json<Vec<List>>, Response> {
    // For now, assume user_id = 1 (will be extracted from JWT later)
    let user_id = 1;

    let lists = state.download_service.get_user_lists(user_id)
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
    state.download_service.delete_list(id)
        .await
        .map_err(|e| {
            tracing::error!("Failed to delete list: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to delete list".to_string()
            })).into_response()
        })?;

    Ok(StatusCode::NO_CONTENT)
}

/// Batch delete lists
pub async fn batch_delete_lists(
    State(state): State<AppState>,
    Json(payload): Json<BatchDeleteRequest>,
) -> Result<StatusCode, Response> {
    state.download_service.delete_lists(payload.ids)
        .await
        .map_err(|e| {
            tracing::error!("Failed to batch delete lists: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to batch delete lists".to_string()
            })).into_response()
        })?;

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

/// Search for and download a list of items
pub async fn search_list(
    State(state): State<AppState>,
    Json(payload): Json<SearchListRequest>,
) -> Result<Json<List>, Response> {
    // For now, assume user_id = 1 (will be extracted from JWT later)
    let user_id = 1;

    let list = state.download_service.search_and_download_list(
        payload.queries,
        payload.name,
        user_id,
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to search and download list: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
            error: e.to_string()
        })).into_response()
    })?;

    Ok(Json(list))
}
