mod admin;
mod attribution;
mod auth;
mod chat;
mod cover;
mod items;
mod lists;
mod metadata;
mod oauth;
mod profile;
mod queue;
mod uploads;
mod ws;

pub use auth::{AuthUser, Claims, ErrorResponse, verify_token, require_admin};
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
use crate::protocol::SoulseekClient;
use crate::services::{DownloadService, EmailService, QueueService, MetadataService, OAuthService, SharingService, UploadService};

/// Application state
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub download_service: Arc<DownloadService>,
    pub queue_service: Arc<QueueService>,
    pub metadata_service: Arc<MetadataService>,
    pub oauth_service: Option<Arc<OAuthService>>,
    pub sharing_service: Arc<SharingService>,
    pub upload_service: Arc<UploadService>,
    pub slsk_client: Arc<SoulseekClient>,
    pub jwt_secret: String,
    pub email_service: Option<EmailService>,
    pub ws_broadcast: broadcast::Sender<WsEvent>,
    pub storage_path: String,
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

        // Profile routes (auth required - AuthUser extractor handles this)
        .route("/api/profile", put(profile::update_profile))
        .route("/api/profile/avatar", post(profile::upload_avatar).delete(profile::delete_avatar).get(profile::get_avatar))

        // Item routes (auth required - AuthUser extractor handles this)
        .route("/api/items", get(items::list_items))
        .route("/api/items/:id", get(items::get_item))
        .route("/api/items/:id", delete(items::delete_item))
        .route("/api/items", delete(items::batch_delete_items))
        .route("/api/items/:id/download", get(items::download_item))
        .route("/api/items/:id/cover", get(cover::get_item_cover))
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
        .route("/api/queue/history", get(queue::get_search_history))
        .route("/api/queue/search", post(queue::enqueue_search))
        .route("/api/queue/list", post(queue::enqueue_list))
        .route("/api/queue/:id", delete(queue::cancel_search))

        // Metadata routes (auth required - AuthUser extractor handles this)
        .route("/api/items/:id/metadata", get(metadata::get_item_metadata))
        .route("/api/items/:id/metadata", delete(metadata::clear_item_metadata))
        .route("/api/items/:id/metadata/refresh", post(metadata::refresh_item_metadata))
        .route("/api/metadata/job", post(metadata::start_metadata_job))
        .route("/api/metadata/job", get(metadata::get_job_status))
        .route("/api/covers/backfill", post(metadata::start_cover_backfill).get(metadata::get_cover_backfill_status))

        // Upload/sharing routes (auth required - AuthUser extractor handles this)
        .route("/api/uploads", get(uploads::get_upload_status))
        .route("/api/uploads/config", get(uploads::get_upload_config).put(uploads::update_upload_config))
        .route("/api/uploads/active", get(uploads::get_active_uploads))
        .route("/api/sharing/stats", get(uploads::get_sharing_stats))
        .route("/api/sharing/rescan", post(uploads::rescan_shares))

        // Chat routes (auth required - AuthUser extractor handles this)
        .route("/api/chat/messages", get(chat::get_messages))
        .route("/api/chat/send", post(chat::send_message))

        // WebSocket for download progress
        .route("/api/ws/progress", get(ws::progress_handler))

        // OAuth routes (auth required - AuthUser extractor handles this)
        .route("/api/oauth/connections", get(oauth::get_all_connection_statuses))
        .route("/api/oauth/:service/status", get(oauth::get_connection_status))
        .route("/api/oauth/:service/connect", get(oauth::start_oauth))
        .route("/api/oauth/:service/callback", post(oauth::oauth_callback))
        .route("/api/oauth/:service/disconnect", delete(oauth::disconnect_oauth))
        .route("/api/oauth/:service/playlists", get(oauth::get_playlists))
        .route("/api/oauth/:service/playlists/:playlist_id/tracks", get(oauth::get_playlist_tracks))

        // Admin routes (auth + admin role required)
        .route("/api/admin/stats", get(admin::get_stats))
        .route("/api/admin/users", get(admin::get_users))
        .route("/api/admin/users/:id/role", put(admin::update_user_role))

        // Health check (public)
        .route("/health", get(|| async { "OK" }))

        // Attribution pages (public - for API key verification)
        .route("/getsongbpm", get(attribution::getsongbpm_attribution))

        .layer(CorsLayer::permissive())
        .with_state(state)
}
