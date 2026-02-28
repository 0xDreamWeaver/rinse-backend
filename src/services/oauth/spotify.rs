//! Spotify OAuth Provider
//!
//! Implements the MusicServiceProvider trait for Spotify using PKCE OAuth flow.
//! Handles authentication, token refresh, and playlist/track fetching.

use super::{
    AuthorizationUrl, ExternalPlaylist, ExternalTrack, ExternalUserProfile,
    MusicService, MusicServiceProvider, OAuthTokens,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const SPOTIFY_AUTH_URL: &str = "https://accounts.spotify.com/authorize";
const SPOTIFY_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SPOTIFY_API_BASE: &str = "https://api.spotify.com/v1";

/// Spotify provider configuration
pub struct SpotifyProvider {
    http_client: Client,
    client_id: String,
}

impl SpotifyProvider {
    /// Create a new Spotify provider
    /// Requires SPOTIFY_CLIENT_ID environment variable
    pub fn new(http_client: Client) -> Result<Self> {
        let client_id = std::env::var("SPOTIFY_CLIENT_ID")
            .context("SPOTIFY_CLIENT_ID environment variable not set")?;

        Ok(Self {
            http_client,
            client_id,
        })
    }

    /// Check if Spotify is configured
    pub fn is_configured() -> bool {
        std::env::var("SPOTIFY_CLIENT_ID").is_ok()
    }

    /// Generate a random code verifier for PKCE (43-128 characters)
    fn generate_code_verifier() -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let bytes: Vec<u8> = (0..32).map(|_| rng.gen()).collect();
        URL_SAFE_NO_PAD.encode(&bytes)
    }

    /// Generate code challenge from verifier (SHA256, base64url encoded)
    fn generate_code_challenge(verifier: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(verifier.as_bytes());
        let hash = hasher.finalize();
        URL_SAFE_NO_PAD.encode(&hash)
    }

    /// Generate a random state parameter for CSRF protection
    fn generate_state() -> String {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let bytes: Vec<u8> = (0..16).map(|_| rng.gen()).collect();
        URL_SAFE_NO_PAD.encode(&bytes)
    }

    /// Get the scopes needed for playlist import
    fn get_scopes() -> &'static str {
        "playlist-read-private playlist-read-collaborative user-read-private"
    }
}

// ============================================================================
// Spotify API Response Types
// ============================================================================

#[derive(Debug, Deserialize)]
struct SpotifyTokenResponse {
    access_token: String,
    token_type: String,
    expires_in: i64,
    refresh_token: Option<String>,
    scope: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyUserProfile {
    id: String,
    display_name: Option<String>,
    external_urls: SpotifyExternalUrls,
    images: Vec<SpotifyImage>,
}

#[derive(Debug, Deserialize)]
struct SpotifyExternalUrls {
    spotify: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyImage {
    url: String,
    height: Option<i32>,
    width: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistsResponse {
    items: Vec<SpotifyPlaylist>,
    total: i32,
    limit: i32,
    offset: i32,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylist {
    id: String,
    name: String,
    description: Option<String>,
    owner: SpotifyPlaylistOwner,
    // Feb 2026 API change: `tracks` renamed to `items`
    items: SpotifyPlaylistItems,
    images: Vec<SpotifyImage>,
    external_urls: SpotifyExternalUrls,
    public: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistOwner {
    id: String,
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistItems {
    total: i32,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistTracksResponse {
    items: Vec<SpotifyPlaylistTrackItem>,
    total: i32,
    limit: i32,
    offset: i32,
}

#[derive(Debug, Deserialize)]
struct SpotifyPlaylistTrackItem {
    // Feb 2026 API change: `track` renamed to `item`
    item: Option<SpotifyTrack>,
}

#[derive(Debug, Deserialize)]
struct SpotifyTrack {
    id: String,
    name: String,
    artists: Vec<SpotifyArtist>,
    album: SpotifyAlbum,
    duration_ms: i64,
    external_urls: SpotifyExternalUrls,
}

#[derive(Debug, Deserialize)]
struct SpotifyArtist {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyAlbum {
    id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct SpotifyError {
    error: SpotifyErrorDetails,
}

#[derive(Debug, Deserialize)]
struct SpotifyErrorDetails {
    status: i32,
    message: String,
}

// ============================================================================
// MusicServiceProvider Implementation
// ============================================================================

#[async_trait]
impl MusicServiceProvider for SpotifyProvider {
    fn service(&self) -> MusicService {
        MusicService::Spotify
    }

    fn get_authorization_url(&self, redirect_uri: &str) -> AuthorizationUrl {
        let code_verifier = Self::generate_code_verifier();
        let code_challenge = Self::generate_code_challenge(&code_verifier);
        let state = Self::generate_state();

        let url = format!(
            "{}?client_id={}&response_type=code&redirect_uri={}&scope={}&code_challenge_method=S256&code_challenge={}&state={}",
            SPOTIFY_AUTH_URL,
            urlencoding::encode(&self.client_id),
            urlencoding::encode(redirect_uri),
            urlencoding::encode(Self::get_scopes()),
            urlencoding::encode(&code_challenge),
            urlencoding::encode(&state)
        );

        AuthorizationUrl {
            url,
            state,
            code_verifier,
        }
    }

    async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<OAuthTokens> {
        tracing::debug!("[Spotify] Exchanging authorization code for tokens");

        let params = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &self.client_id),
            ("code_verifier", code_verifier),
        ];

        let response = self
            .http_client
            .post(SPOTIFY_TOKEN_URL)
            .form(&params)
            .send()
            .await
            .context("Failed to send token request to Spotify")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!(
                "[Spotify] Token exchange failed: status={}, body={}",
                status,
                error_text
            );
            anyhow::bail!("Spotify token exchange failed: {}", status);
        }

        let token_response: SpotifyTokenResponse = response
            .json()
            .await
            .context("Failed to parse Spotify token response")?;

        let expires_at = Utc::now() + Duration::seconds(token_response.expires_in);
        let scopes: Vec<String> = token_response
            .scope
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        tracing::info!(
            "[Spotify] Token exchange successful, expires in {}s",
            token_response.expires_in
        );

        Ok(OAuthTokens {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token,
            expires_at: Some(expires_at),
            scopes,
        })
    }

    async fn refresh_token(&self, refresh_token: &str) -> Result<OAuthTokens> {
        tracing::debug!("[Spotify] Refreshing access token");

        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &self.client_id),
        ];

        let response = self
            .http_client
            .post(SPOTIFY_TOKEN_URL)
            .form(&params)
            .send()
            .await
            .context("Failed to send refresh token request to Spotify")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!(
                "[Spotify] Token refresh failed: status={}, body={}",
                status,
                error_text
            );
            anyhow::bail!("Spotify token refresh failed: {}", status);
        }

        let token_response: SpotifyTokenResponse = response
            .json()
            .await
            .context("Failed to parse Spotify refresh token response")?;

        let expires_at = Utc::now() + Duration::seconds(token_response.expires_in);
        let scopes: Vec<String> = token_response
            .scope
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();

        tracing::info!(
            "[Spotify] Token refresh successful, expires in {}s",
            token_response.expires_in
        );

        Ok(OAuthTokens {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token, // May be None, caller should keep old one
            expires_at: Some(expires_at),
            scopes,
        })
    }

    async fn get_user_profile(&self, access_token: &str) -> Result<ExternalUserProfile> {
        tracing::debug!("[Spotify] Fetching user profile");

        let response = self
            .http_client
            .get(format!("{}/me", SPOTIFY_API_BASE))
            .bearer_auth(access_token)
            .send()
            .await
            .context("Failed to fetch Spotify user profile")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!(
                "[Spotify] Profile fetch failed: status={}, body={}",
                status,
                error_text
            );
            anyhow::bail!("Failed to fetch Spotify profile: {}", status);
        }

        let profile: SpotifyUserProfile = response
            .json()
            .await
            .context("Failed to parse Spotify profile response")?;

        let image_url = profile.images.first().map(|img| img.url.clone());

        Ok(ExternalUserProfile {
            id: profile.id.clone(),
            username: profile.id, // Spotify uses ID as username
            display_name: profile.display_name,
            profile_url: Some(profile.external_urls.spotify),
            image_url,
        })
    }

    async fn get_user_playlists(
        &self,
        access_token: &str,
        limit: i32,
        offset: i32,
    ) -> Result<(Vec<ExternalPlaylist>, i32)> {
        let limit = limit.min(50); // Spotify max is 50

        tracing::debug!(
            "[Spotify] Fetching playlists: limit={}, offset={}",
            limit,
            offset
        );

        let response = self
            .http_client
            .get(format!("{}/me/playlists", SPOTIFY_API_BASE))
            .bearer_auth(access_token)
            .query(&[("limit", limit), ("offset", offset)])
            .send()
            .await
            .context("Failed to fetch Spotify playlists")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!(
                "[Spotify] Playlists fetch failed: status={}, body={}",
                status,
                error_text
            );
            anyhow::bail!("Failed to fetch Spotify playlists: {}", status);
        }

        let playlists_response: SpotifyPlaylistsResponse = response
            .json()
            .await
            .context("Failed to parse Spotify playlists response")?;

        let playlists: Vec<ExternalPlaylist> = playlists_response
            .items
            .into_iter()
            .map(|p| ExternalPlaylist {
                id: p.id,
                name: p.name,
                description: p.description,
                owner_name: p.owner.display_name.unwrap_or(p.owner.id),
                track_count: p.items.total,
                image_url: p.images.first().map(|img| img.url.clone()),
                external_url: p.external_urls.spotify,
                is_public: p.public.unwrap_or(false),
            })
            .collect();

        tracing::debug!(
            "[Spotify] Fetched {} playlists (total: {})",
            playlists.len(),
            playlists_response.total
        );

        Ok((playlists, playlists_response.total))
    }

    async fn get_playlist(&self, access_token: &str, playlist_id: &str) -> Result<ExternalPlaylist> {
        tracing::debug!("[Spotify] Fetching playlist: {}", playlist_id);

        let response = self
            .http_client
            .get(format!("{}/playlists/{}", SPOTIFY_API_BASE, playlist_id))
            .bearer_auth(access_token)
            .send()
            .await
            .context("Failed to fetch Spotify playlist")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!(
                "[Spotify] Playlist fetch failed: status={}, body={}",
                status,
                error_text
            );
            anyhow::bail!("Failed to fetch Spotify playlist: {}", status);
        }

        let playlist: SpotifyPlaylist = response
            .json()
            .await
            .context("Failed to parse Spotify playlist response")?;

        Ok(ExternalPlaylist {
            id: playlist.id,
            name: playlist.name,
            description: playlist.description,
            owner_name: playlist.owner.display_name.unwrap_or(playlist.owner.id),
            track_count: playlist.items.total,
            image_url: playlist.images.first().map(|img| img.url.clone()),
            external_url: playlist.external_urls.spotify,
            is_public: playlist.public.unwrap_or(false),
        })
    }

    async fn get_playlist_tracks(
        &self,
        access_token: &str,
        playlist_id: &str,
        limit: i32,
        offset: i32,
    ) -> Result<(Vec<ExternalTrack>, i32)> {
        let limit = limit.min(100); // Spotify max is 100 for tracks

        tracing::debug!(
            "[Spotify] Fetching playlist tracks: playlist={}, limit={}, offset={}",
            playlist_id,
            limit,
            offset
        );

        // Feb 2026 API change: `/tracks` endpoint renamed to `/items`
        let response = self
            .http_client
            .get(format!(
                "{}/playlists/{}/items",
                SPOTIFY_API_BASE, playlist_id
            ))
            .bearer_auth(access_token)
            .query(&[("limit", limit), ("offset", offset)])
            .send()
            .await
            .context("Failed to fetch Spotify playlist items")?;

        if !response.status().is_success() {
            let status = response.status();
            let error_text = response.text().await.unwrap_or_default();
            tracing::error!(
                "[Spotify] Playlist tracks fetch failed: status={}, body={}",
                status,
                error_text
            );
            anyhow::bail!("Failed to fetch Spotify playlist tracks: {}", status);
        }

        let tracks_response: SpotifyPlaylistTracksResponse = response
            .json()
            .await
            .context("Failed to parse Spotify playlist tracks response")?;

        // Feb 2026 API change: nested field renamed from `track` to `item`
        let tracks: Vec<ExternalTrack> = tracks_response
            .items
            .into_iter()
            .filter_map(|entry| {
                entry.item.map(|t| ExternalTrack {
                    id: t.id,
                    name: t.name,
                    artists: t.artists.into_iter().map(|a| a.name).collect(),
                    album: Some(t.album.name),
                    duration_ms: Some(t.duration_ms),
                    external_url: t.external_urls.spotify,
                })
            })
            .collect();

        tracing::debug!(
            "[Spotify] Fetched {} tracks (total: {})",
            tracks.len(),
            tracks_response.total
        );

        Ok((tracks, tracks_response.total))
    }
}
