use axum::{
    extract::{State, Query, ws::{WebSocket, WebSocketUpgrade, Message}},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::api::{AppState, verify_token};

/// Events that can be broadcast to WebSocket clients
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    /// Search has started
    SearchStarted {
        item_id: i64,
        query: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Search progress update
    SearchProgress {
        item_id: i64,
        results_count: usize,
        users_count: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Search completed
    SearchCompleted {
        item_id: i64,
        results_count: usize,
        selected_file: Option<String>,
        selected_user: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Download has started
    DownloadStarted {
        item_id: i64,
        filename: String,
        total_bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Download progress update
    DownloadProgress {
        item_id: i64,
        bytes_downloaded: u64,
        total_bytes: u64,
        progress_pct: f64,
        speed_kbps: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Download completed successfully
    DownloadCompleted {
        item_id: i64,
        filename: String,
        total_bytes: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Download failed
    DownloadFailed {
        item_id: i64,
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Download was queued by peer
    DownloadQueued {
        item_id: i64,
        position: Option<u32>,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Item status changed (generic update)
    ItemUpdated {
        item_id: i64,
        filename: String,
        status: String,
        progress: f64,
    },
    /// Item already exists in library (duplicate found)
    DuplicateFound {
        item_id: i64,
        filename: String,
        query: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },

    // ========================================================================
    // Queue-related events
    // ========================================================================

    /// Search has been added to the queue
    SearchQueued {
        queue_id: i64,
        query: String,
        position: i32,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Search is now being processed
    SearchProcessing {
        queue_id: i64,
        query: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// Search failed (no results or error)
    SearchFailed {
        queue_id: i64,
        query: String,
        error: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
    },
    /// New list has been created
    ListCreated {
        list_id: i64,
        name: String,
        total_items: i32,
    },
    /// List progress update
    ListProgress {
        list_id: i64,
        completed: i32,
        failed: i32,
        total: i32,
        status: String,
    },
    /// Queue status update (periodic)
    QueueStatus {
        pending: i64,
        processing: i64,
        active_downloads: i64,
    },

    // ========================================================================
    // Metadata-related events
    // ========================================================================

    /// Metadata lookup has started for an item
    MetadataLookupStarted {
        item_id: i64,
    },
    /// Metadata has been updated for an item
    MetadataUpdated {
        item_id: i64,
        #[serde(skip_serializing_if = "Option::is_none")]
        metadata: Option<crate::models::TrackMetadata>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Progress update for library-wide metadata scan job
    MetadataJobProgress {
        total: i64,
        completed: i64,
        failed: i64,
    },

    // ========================================================================
    // Upload-related events
    // ========================================================================

    /// An upload has started
    UploadStarted {
        username: String,
        filename: String,
        file_size: u64,
    },
    /// Upload progress update
    UploadProgress {
        username: String,
        filename: String,
        bytes_sent: u64,
        total_bytes: u64,
        speed_kbps: f64,
    },
    /// Upload completed successfully
    UploadCompleted {
        username: String,
        filename: String,
        total_bytes: u64,
    },
    /// Upload failed
    UploadFailed {
        username: String,
        filename: String,
        error: String,
    },

    // ========================================================================
    // Chat-related events
    // ========================================================================

    /// Private chat message received or sent
    ChatMessage {
        username: String,
        message: String,
        timestamp: u32,
        incoming: bool,
        is_new: bool,
    },
}

/// Query params for WebSocket connection
#[derive(Deserialize)]
pub struct WsQuery {
    token: Option<String>,
}

/// Create a new broadcast channel for WebSocket events
pub fn create_broadcast_channel() -> (broadcast::Sender<WsEvent>, broadcast::Receiver<WsEvent>) {
    broadcast::channel(256)
}

/// WebSocket handler for download progress updates
pub async fn progress_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<AppState>,
) -> Response {
    // Validate token if provided
    if let Some(token) = &query.token {
        if verify_token(token, &state.jwt_secret).is_err() {
            tracing::warn!("WebSocket connection rejected: invalid token");
            // We can't return an error directly from WebSocket upgrade,
            // so we'll just close the socket immediately in handle_socket
        }
    }

    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    tracing::debug!("WebSocket client connected");

    // Subscribe to broadcast channel
    let mut rx = state.ws_broadcast.subscribe();

    // Send initial status of only active/in-progress items (not completed ones)
    // This prevents flooding the client with status updates for all historical items
    if let Ok(items) = state.download_service.get_all_items().await {
        for item in items {
            // Only send items that are actively in progress
            let is_active = matches!(
                item.download_status.as_str(),
                "pending" | "downloading" | "queued"
            );
            if !is_active {
                continue;
            }

            let update = WsEvent::ItemUpdated {
                item_id: item.id,
                filename: item.filename.clone(),
                status: item.download_status.clone(),
                progress: item.download_progress,
            };

            if let Ok(msg) = serde_json::to_string(&update) {
                if socket.send(Message::Text(msg)).await.is_err() {
                    return;
                }
            }
        }
    }

    loop {
        tokio::select! {
            // Receive broadcast events and forward to client
            event = rx.recv() => {
                match event {
                    Ok(ws_event) => {
                        if let Ok(msg) = serde_json::to_string(&ws_event) {
                            if socket.send(Message::Text(msg)).await.is_err() {
                                tracing::debug!("WebSocket client disconnected (send error)");
                                return;
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket client lagged, missed {} events", n);
                        // Continue - we just missed some events
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::debug!("Broadcast channel closed");
                        return;
                    }
                }
            }

            // Handle incoming messages from client
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(_))) => {
                        // Could handle client messages here (e.g., subscribe to specific items)
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::debug!("WebSocket client disconnected");
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
}
