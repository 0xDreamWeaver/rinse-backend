mod protocol;
mod db;
mod models;
mod api;
mod services;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

use db::Database;
use services::{DownloadService, EmailService, QueueService, QueueConfig, MetadataService, OAuthService};
use services::{SharingService, UploadService, UploadConfig};
use services::oauth::spotify::SpotifyProvider;
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

    // Concurrent search configuration
    let max_concurrent_searches: usize = std::env::var("MAX_CONCURRENT_SEARCHES")
        .unwrap_or_else(|_| "8".to_string())
        .parse()
        .unwrap_or(8);

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
    tokio::fs::create_dir_all(format!("{}/avatars", &storage_path)).await?;
    tokio::fs::create_dir_all(format!("{}/covers", &storage_path)).await?;

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
        db.create_user_verified("admin", &password_hash, "admin").await?;
        tracing::info!("Default admin user created (username: admin, password: admin123)");
    }

    // Create WebSocket broadcast channel
    let (ws_broadcast, _ws_rx) = create_broadcast_channel();

    // Initialize download service (handles connection and CRUD operations)
    let mut download_service = DownloadService::new(
        db.clone(),
        PathBuf::from(&storage_path),
    );

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

    // Connect to Soulseek and get the shared client
    tracing::info!("Connecting to Soulseek network as '{}'...", slsk_username);
    let slsk_client = match download_service.connect(&slsk_username, &slsk_password).await {
        Ok(client) => {
            tracing::info!("Successfully connected to Soulseek network");
            client
        }
        Err(e) => {
            tracing::error!("Failed to connect to Soulseek: {}. The application will not start without a connection.", e);
            return Err(e);
        }
    };

    let download_service = Arc::new(download_service);

    // Initialize sharing service
    let sharing_enabled: bool = std::env::var("SHARING_ENABLED")
        .unwrap_or_else(|_| "true".to_string())
        .parse()
        .unwrap_or(true);
    let sharing_service = Arc::new(SharingService::new(
        PathBuf::from(&storage_path),
        sharing_enabled,
    ));

    // Scan shared files on startup
    match sharing_service.scan().await {
        Ok((folders, files)) => {
            tracing::info!("Sharing: {} files in {} folders indexed", files, folders);
        }
        Err(e) => {
            tracing::warn!("Sharing: scan failed: {} - uploads will be disabled", e);
        }
    }

    // Initialize upload service
    let upload_config = UploadConfig {
        max_upload_slots: std::env::var("MAX_UPLOAD_SLOTS")
            .unwrap_or_else(|_| "3".to_string())
            .parse()
            .unwrap_or(3),
        max_upload_speed_kbps: std::env::var("MAX_UPLOAD_SPEED_KBPS")
            .unwrap_or_else(|_| "0".to_string())
            .parse()
            .unwrap_or(0),
        sharing_enabled,
    };
    tracing::info!(
        "Upload config: slots={}, speed_limit={}KB/s, enabled={}",
        upload_config.max_upload_slots, upload_config.max_upload_speed_kbps, upload_config.sharing_enabled
    );
    let upload_service = Arc::new(UploadService::new(
        upload_config,
        Arc::clone(&sharing_service),
        ws_broadcast.clone(),
    ));

    // Start upload queue checker
    let _upload_checker = upload_service.start_queue_checker();

    // Wire sharing and upload services into the Soulseek client
    // (client was created before these services, so we set them after)
    slsk_client.set_sharing_service(Arc::clone(&sharing_service)).await;
    slsk_client.set_upload_service(Arc::clone(&upload_service)).await;
    // Wire the Soulseek client into the upload service (for establishing P/F connections)
    upload_service.set_slsk_client(Arc::clone(&slsk_client)).await;
    tracing::info!("Sharing and upload services wired into Soulseek client");

    // Report real share counts to server (replace the initial 0,0)
    {
        let (folders, files) = sharing_service.counts().await;
        if files > 0 {
            let mut data = bytes::BytesMut::new();
            bytes::BufMut::put_u32_le(&mut data, folders);
            bytes::BufMut::put_u32_le(&mut data, files);
            if let Err(e) = slsk_client.router().send_message(35, &data).await {
                tracing::warn!("Failed to send SharedFoldersFiles to server: {}", e);
            } else {
                tracing::info!("Reported {} files in {} folders to Soulseek server", files, folders);
            }
        }
    }

    // Start background task for periodic upload speed reporting
    {
        let upload_svc = Arc::clone(&upload_service);
        let client = Arc::clone(&slsk_client);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            loop {
                interval.tick().await;
                let speed = upload_svc.average_speed_bytes_per_sec().await;
                if speed > 0 {
                    let mut data = bytes::BytesMut::new();
                    bytes::BufMut::put_u32_le(&mut data, speed as u32);
                    if let Err(e) = client.router().send_message(121, &data).await {
                        tracing::debug!("Failed to send upload speed to server: {}", e);
                    }
                }
            }
        });
    }

    // Forward incoming chat messages to the WebSocket broadcast channel
    {
        let mut chat_rx = slsk_client.router().subscribe_chat();
        let ws_tx = ws_broadcast.clone();
        tokio::spawn(async move {
            loop {
                match chat_rx.recv().await {
                    Ok(msg) => {
                        let _ = ws_tx.send(crate::api::WsEvent::ChatMessage {
                            username: msg.username,
                            message: msg.message,
                            timestamp: msg.timestamp,
                            incoming: msg.incoming,
                            is_new: msg.is_new,
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("Chat forwarder lagged by {} messages", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        tracing::warn!("Chat broadcast channel closed");
                        break;
                    }
                }
            }
        });
    }

    // Create metadata service for track enrichment
    tracing::info!("Initializing metadata service...");
    let metadata_service = Arc::new(MetadataService::new(
        db.clone(),
        ws_broadcast.clone(),
        &storage_path,
    ));

    // Initialize OAuth service (optional - requires SPOTIFY_CLIENT_ID for Spotify)
    let oauth_service = if SpotifyProvider::is_configured() {
        tracing::info!("Spotify OAuth configured, initializing OAuthService...");
        let redirect_base = std::env::var("OAUTH_REDIRECT_BASE")
            .unwrap_or_else(|_| "http://localhost:5173".to_string());

        let http_client = reqwest::Client::new();
        match SpotifyProvider::new(http_client) {
            Ok(spotify_provider) => {
                let mut service = OAuthService::new(db.clone(), &jwt_secret, redirect_base);
                service.register_provider(Arc::new(spotify_provider));
                tracing::info!("OAuthService initialized with Spotify provider");
                Some(Arc::new(service))
            }
            Err(e) => {
                tracing::error!("Failed to create SpotifyProvider: {}", e);
                None
            }
        }
    } else {
        tracing::info!("Spotify OAuth not configured (SPOTIFY_CLIENT_ID not set)");
        None
    };

    // Create queue service with shared SoulseekClient
    let queue_config = QueueConfig {
        poll_interval_ms: 500,
        search_timeout_secs: 10,
        storage_path: PathBuf::from(&storage_path),
        max_concurrent_searches,
    };
    tracing::info!(
        "Queue config: max_concurrent_searches={}, search_timeout={}s",
        max_concurrent_searches, queue_config.search_timeout_secs
    );
    let queue_service = Arc::new(QueueService::new(
        db.clone(),
        Arc::clone(&slsk_client),
        ws_broadcast.clone(),
        queue_config,
        Arc::clone(&metadata_service),
        Arc::clone(&sharing_service),
    ));

    // Perform recovery on startup (reset interrupted downloads)
    tracing::info!("Running queue recovery...");
    match queue_service.recover_on_startup().await {
        Ok((searches_reset, downloads_failed)) => {
            if searches_reset > 0 || downloads_failed > 0 {
                tracing::info!(
                    "Recovery complete: {} searches reset, {} interrupted downloads marked failed",
                    searches_reset, downloads_failed
                );
            }
        }
        Err(e) => {
            tracing::warn!("Queue recovery failed: {}", e);
        }
    }

    // Recalculate progress for any lists stuck in "downloading" status
    queue_service.recalculate_downloading_lists().await;

    // Start queue worker and transfer monitor
    tracing::info!("Starting queue worker and transfer monitor...");
    let _queue_worker = queue_service.start_worker();
    let _transfer_monitor = queue_service.start_transfer_monitor();

    // Create application state
    let state = AppState {
        db,
        download_service,
        queue_service,
        metadata_service,
        oauth_service,
        sharing_service,
        upload_service,
        slsk_client,
        jwt_secret,
        email_service,
        ws_broadcast,
        storage_path: storage_path.clone(),
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
