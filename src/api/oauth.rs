//! OAuth API endpoints for external service connections

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::{AppState, AuthUser};
use crate::models::{
    ExternalPlaylistResponse, OAuthCallbackRequest, OAuthCallbackResponse,
    OAuthConnectResponse, OAuthConnectionStatus, PlaylistsResponse,
};
use crate::services::oauth::MusicService;

/// Query parameters for playlists endpoint
#[derive(Debug, Deserialize)]
pub struct PlaylistsQuery {
    pub limit: Option<i32>,
    pub offset: Option<i32>,
}

/// Get OAuth connection status for a service
pub async fn get_connection_status(
    State(state): State<AppState>,
    Path(service): Path<String>,
    user: AuthUser,
) -> Result<Json<OAuthConnectionStatus>, (StatusCode, String)> {
    let service = MusicService::from_str(&service).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("Unknown service: {}", service),
        )
    })?;

    // Check if service is configured
    let oauth_service = state.oauth_service.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth service not available".to_string(),
        )
    })?;

    if !oauth_service.is_service_configured(service) {
        return Ok(Json(OAuthConnectionStatus {
            service: service.to_string(),
            connected: false,
            username: None,
            connected_at: None,
            last_used_at: None,
        }));
    }

    // Get connection status from database
    match oauth_service
        .get_connection_status(user.id, service)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    {
        Some(conn) => Ok(Json(conn.into())),
        None => Ok(Json(OAuthConnectionStatus {
            service: service.to_string(),
            connected: false,
            username: None,
            connected_at: None,
            last_used_at: None,
        })),
    }
}

/// Get all OAuth connection statuses for the user
pub async fn get_all_connection_statuses(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Vec<OAuthConnectionStatus>>, (StatusCode, String)> {
    let oauth_service = state.oauth_service.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth service not available".to_string(),
        )
    })?;

    let connections = state
        .db
        .get_user_oauth_connections(user.id)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let statuses: Vec<OAuthConnectionStatus> = connections.into_iter().map(|c| c.into()).collect();

    Ok(Json(statuses))
}

/// Start OAuth authorization flow - returns redirect URL
pub async fn start_oauth(
    State(state): State<AppState>,
    Path(service): Path<String>,
    user: AuthUser,
) -> Result<Json<OAuthConnectResponse>, (StatusCode, String)> {
    let service = MusicService::from_str(&service).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("Unknown service: {}", service),
        )
    })?;

    let oauth_service = state.oauth_service.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth service not available".to_string(),
        )
    })?;

    if !oauth_service.is_service_configured(service) {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            format!("{} integration is not configured", service),
        ));
    }

    let auth_url = oauth_service
        .start_authorization(user.id, service)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Note: We don't return the state to the client - it's stored server-side
    // The client just redirects to the auth_url
    Ok(Json(OAuthConnectResponse {
        auth_url,
        state: String::new(), // Not needed by client
    }))
}

/// Complete OAuth authorization - exchange code for tokens
pub async fn oauth_callback(
    State(state): State<AppState>,
    Path(service): Path<String>,
    user: AuthUser,
    Json(payload): Json<OAuthCallbackRequest>,
) -> Result<Json<OAuthCallbackResponse>, (StatusCode, String)> {
    let service = MusicService::from_str(&service).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("Unknown service: {}", service),
        )
    })?;

    let oauth_service = state.oauth_service.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth service not available".to_string(),
        )
    })?;

    let profile = oauth_service
        .complete_authorization(user.id, service, &payload.code, &payload.state)
        .await
        .map_err(|e| {
            tracing::error!("[OAuth] Callback failed for {}: {}", service, e);
            (StatusCode::BAD_REQUEST, e.to_string())
        })?;

    Ok(Json(OAuthCallbackResponse {
        connected: true,
        service: service.to_string(),
        username: profile.username,
    }))
}

/// Disconnect OAuth service
pub async fn disconnect_oauth(
    State(state): State<AppState>,
    Path(service): Path<String>,
    user: AuthUser,
) -> Result<StatusCode, (StatusCode, String)> {
    let service = MusicService::from_str(&service).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("Unknown service: {}", service),
        )
    })?;

    let oauth_service = state.oauth_service.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth service not available".to_string(),
        )
    })?;

    oauth_service
        .disconnect(user.id, service)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

/// Get user's playlists from a connected service
pub async fn get_playlists(
    State(state): State<AppState>,
    Path(service): Path<String>,
    Query(query): Query<PlaylistsQuery>,
    user: AuthUser,
) -> Result<Json<PlaylistsResponse>, (StatusCode, String)> {
    let service = MusicService::from_str(&service).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("Unknown service: {}", service),
        )
    })?;

    let oauth_service = state.oauth_service.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "OAuth service not available".to_string(),
        )
    })?;

    let limit = query.limit.unwrap_or(50).min(50);
    let offset = query.offset.unwrap_or(0);

    let (playlists, total) = oauth_service
        .get_playlists(user.id, service, limit, offset)
        .await
        .map_err(|e| {
            tracing::error!("[OAuth] Failed to get playlists from {}: {}", service, e);
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?;

    let playlist_responses: Vec<ExternalPlaylistResponse> = playlists
        .into_iter()
        .map(|p| ExternalPlaylistResponse {
            id: p.id,
            name: p.name,
            description: p.description,
            owner_name: p.owner_name,
            track_count: p.track_count,
            image_url: p.image_url,
            external_url: p.external_url,
            is_public: p.is_public,
        })
        .collect();

    Ok(Json(PlaylistsResponse {
        playlists: playlist_responses,
        total,
        limit,
        offset,
    }))
}
