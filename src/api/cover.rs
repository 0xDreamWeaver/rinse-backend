//! Cover art serving endpoint
//!
//! Serves cached cover art images as WebP.
//! No auth required — cover art is publicly available album artwork.

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    body::Body,
    Json,
};
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;

use crate::api::AppState;
use crate::services::metadata::cover_cache::CoverSize;

#[derive(Deserialize)]
pub struct CoverQuery {
    #[serde(default = "default_size")]
    size: String,
}

fn default_size() -> String {
    "thumb".to_string()
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// GET /api/items/:id/cover?size=thumb|full
///
/// Serves cached WebP cover art. Returns 404 if not cached.
pub async fn get_item_cover(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<CoverQuery>,
) -> Result<Response, Response> {
    let size = CoverSize::from_str(&query.size);
    let path = state
        .metadata_service
        .cover_cache()
        .cover_path(id, size);

    let file = tokio::fs::File::open(&path).await.map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "Cover art not cached".to_string(),
            }),
        )
            .into_response()
    })?;

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/webp")
        .header(header::CACHE_CONTROL, "public, max-age=604800")
        .body(body)
        .unwrap())
}
