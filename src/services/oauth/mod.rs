//! OAuth Service - Multi-provider OAuth2 management
//!
//! Provides a unified interface for:
//! - OAuth2 authorization flows (PKCE for public clients)
//! - Token storage with encryption at rest
//! - Automatic token refresh management
//! - Provider-specific API calls (playlists, tracks)

pub mod encryption;
pub mod spotify;

use crate::db::Database;
use crate::models::{OAuthConnection, OAuthPendingState};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Supported music streaming services
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MusicService {
    Spotify,
    Tidal,
    Soundcloud,
    Beatport,
}

impl MusicService {
    pub fn as_str(&self) -> &'static str {
        match self {
            MusicService::Spotify => "spotify",
            MusicService::Tidal => "tidal",
            MusicService::Soundcloud => "soundcloud",
            MusicService::Beatport => "beatport",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "spotify" => Some(MusicService::Spotify),
            "tidal" => Some(MusicService::Tidal),
            "soundcloud" => Some(MusicService::Soundcloud),
            "beatport" => Some(MusicService::Beatport),
            _ => None,
        }
    }
}

impl std::fmt::Display for MusicService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// OAuth token pair with metadata
#[derive(Debug, Clone)]
pub struct OAuthTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub scopes: Vec<String>,
}

/// External user profile from a music service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalUserProfile {
    pub id: String,
    pub username: String,
    pub display_name: Option<String>,
    pub profile_url: Option<String>,
    pub image_url: Option<String>,
}

/// Playlist summary from external service
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalPlaylist {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub owner_name: String,
    pub track_count: i32,
    pub image_url: Option<String>,
    pub external_url: String,
    pub is_public: bool,
}

/// Track from external playlist
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalTrack {
    pub id: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: Option<i64>,
    pub external_url: String,
}

/// Authorization URL response with PKCE parameters
#[derive(Debug, Clone)]
pub struct AuthorizationUrl {
    pub url: String,
    pub state: String,
    pub code_verifier: String,
}

/// Trait that all music service providers must implement
#[async_trait]
pub trait MusicServiceProvider: Send + Sync {
    /// Get the service type
    fn service(&self) -> MusicService;

    /// Get the OAuth authorization URL for user consent
    /// Returns AuthorizationUrl with the URL, state, and code_verifier (for PKCE)
    fn get_authorization_url(&self, redirect_uri: &str) -> AuthorizationUrl;

    /// Exchange authorization code for tokens
    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<OAuthTokens>;

    /// Refresh an expired access token
    async fn refresh_token(&self, refresh_token: &str) -> Result<OAuthTokens>;

    /// Check if access token is expired or will expire soon
    fn is_token_expired(&self, expires_at: Option<DateTime<Utc>>) -> bool {
        match expires_at {
            Some(exp) => Utc::now() + Duration::minutes(5) >= exp,
            None => false, // No expiry means it doesn't expire
        }
    }

    /// Get the user's profile
    async fn get_user_profile(&self, access_token: &str) -> Result<ExternalUserProfile>;

    /// Get the user's playlists (paginated)
    /// Returns (playlists, total_count)
    /// If current_user_id is provided, filters to playlists the user can edit
    async fn get_user_playlists(
        &self,
        access_token: &str,
        current_user_id: Option<&str>,
        limit: i32,
        offset: i32,
    ) -> Result<(Vec<ExternalPlaylist>, i32)>;

    /// Get a specific playlist's metadata
    async fn get_playlist(&self, access_token: &str, playlist_id: &str) -> Result<ExternalPlaylist>;

    /// Get tracks from a specific playlist (paginated)
    /// Returns (tracks, total_count)
    async fn get_playlist_tracks(
        &self,
        access_token: &str,
        playlist_id: &str,
        limit: i32,
        offset: i32,
    ) -> Result<(Vec<ExternalTrack>, i32)>;
}

/// OAuthService manages OAuth connections for all providers
pub struct OAuthService {
    db: Database,
    encryption_key: Vec<u8>,
    providers: HashMap<MusicService, Arc<dyn MusicServiceProvider>>,
    redirect_base: String,
}

impl OAuthService {
    /// Create a new OAuthService
    pub fn new(db: Database, jwt_secret: &str, redirect_base: String) -> Self {
        let encryption_key = encryption::derive_encryption_key(jwt_secret);

        Self {
            db,
            encryption_key,
            providers: HashMap::new(),
            redirect_base,
        }
    }

    /// Register a provider
    pub fn register_provider(&mut self, provider: Arc<dyn MusicServiceProvider>) {
        self.providers.insert(provider.service(), provider);
    }

    /// Get redirect URI for a service
    pub fn get_redirect_uri(&self, service: MusicService) -> String {
        format!("{}/oauth/{}/callback", self.redirect_base, service.as_str())
    }

    /// Get a registered provider
    pub fn get_provider(&self, service: MusicService) -> Option<&Arc<dyn MusicServiceProvider>> {
        self.providers.get(&service)
    }

    /// Check if a service is configured
    pub fn is_service_configured(&self, service: MusicService) -> bool {
        self.providers.contains_key(&service)
    }

    /// Get connection status for a user and service
    pub async fn get_connection_status(
        &self,
        user_id: i64,
        service: MusicService,
    ) -> Result<Option<OAuthConnection>> {
        self.db
            .get_oauth_connection(user_id, service.as_str())
            .await
    }

    /// Start OAuth authorization flow
    /// Returns the authorization URL to redirect the user to
    pub async fn start_authorization(
        &self,
        user_id: i64,
        service: MusicService,
    ) -> Result<String> {
        let provider = self
            .get_provider(service)
            .context(format!("Service {} is not configured", service))?;

        let redirect_uri = self.get_redirect_uri(service);
        let auth = provider.get_authorization_url(&redirect_uri);

        // Store the pending state for verification
        let expires_at = Utc::now() + Duration::minutes(5);
        self.db
            .create_oauth_pending_state(
                user_id,
                service.as_str(),
                &auth.state,
                &auth.code_verifier,
                expires_at,
            )
            .await?;

        Ok(auth.url)
    }

    /// Complete OAuth authorization by exchanging code for tokens
    pub async fn complete_authorization(
        &self,
        user_id: i64,
        service: MusicService,
        code: &str,
        state: &str,
    ) -> Result<ExternalUserProfile> {
        let provider = self
            .get_provider(service)
            .context(format!("Service {} is not configured", service))?;

        // Verify and retrieve the pending state
        let pending = self
            .db
            .get_and_delete_oauth_pending_state(state)
            .await?
            .context("Invalid or expired OAuth state")?;

        // Verify it belongs to this user
        if pending.user_id != user_id {
            anyhow::bail!("OAuth state does not belong to this user");
        }

        // Check expiry
        if pending.expires_at < Utc::now() {
            anyhow::bail!("OAuth state has expired");
        }

        // Exchange code for tokens
        let redirect_uri = self.get_redirect_uri(service);
        let tokens = provider
            .exchange_code(code, &redirect_uri, &pending.code_verifier)
            .await
            .context("Failed to exchange authorization code")?;

        // Get user profile
        let profile = provider
            .get_user_profile(&tokens.access_token)
            .await
            .context("Failed to get user profile")?;

        // Encrypt tokens
        let access_token_encrypted =
            encryption::encrypt_token(&tokens.access_token, &self.encryption_key)?;
        let refresh_token_encrypted = match &tokens.refresh_token {
            Some(rt) => Some(encryption::encrypt_token(rt, &self.encryption_key)?),
            None => None,
        };

        // Store connection
        self.db
            .upsert_oauth_connection(
                user_id,
                service.as_str(),
                &profile.id,
                &profile.username,
                &access_token_encrypted,
                refresh_token_encrypted.as_deref(),
                tokens.expires_at,
                &serde_json::to_string(&tokens.scopes)?,
            )
            .await?;

        tracing::info!(
            "[OAuth] Connected {} for user {} ({})",
            service,
            user_id,
            profile.username
        );

        Ok(profile)
    }

    /// Disconnect a service
    pub async fn disconnect(&self, user_id: i64, service: MusicService) -> Result<()> {
        self.db
            .delete_oauth_connection(user_id, service.as_str())
            .await?;

        tracing::info!("[OAuth] Disconnected {} for user {}", service, user_id);
        Ok(())
    }

    /// Get a valid access token, refreshing if necessary
    pub async fn get_valid_access_token(
        &self,
        user_id: i64,
        service: MusicService,
    ) -> Result<String> {
        let connection = self
            .db
            .get_oauth_connection(user_id, service.as_str())
            .await?
            .context("No connection found")?;

        if connection.status != "active" {
            anyhow::bail!("Connection is not active (status: {})", connection.status);
        }

        let provider = self
            .get_provider(service)
            .context(format!("Service {} is not configured", service))?;

        // Check if token needs refresh
        if provider.is_token_expired(connection.token_expires_at) {
            return self.refresh_access_token(user_id, service, &connection).await;
        }

        // Decrypt and return current token
        encryption::decrypt_token(&connection.access_token_encrypted, &self.encryption_key)
    }

    /// Refresh the access token
    async fn refresh_access_token(
        &self,
        user_id: i64,
        service: MusicService,
        connection: &OAuthConnection,
    ) -> Result<String> {
        let provider = self
            .get_provider(service)
            .context(format!("Service {} is not configured", service))?;

        let refresh_token_encrypted = connection
            .refresh_token_encrypted
            .as_ref()
            .context("No refresh token available")?;

        let refresh_token =
            encryption::decrypt_token(refresh_token_encrypted, &self.encryption_key)?;

        tracing::debug!("[OAuth] Refreshing token for {} user {}", service, user_id);

        let tokens = provider.refresh_token(&refresh_token).await.map_err(|e| {
            tracing::warn!(
                "[OAuth] Token refresh failed for {} user {}: {}",
                service,
                user_id,
                e
            );
            e
        })?;

        // Encrypt new tokens
        let access_token_encrypted =
            encryption::encrypt_token(&tokens.access_token, &self.encryption_key)?;
        let refresh_token_encrypted = match &tokens.refresh_token {
            Some(rt) => Some(encryption::encrypt_token(rt, &self.encryption_key)?),
            None => Some(encryption::encrypt_token(&refresh_token, &self.encryption_key)?), // Keep old refresh token
        };

        // Update connection
        self.db
            .update_oauth_tokens(
                user_id,
                service.as_str(),
                &access_token_encrypted,
                refresh_token_encrypted.as_deref(),
                tokens.expires_at,
            )
            .await?;

        tracing::info!("[OAuth] Refreshed token for {} user {}", service, user_id);

        Ok(tokens.access_token)
    }

    /// Get user's playlists from a service
    /// Only returns playlists the user has edit permissions for (owner or collaborator)
    pub async fn get_playlists(
        &self,
        user_id: i64,
        service: MusicService,
        limit: i32,
        offset: i32,
    ) -> Result<(Vec<ExternalPlaylist>, i32)> {
        // Get connection to access external_user_id for filtering
        let connection = self
            .db
            .get_oauth_connection(user_id, service.as_str())
            .await?
            .context("No connection found")?;

        let access_token = self.get_valid_access_token(user_id, service).await?;

        let provider = self
            .get_provider(service)
            .context(format!("Service {} is not configured", service))?;

        provider
            .get_user_playlists(&access_token, connection.external_user_id.as_deref(), limit, offset)
            .await
    }

    /// Get a specific playlist
    pub async fn get_playlist(
        &self,
        user_id: i64,
        service: MusicService,
        playlist_id: &str,
    ) -> Result<ExternalPlaylist> {
        let access_token = self.get_valid_access_token(user_id, service).await?;

        let provider = self
            .get_provider(service)
            .context(format!("Service {} is not configured", service))?;

        provider.get_playlist(&access_token, playlist_id).await
    }

    /// Get tracks from a playlist
    pub async fn get_playlist_tracks(
        &self,
        user_id: i64,
        service: MusicService,
        playlist_id: &str,
        limit: i32,
        offset: i32,
    ) -> Result<(Vec<ExternalTrack>, i32)> {
        let access_token = self.get_valid_access_token(user_id, service).await?;

        let provider = self
            .get_provider(service)
            .context(format!("Service {} is not configured", service))?;

        provider
            .get_playlist_tracks(&access_token, playlist_id, limit, offset)
            .await
    }

    /// Get all tracks from a playlist (handles pagination)
    pub async fn get_all_playlist_tracks(
        &self,
        user_id: i64,
        service: MusicService,
        playlist_id: &str,
    ) -> Result<Vec<ExternalTrack>> {
        let mut all_tracks = Vec::new();
        let mut offset = 0;
        let limit = 100; // Max per request for most services

        loop {
            let (tracks, total) = self
                .get_playlist_tracks(user_id, service, playlist_id, limit, offset)
                .await?;

            all_tracks.extend(tracks);

            offset += limit;
            if offset >= total {
                break;
            }
        }

        Ok(all_tracks)
    }
}
