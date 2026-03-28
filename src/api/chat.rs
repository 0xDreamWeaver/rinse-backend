//! Chat API endpoints for Soulseek private messaging.

use axum::{
    extract::{State, Query},
    Json,
    response::{IntoResponse, Response},
    http::StatusCode,
};
use serde::{Deserialize, Serialize};

use super::{AppState, AuthUser, require_admin};

/// Query params for filtering chat messages
#[derive(Deserialize)]
pub struct ChatQuery {
    /// Filter to a specific username conversation
    pub username: Option<String>,
}

/// Response for chat message list
#[derive(Serialize)]
pub struct ChatMessageResponse {
    pub username: String,
    pub message: String,
    pub timestamp: u32,
    pub incoming: bool,
    pub is_new: bool,
}

/// Request to send a private message
#[derive(Deserialize)]
pub struct SendMessageRequest {
    pub username: String,
    pub message: String,
}

/// GET /api/chat/messages - Get recent chat history (admin only)
pub async fn get_messages(
    auth: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<ChatQuery>,
) -> Result<Json<Vec<ChatMessageResponse>>, Response> {
    require_admin(&auth)?;
    let history = state.slsk_client.router().get_chat_history().await;

    let messages: Vec<ChatMessageResponse> = history
        .into_iter()
        .filter(|msg| {
            if let Some(ref username) = query.username {
                msg.username.eq_ignore_ascii_case(username)
            } else {
                true
            }
        })
        .map(|msg| ChatMessageResponse {
            username: msg.username,
            message: msg.message,
            timestamp: msg.timestamp,
            incoming: msg.incoming,
            is_new: msg.is_new,
        })
        .collect();

    Ok(Json(messages))
}

/// POST /api/chat/send - Send a private message (admin only)
pub async fn send_message(
    auth: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<SendMessageRequest>,
) -> Result<Json<serde_json::Value>, Response> {
    require_admin(&auth)?;
    if req.username.is_empty() || req.message.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Username and message are required".to_string()).into_response());
    }

    state
        .slsk_client
        .router()
        .send_private_message(&req.username, &req.message)
        .await
        .map_err(|e| {
            (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to send message: {}", e)).into_response()
        })?;

    Ok(Json(serde_json::json!({
        "status": "sent",
        "username": req.username,
    })))
}
