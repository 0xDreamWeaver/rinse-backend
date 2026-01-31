mod protocol;
mod db;
mod models;
mod api;
mod services;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

use db::Database;
use services::{DownloadService, EmailService};
use api::{AppState, create_router, create_broadcast_channel};

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if it exists (silently ignore if not found)
    let _ = dotenvy::dotenv();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .init();

    tracing::info!("Starting Rinse backend...");

    // Load configuration from environment or defaults
    let database_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "sqlite:data/rinse.db".to_string());
    let storage_path = std::env::var("STORAGE_PATH")
        .unwrap_or_else(|_| "storage".to_string());
    let jwt_secret = std::env::var("JWT_SECRET")
        .unwrap_or_else(|_| "change-me-in-production".to_string());
    let bind_addr = std::env::var("BIND_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:3000".to_string());

    // Soulseek credentials - default to RinseService
    let slsk_username = std::env::var("SLSK_USERNAME")
        .unwrap_or_else(|_| "RinseService".to_string());
    let slsk_password = std::env::var("SLSK_PASSWORD")
        .unwrap_or_else(|_| "rinse2024".to_string());

    // SMTP configuration (optional - for email verification)
    let email_service = if let (Ok(host), Ok(user), Ok(pass)) = (
        std::env::var("SMTP_HOST"),
        std::env::var("SMTP_USERNAME"),
        std::env::var("SMTP_PASSWORD"),
    ) {
        let port: u16 = std::env::var("SMTP_PORT")
            .unwrap_or_else(|_| "587".to_string())
            .parse()
            .unwrap_or(587);
        let from_email = std::env::var("SMTP_FROM_EMAIL")
            .unwrap_or_else(|_| "noreply@rinse.local".to_string());
        let from_name = std::env::var("SMTP_FROM_NAME")
            .unwrap_or_else(|_| "Rinse".to_string());
        let app_url = std::env::var("APP_URL")
            .unwrap_or_else(|_| "http://localhost:5173".to_string());

        tracing::info!("SMTP configured ({}:{}), email verification enabled", host, port);
        Some(EmailService::new(host, port, user, pass, from_email, from_name, app_url))
    } else {
        tracing::warn!("SMTP not configured - email verification will be disabled");
        tracing::warn!("Set SMTP_HOST, SMTP_USERNAME, SMTP_PASSWORD to enable email verification");
        None
    };

    // Ensure storage directory exists
    tokio::fs::create_dir_all(&storage_path).await?;

    // Initialize database
    tracing::info!("Connecting to database at {}", database_url);
    let db = Database::new(&database_url).await?;

    // Create default admin user if none exists (pre-verified, no email required)
    if db.get_user_by_username("admin").await?.is_none() {
        tracing::info!("Creating default admin user...");
        use argon2::{
            password_hash::{rand_core::OsRng, PasswordHasher, SaltString},
            Argon2
        };
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let password_hash = argon2.hash_password("admin123".as_bytes(), &salt)
            .map_err(|e| anyhow::anyhow!("Failed to hash password: {}", e))?
            .to_string();
        db.create_user_verified("admin", &password_hash).await?;
        tracing::info!("Default admin user created (username: admin, password: admin123)");
    }

    // Create WebSocket broadcast channel
    let (ws_broadcast, _ws_rx) = create_broadcast_channel();

    // Initialize download service with broadcast sender
    let download_service = Arc::new(DownloadService::new(
        db.clone(),
        PathBuf::from(&storage_path),
        ws_broadcast.clone(),
    ));

    // Initialize UPnP port forwarding for incoming peer connections
    tracing::info!("Initializing UPnP port forwarding...");
    match services::upnp::init_upnp().await {
        Ok(true) => tracing::info!("UPnP port forwarding enabled (ports 2234, 2235)"),
        Ok(false) => {
            tracing::warn!("UPnP not available - peers may not be able to connect to us");
            tracing::warn!("Consider manually forwarding port 2234 on your router");
        }
        Err(e) => {
            tracing::warn!("UPnP initialization failed: {} - continuing without port forwarding", e);
            tracing::warn!("Peers may not be able to connect for file transfers");
        }
    }

    // Connect to Soulseek automatically
    tracing::info!("Connecting to Soulseek network as '{}'...", slsk_username);
    match download_service.connect(&slsk_username, &slsk_password).await {
        Ok(_) => tracing::info!("Successfully connected to Soulseek network"),
        Err(e) => {
            tracing::error!("Failed to connect to Soulseek: {}. The application will start but downloads will fail until connected.", e);
            tracing::warn!("Please check your Soulseek credentials or network connection.");
        }
    }

    // Create application state
    let state = AppState {
        db,
        download_service,
        jwt_secret,
        email_service,
        ws_broadcast,
    };

    // Create router
    let app = create_router(state);

    // Start server
    tracing::info!("Starting server on {}", bind_addr);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    axum::serve(listener, app)
        .await?;

    Ok(())
}
