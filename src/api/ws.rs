use axum::{
    extract::{State, ws::{WebSocket, WebSocketUpgrade, Message}},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;

use crate::api::AppState;
use crate::models::Item;

#[derive(Serialize)]
struct ProgressUpdate {
    item_id: i64,
    filename: String,
    status: String,
    progress: f64,
}

/// WebSocket handler for download progress updates
pub async fn progress_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    tracing::info!("WebSocket client connected");

    // Send initial status of all items
    if let Ok(items) = state.download_service.get_all_items().await {
        for item in items {
            let update = ProgressUpdate {
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

    // Poll for updates every 500ms
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));

    loop {
        tokio::select! {
            _ = interval.tick() => {
                // Get current items and send updates
                match state.download_service.get_all_items().await {
                    Ok(items) => {
                        for item in items {
                            // Only send if downloading or recently updated
                            if item.download_status == "downloading" || item.download_status == "pending" {
                                let update = ProgressUpdate {
                                    item_id: item.id,
                                    filename: item.filename.clone(),
                                    status: item.download_status.clone(),
                                    progress: item.download_progress,
                                };

                                if let Ok(msg) = serde_json::to_string(&update) {
                                    if socket.send(Message::Text(msg)).await.is_err() {
                                        tracing::info!("WebSocket client disconnected");
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to get items: {}", e);
                    }
                }
            }

            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(_))) => {
                        // Echo or handle text messages if needed
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        tracing::info!("WebSocket client disconnected");
                        return;
                    }
                    _ => {}
                }
            }
        }
    }
}
