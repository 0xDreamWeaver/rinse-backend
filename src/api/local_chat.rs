//! Local chat (shoutbox) API endpoints.

use axum::{
    extract::{State, Query},
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::{AppState, AuthUser};
use crate::api::ws::WsEvent;

/// Query params for local chat messages
#[derive(Deserialize)]
pub struct LocalChatQuery {
    /// Max messages to return (default 50)
    pub limit: Option<i64>,
    /// Return messages before this ID (cursor pagination)
    pub before_id: Option<i64>,
}

/// Request to send a local chat message
#[derive(Deserialize)]
pub struct SendLocalChatRequest {
    pub message: String,
}

/// GET /api/local-chat/messages
pub async fn get_messages(
    _auth: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<LocalChatQuery>,
) -> Result<Json<Vec<crate::models::LocalChatRow>>, Response> {
    let limit = query.limit.unwrap_or(50).min(100);
    let messages = state
        .db
        .get_local_chat_messages(limit, query.before_id)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to get messages: {}", e)).into_response()
        })?;

    Ok(Json(messages))
}

/// POST /api/local-chat/send
pub async fn send_message(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<SendLocalChatRequest>,
) -> Result<Json<crate::models::LocalChatRow>, Response> {
    let message = req.message.trim().to_string();

    if message.is_empty() || message.len() > 2000 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Message must be between 1 and 2000 characters".to_string(),
        ).into_response());
    }

    // Insert message
    let msg_id = state
        .db
        .insert_local_chat_message(auth.id, &message)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to send message: {}", e)).into_response()
        })?;

    // Fetch back with user info
    let chat_msg = state
        .db
        .get_local_chat_message(msg_id)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to fetch message: {}", e)).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::INTERNAL_SERVER_ERROR, "Message not found after insert".to_string()).into_response()
        })?;

    // Broadcast via WebSocket
    let _ = state.ws_broadcast.send(WsEvent::LocalChatMessage {
        id: chat_msg.id,
        user_id: chat_msg.user_id,
        username: chat_msg.username.clone(),
        display_name: chat_msg.display_name.clone(),
        has_avatar: chat_msg.has_avatar,
        message: chat_msg.message.clone(),
        created_at: chat_msg.created_at.to_rfc3339(),
    });

    Ok(Json(chat_msg))
}

/// Query params for share search
#[derive(Deserialize)]
pub struct ShareSearchQuery {
    pub q: String,
}

/// GET /api/local-chat/search-items
pub async fn search_items(
    _auth: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<ShareSearchQuery>,
) -> Result<Json<Vec<crate::models::ShareSearchRow>>, Response> {
    let q = query.q.trim();
    if q.is_empty() {
        return Ok(Json(vec![]));
    }

    let items = state
        .db
        .search_items_for_share(q, 10)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to search items: {}", e)).into_response()
        })?;

    Ok(Json(items))
}

/// GET /api/local-chat/search-lists
pub async fn search_lists(
    auth: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<ShareSearchQuery>,
) -> Result<Json<Vec<crate::models::List>>, Response> {
    let q = query.q.trim();
    if q.is_empty() {
        return Ok(Json(vec![]));
    }

    let lists = state
        .db
        .search_user_lists(auth.id, q, 10)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to search lists: {}", e)).into_response()
        })?;

    Ok(Json(lists))
}
