//! Direct message API endpoints.

use axum::{
    extract::{State, Path, Query},
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;

use super::{AppState, AuthUser};
use crate::api::ws::WsEvent;

/// Query params for DM thread pagination
#[derive(Deserialize)]
pub struct DmThreadQuery {
    pub limit: Option<i64>,
    pub before_id: Option<i64>,
}

/// Request to send a direct message
#[derive(Deserialize)]
pub struct SendDmRequest {
    pub message: String,
}

/// Query params for user search
#[derive(Deserialize)]
pub struct UserSearchQuery {
    pub q: String,
}

/// GET /api/direct-messages — list conversations for authenticated user
pub async fn get_conversations(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<crate::models::DmConversationSummary>>, Response> {
    let conversations = state
        .db
        .get_dm_conversations(auth.id)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to get conversations: {}", e)).into_response()
        })?;

    Ok(Json(conversations))
}

/// GET /api/direct-messages/:user_id — get thread with a specific user
pub async fn get_thread(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<i64>,
    Query(query): Query<DmThreadQuery>,
) -> Result<Json<Vec<crate::models::DirectMessageRow>>, Response> {
    let limit = query.limit.unwrap_or(50).min(100);
    let messages = state
        .db
        .get_dm_thread(auth.id, user_id, limit, query.before_id)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to get thread: {}", e)).into_response()
        })?;

    Ok(Json(messages))
}

/// POST /api/direct-messages/:user_id — send a DM to a user
pub async fn send_message(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(recipient_id): Path<i64>,
    Json(req): Json<SendDmRequest>,
) -> Result<Json<crate::models::DirectMessageRow>, Response> {
    let message = req.message.trim().to_string();

    if message.is_empty() || message.len() > 2000 {
        return Err((
            StatusCode::BAD_REQUEST,
            "Message must be between 1 and 2000 characters".to_string(),
        ).into_response());
    }

    if auth.id == recipient_id {
        return Err((
            StatusCode::BAD_REQUEST,
            "Cannot send a message to yourself".to_string(),
        ).into_response());
    }

    // Verify recipient exists
    let recipient = state.db.get_user_by_id(recipient_id).await.map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to look up recipient: {}", e)).into_response()
    })?;
    if recipient.is_none() {
        return Err((StatusCode::NOT_FOUND, "Recipient not found".to_string()).into_response());
    }

    // Insert message
    let msg_id = state
        .db
        .insert_direct_message(auth.id, recipient_id, &message)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to send message: {}", e)).into_response()
        })?;

    // Fetch back with user info
    let dm = state
        .db
        .get_direct_message(msg_id)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to fetch message: {}", e)).into_response()
        })?
        .ok_or_else(|| {
            (StatusCode::INTERNAL_SERVER_ERROR, "Message not found after insert".to_string()).into_response()
        })?;

    // Broadcast via WebSocket
    let _ = state.ws_broadcast.send(WsEvent::DirectMessage {
        id: dm.id,
        sender_id: dm.sender_id,
        sender_username: dm.sender_username.clone(),
        sender_display_name: dm.sender_display_name.clone(),
        sender_has_avatar: dm.sender_has_avatar,
        recipient_id: dm.recipient_id,
        message: dm.message.clone(),
        created_at: dm.created_at.to_rfc3339(),
    });

    Ok(Json(dm))
}

/// PUT /api/direct-messages/:user_id/read — mark conversation as read
pub async fn mark_read(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(sender_id): Path<i64>,
) -> Result<Json<crate::models::MessageResponse>, Response> {
    state
        .db
        .mark_dms_read(auth.id, sender_id)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to mark as read: {}", e)).into_response()
        })?;

    Ok(Json(crate::models::MessageResponse {
        message: "Marked as read".to_string(),
    }))
}

/// GET /api/users/search?q= — search users for DM picker
pub async fn search_users(
    auth: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<UserSearchQuery>,
) -> Result<Json<Vec<crate::models::UserSummary>>, Response> {
    let q = query.q.trim();
    if q.is_empty() {
        return Ok(Json(vec![]));
    }

    let users = state
        .db
        .search_users(q, auth.id, 10)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to search users: {}", e)).into_response()
        })?;

    Ok(Json(users))
}
