mod attribution;
mod auth;
mod items;
mod lists;
mod metadata;
mod queue;
mod ws;

pub use auth::{AuthUser, Claims, ErrorResponse, verify_token};
pub use items::*;
pub use lists::*;
pub use metadata::*;
pub use queue::*;
pub use ws::{progress_handler, WsEvent, create_broadcast_channel};

use axum::{
    Router,
    routing::{get, post, delete, put},
};
use tower_http::cors::CorsLayer;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::db::Database;
use crate::services::{DownloadService, EmailService, QueueService, MetadataService};

/// Application state
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub download_service: Arc<DownloadService>,
    pub queue_service: Arc<QueueService>,
    pub metadata_service: Arc<MetadataService>,
    pub jwt_secret: String,
    pub email_service: Option<EmailService>,
    pub ws_broadcast: broadcast::Sender<WsEvent>,
}

/// Create the API router
pub fn create_router(state: AppState) -> Router {
    Router::new()
        // Public authentication routes (no auth required)
        .route("/api/auth/login", post(auth::login))
        .route("/api/auth/register", post(auth::register))
        .route("/api/auth/verify-email", post(auth::verify_email))
        .route("/api/auth/resend-verification", post(auth::resend_verification))

        // Protected authentication routes (auth required)
        .route("/api/auth/me", get(auth::me))

        // Item routes (auth required - AuthUser extractor handles this)
        .route("/api/items", get(items::list_items))
        .route("/api/items/:id", get(items::get_item))
        .route("/api/items/:id", delete(items::delete_item))
        .route("/api/items", delete(items::batch_delete_items))
        .route("/api/items/:id/download", get(items::download_item))
        // LEGACY: Commented out - use POST /api/queue/search instead
        // .route("/api/items/search", post(items::search_item))

        // List routes (auth required - AuthUser extractor handles this)
        .route("/api/lists", get(lists::list_lists))
        .route("/api/lists/:id", get(lists::get_list))
        .route("/api/lists/:id", delete(lists::delete_list))
        .route("/api/lists/:id", put(lists::rename_list))
        .route("/api/lists/:id/with-items", delete(lists::delete_list_with_items))
        .route("/api/lists", delete(lists::batch_delete_lists))
        .route("/api/lists/:id/download", get(lists::download_list))
        .route("/api/lists/:list_id/items/:item_id", delete(lists::remove_item_from_list))
        .route("/api/lists/:id/items", delete(lists::batch_remove_items_from_list))
        // LEGACY: Commented out - use POST /api/queue/list instead
        // .route("/api/lists/search", post(lists::search_list))

        // Queue routes (auth required - AuthUser extractor handles this)
        .route("/api/queue", get(queue::get_queue_status))
        .route("/api/queue/items", get(queue::get_queue_items))
        .route("/api/queue/search", post(queue::enqueue_search))
        .route("/api/queue/list", post(queue::enqueue_list))
        .route("/api/queue/:id", delete(queue::cancel_search))

        // Metadata routes (auth required - AuthUser extractor handles this)
        .route("/api/items/:id/metadata", get(metadata::get_item_metadata))
        .route("/api/items/:id/metadata", delete(metadata::clear_item_metadata))
        .route("/api/items/:id/metadata/refresh", post(metadata::refresh_item_metadata))
        .route("/api/metadata/job", post(metadata::start_metadata_job))
        .route("/api/metadata/job", get(metadata::get_job_status))

        // WebSocket for download progress
        .route("/api/ws/progress", get(ws::progress_handler))

        // Health check (public)
        .route("/health", get(|| async { "OK" }))

        // Attribution pages (public - for API key verification)
        .route("/getsongbpm", get(attribution::getsongbpm_attribution))

        .layer(CorsLayer::permissive())
        .with_state(state)
}
