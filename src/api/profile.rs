use axum::{
    extract::{Multipart, Query, State},
    http::{StatusCode, header},
    Json,
    response::{IntoResponse, Response},
    body::Body,
};
use serde::{Deserialize, Serialize};
use tokio_util::io::ReaderStream;

use crate::api::{AppState, AuthUser, verify_token};
use crate::models::{UpdateProfileRequest, UserResponse};

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Query params for avatar GET (uses token query param since <img> can't set headers)
#[derive(Deserialize)]
pub struct AvatarQuery {
    token: String,
}

/// PUT /api/profile — Update display_name and bio
pub async fn update_profile(
    State(state): State<AppState>,
    auth_user: AuthUser,
    Json(payload): Json<UpdateProfileRequest>,
) -> Result<Json<UserResponse>, Response> {
    // Trim and validate display_name
    let display_name = payload.display_name
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(ref name) = display_name {
        if name.len() > 50 {
            return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
                error: "Display name must be 50 characters or fewer".to_string()
            })).into_response());
        }
    }

    // Trim and validate bio
    let bio = payload.bio
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let Some(ref b) = bio {
        if b.len() > 500 {
            return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
                error: "Bio must be 500 characters or fewer".to_string()
            })).into_response());
        }
    }

    let user = state.db.update_user_profile(
        auth_user.id,
        display_name.as_deref(),
        bio.as_deref(),
    )
    .await
    .map_err(|e| {
        tracing::error!("Failed to update profile: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
            error: "Failed to update profile".to_string()
        })).into_response()
    })?;

    tracing::info!(
        "[Profile] User '{}' (id={}) updated profile: display_name={}, bio={}",
        auth_user.username,
        auth_user.id,
        display_name.as_deref().unwrap_or("<cleared>"),
        if bio.is_some() { "set" } else { "cleared" },
    );

    Ok(Json(UserResponse {
        id: user.id,
        username: user.username,
        email: user.email.unwrap_or_default(),
        email_verified: user.email_verified,
        display_name: user.display_name,
        bio: user.bio,
        has_avatar: user.avatar_path.is_some(),
        role: user.role,
        created_at: user.created_at,
    }))
}

/// POST /api/profile/avatar — Upload avatar (multipart, max 2MB)
pub async fn upload_avatar(
    State(state): State<AppState>,
    auth_user: AuthUser,
    mut multipart: Multipart,
) -> Result<Json<UserResponse>, Response> {
    // Read the first field from the multipart form
    let field = multipart.next_field().await
        .map_err(|e| {
            tracing::error!("Failed to read multipart field: {}", e);
            (StatusCode::BAD_REQUEST, Json(ErrorResponse {
                error: "Failed to read upload".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::BAD_REQUEST, Json(ErrorResponse {
                error: "No file provided".to_string()
            })).into_response()
        })?;

    // Validate content type
    let content_type = field.content_type()
        .unwrap_or("application/octet-stream")
        .to_string();

    if !["image/jpeg", "image/png", "image/webp"].contains(&content_type.as_str()) {
        return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
            error: "Invalid file type. Accepted: JPEG, PNG, WebP".to_string()
        })).into_response());
    }

    // Read file bytes with 2MB limit
    let data = field.bytes().await
        .map_err(|e| {
            tracing::error!("Failed to read file bytes: {}", e);
            (StatusCode::BAD_REQUEST, Json(ErrorResponse {
                error: "Failed to read file".to_string()
            })).into_response()
        })?;

    if data.len() > 2 * 1024 * 1024 {
        return Err((StatusCode::BAD_REQUEST, Json(ErrorResponse {
            error: "File too large. Maximum size is 2MB".to_string()
        })).into_response());
    }

    let data_len = data.len();

    // Process image in a blocking thread (decode, resize, encode as WebP)
    let processed = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, String> {
        let img = image::load_from_memory(&data)
            .map_err(|e| format!("Failed to decode image: {}", e))?;

        // Resize to fit within 512x512, preserving aspect ratio
        let resized = img.resize(512, 512, image::imageops::FilterType::Lanczos3);

        // Encode as WebP
        let mut buf = std::io::Cursor::new(Vec::new());
        resized.write_to(&mut buf, image::ImageFormat::WebP)
            .map_err(|e| format!("Failed to encode image: {}", e))?;

        Ok(buf.into_inner())
    })
    .await
    .map_err(|e| {
        tracing::error!("Image processing task failed: {}", e);
        (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
            error: "Failed to process image".to_string()
        })).into_response()
    })?
    .map_err(|e| {
        tracing::error!("Image processing error: {}", e);
        (StatusCode::BAD_REQUEST, Json(ErrorResponse {
            error: e
        })).into_response()
    })?;

    // Save to disk
    let avatar_filename = format!("{}.webp", auth_user.id);
    let avatar_path = format!("{}/avatars/{}", state.storage_path, avatar_filename);

    tokio::fs::write(&avatar_path, &processed).await
        .map_err(|e| {
            tracing::error!("Failed to write avatar file: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to save avatar".to_string()
            })).into_response()
        })?;

    tracing::info!(
        "[Profile] User '{}' (id={}) uploaded avatar: type={}, original_size={}KB, processed_size={}KB",
        auth_user.username,
        auth_user.id,
        content_type,
        data_len / 1024,
        processed.len() / 1024,
    );

    // Update DB
    let relative_path = format!("avatars/{}", avatar_filename);
    state.db.update_user_avatar(auth_user.id, Some(&relative_path)).await
        .map_err(|e| {
            tracing::error!("Failed to update avatar in DB: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to update avatar".to_string()
            })).into_response()
        })?;

    // Fetch updated user
    let user = state.db.get_user_by_id(auth_user.id).await
        .map_err(|e| {
            tracing::error!("Failed to fetch user: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Internal server error".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::NOT_FOUND, Json(ErrorResponse {
                error: "User not found".to_string()
            })).into_response()
        })?;

    Ok(Json(UserResponse {
        id: user.id,
        username: user.username,
        email: user.email.unwrap_or_default(),
        email_verified: user.email_verified,
        display_name: user.display_name,
        bio: user.bio,
        has_avatar: user.avatar_path.is_some(),
        role: user.role,
        created_at: user.created_at,
    }))
}

/// DELETE /api/profile/avatar — Remove avatar
pub async fn delete_avatar(
    State(state): State<AppState>,
    auth_user: AuthUser,
) -> Result<Json<UserResponse>, Response> {
    // Get user to find avatar path
    let user = state.db.get_user_by_id(auth_user.id).await
        .map_err(|e| {
            tracing::error!("Failed to fetch user: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Internal server error".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::NOT_FOUND, Json(ErrorResponse {
                error: "User not found".to_string()
            })).into_response()
        })?;

    // Delete file from disk if it exists
    if let Some(ref avatar_path) = user.avatar_path {
        let full_path = format!("{}/{}", state.storage_path, avatar_path);
        if let Err(e) = tokio::fs::remove_file(&full_path).await {
            // Log but don't fail — file might already be gone
            tracing::warn!("Failed to delete avatar file {}: {}", full_path, e);
        }
    }

    // Clear in DB
    state.db.update_user_avatar(auth_user.id, None).await
        .map_err(|e| {
            tracing::error!("Failed to clear avatar in DB: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Failed to remove avatar".to_string()
            })).into_response()
        })?;

    tracing::info!(
        "[Profile] User '{}' (id={}) removed avatar",
        auth_user.username,
        auth_user.id,
    );

    Ok(Json(UserResponse {
        id: user.id,
        username: user.username,
        email: user.email.unwrap_or_default(),
        email_verified: user.email_verified,
        display_name: user.display_name,
        bio: user.bio,
        has_avatar: false,
        role: user.role,
        created_at: user.created_at,
    }))
}

/// GET /api/profile/avatar?token=xxx — Serve avatar image
pub async fn get_avatar(
    State(state): State<AppState>,
    Query(query): Query<AvatarQuery>,
) -> Result<Response, Response> {
    // Validate token from query param (img src can't set Authorization header)
    let claims = verify_token(&query.token, &state.jwt_secret)
        .map_err(|_| {
            (StatusCode::UNAUTHORIZED, Json(ErrorResponse {
                error: "Invalid or expired token".to_string()
            })).into_response()
        })?;

    // Get user to find avatar path
    let user = state.db.get_user_by_id(claims.sub).await
        .map_err(|e| {
            tracing::error!("Failed to fetch user: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(ErrorResponse {
                error: "Internal server error".to_string()
            })).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::NOT_FOUND, Json(ErrorResponse {
                error: "User not found".to_string()
            })).into_response()
        })?;

    let avatar_path = user.avatar_path.ok_or_else(|| {
        (StatusCode::NOT_FOUND, Json(ErrorResponse {
            error: "No avatar set".to_string()
        })).into_response()
    })?;

    let full_path = format!("{}/{}", state.storage_path, avatar_path);

    let file = tokio::fs::File::open(&full_path).await
        .map_err(|_| {
            (StatusCode::NOT_FOUND, Json(ErrorResponse {
                error: "Avatar file not found".to_string()
            })).into_response()
        })?;

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "image/webp")
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(body)
        .unwrap())
}
