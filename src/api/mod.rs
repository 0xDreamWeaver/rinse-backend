mod auth;
mod items;
mod lists;
mod ws;

pub use auth::{AuthUser, Claims, ErrorResponse, verify_token};
pub use items::*;
pub use lists::*;
pub use ws::*;

use axum::{
    Router,
    routing::{get, post, delete},
};
use tower_http::cors::CorsLayer;
use std::sync::Arc;

use crate::db::Database;
use crate::services::{DownloadService, EmailService};

/// Application state
#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub download_service: Arc<DownloadService>,
    pub jwt_secret: String,
    pub email_service: Option<EmailService>,
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
        .route("/api/items/search", post(items::search_item))

        // List routes (auth required - AuthUser extractor handles this)
        .route("/api/lists", get(lists::list_lists))
        .route("/api/lists/:id", get(lists::get_list))
        .route("/api/lists/:id", delete(lists::delete_list))
        .route("/api/lists", delete(lists::batch_delete_lists))
        .route("/api/lists/:id/download", get(lists::download_list))
        .route("/api/lists/search", post(lists::search_list))

        // WebSocket for download progress
        .route("/api/ws/progress", get(ws::progress_handler))

        // Health check (public)
        .route("/health", get(|| async { "OK" }))

        .layer(CorsLayer::permissive())
        .with_state(state)
}
