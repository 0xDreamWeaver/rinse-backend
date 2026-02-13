//! Metadata Service - Automatic track metadata enrichment
//!
//! This module provides automatic metadata lookup for downloaded tracks using:
//! - MusicBrainz: Artist, Album, Title, Year, Genre, Label, Track#
//! - Cover Art Archive: Album artwork
//! - GetSongBPM: BPM and Musical Key
//! - Local file analysis: Duration
//!
//! ## Usage
//!
//! ```ignore
//! let metadata_service = MetadataService::new(db, ws_broadcast);
//! let metadata = metadata_service.lookup_and_store(
//!     item_id,
//!     "/path/to/file.mp3",
//!     "Artist Title",           // Combined query (fallback)
//!     Some("Artist"),           // Original artist (optional)
//!     Some("Title"),            // Original track (optional)
//! ).await?;
//! ```
//!
//! ## Rate Limiting
//!
//! MusicBrainz requires 1 request/second. This is enforced internally.
//! Manual refresh is limited to once per 24 hours per track.

pub mod coverart;
pub mod getsongbpm;
pub mod musicbrainz;
pub mod tags;

// Phase 2 scaffolds
pub mod acoustid;
pub mod essentia;

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use reqwest::Client;
use tokio::sync::{broadcast, Mutex};
use tracing;

use crate::db::Database;
use crate::models::TrackMetadata;

use coverart::CoverArtClient;
use getsongbpm::GetSongBpmClient;
use musicbrainz::MusicBrainzClient;

/// Metadata service for enriching track information from external APIs
pub struct MetadataService {
    db: Database,
    ws_broadcast: broadcast::Sender<crate::api::WsEvent>,
    http_client: Client,
    musicbrainz: MusicBrainzClient,
    coverart: CoverArtClient,
    getsongbpm: GetSongBpmClient,
    /// Last MusicBrainz API call time (for rate limiting)
    last_mb_call: Arc<Mutex<Instant>>,
    /// Contact email for MusicBrainz User-Agent
    contact_email: String,
}

impl MetadataService {
    /// Create a new MetadataService
    ///
    /// # Arguments
    /// * `db` - Database connection
    /// * `ws_broadcast` - WebSocket broadcast channel
    pub fn new(db: Database, ws_broadcast: broadcast::Sender<crate::api::WsEvent>) -> Self {
        let http_client = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        // Get configuration from environment
        let contact_email = std::env::var("MUSICBRAINZ_CONTACT_EMAIL")
            .unwrap_or_else(|_| "rinse-app@example.com".to_string());

        // Determine environment - defaults to development
        // Set APP_ENV=production in production environments
        let is_production = std::env::var("APP_ENV")
            .map(|env| env.to_lowercase() == "production")
            .unwrap_or(false);

        // Use appropriate API key based on environment
        // Production: GETSONGBPM_API_KEY
        // Development: GETSONGBPM_API_KEY_DEV (falls back to GETSONGBPM_API_KEY if not set)
        let getsongbpm_api_key = if is_production {
            std::env::var("GETSONGBPM_API_KEY").ok()
        } else {
            std::env::var("GETSONGBPM_API_KEY_DEV")
                .ok()
                .or_else(|| std::env::var("GETSONGBPM_API_KEY").ok())
        };

        tracing::info!(
            "[MetadataService] Environment: {} (APP_ENV={})",
            if is_production { "production" } else { "development" },
            std::env::var("APP_ENV").unwrap_or_else(|_| "not set".to_string())
        );

        let musicbrainz = MusicBrainzClient::new(http_client.clone(), &contact_email);
        let coverart = CoverArtClient::new(http_client.clone());
        let getsongbpm = GetSongBpmClient::new(http_client.clone(), getsongbpm_api_key);

        // Initialize rate limiter with a time in the past
        let last_mb_call = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));

        tracing::info!(
            "[MetadataService] Initialized with contact_email={}, getsongbpm_configured={}",
            contact_email,
            getsongbpm.is_configured()
        );

        Self {
            db,
            ws_broadcast,
            http_client,
            musicbrainz,
            coverart,
            getsongbpm,
            last_mb_call,
            contact_email,
        }
    }

    /// Main entry point: Look up metadata and store in database
    ///
    /// This method:
    /// 1. Reads duration from the local file
    /// 2. Queries MusicBrainz for track info (rate limited)
    /// 3. Gets album art from Cover Art Archive
    /// 4. Gets BPM/Key from GetSongBPM
    /// 5. Stores results in database
    /// 6. Writes tags to the file
    ///
    /// All steps are best-effort - partial failures don't stop the process.
    ///
    /// # Arguments
    /// * `query` - Combined search query (used as fallback)
    /// * `artist` - Original artist from user input (optional, enables precise search)
    /// * `track` - Original track name from user input (optional, enables precise search)
    pub async fn lookup_and_store(
        &self,
        item_id: i64,
        file_path: &str,
        query: &str,
        artist: Option<&str>,
        track: Option<&str>,
    ) -> Result<TrackMetadata> {
        tracing::info!(
            "[MetadataService] Starting lookup for item_id={}, artist={:?}, track={:?}, query='{}'",
            item_id,
            artist,
            track,
            query
        );

        let mut metadata = TrackMetadata::default();
        let mut errors: Vec<String> = Vec::new();

        // Step 1: Read duration from local file
        match tags::read_duration(file_path) {
            Ok(duration_ms) => {
                metadata.duration_ms = Some(duration_ms);
                metadata.sources.push("local".to_string());
                tracing::debug!("[MetadataService] Duration: {}ms", duration_ms);
            }
            Err(e) => {
                errors.push(format!("local_duration: {}", e));
                tracing::warn!("[MetadataService] Failed to read duration: {}", e);
            }
        }

        // Step 2: Query MusicBrainz (with rate limiting)
        // Use precise search if we have both artist and track, otherwise use query
        self.wait_for_rate_limit().await;
        let mb_result = match (artist, track) {
            (Some(a), Some(t)) if !a.is_empty() && !t.is_empty() => {
                tracing::info!(
                    "[MetadataService] Using precise MusicBrainz search: artist='{}', title='{}'",
                    a, t
                );
                self.musicbrainz.search_recording_precise(a, t).await
            }
            _ => {
                tracing::info!(
                    "[MetadataService] Using query-based MusicBrainz search: '{}'",
                    query
                );
                self.musicbrainz.search_recording(query).await
            }
        };
        match mb_result {
            Ok(mb_metadata) => {
                let release_mbid = mb_metadata.musicbrainz_id.clone();
                metadata.merge(mb_metadata);
                tracing::info!(
                    "[MetadataService] MusicBrainz found: artist={:?}, title={:?}",
                    metadata.artist,
                    metadata.title
                );

                // Step 3: Get album art from Cover Art Archive (only if we have MBID)
                if let Some(ref mbid) = release_mbid {
                    // MusicBrainz recording ID is for the recording, we need release ID
                    // Let's try to get it from a separate lookup
                    self.wait_for_rate_limit().await;
                    match self.musicbrainz.get_release_mbid(query).await {
                        Ok(Some(release_id)) => {
                            match self.coverart.get_front_cover_url(&release_id).await {
                                Ok(url) => {
                                    metadata.album_art_url = Some(url);
                                    if !metadata.sources.contains(&"coverart".to_string()) {
                                        metadata.sources.push("coverart".to_string());
                                    }
                                }
                                Err(e) => {
                                    errors.push(format!("coverart: {}", e));
                                    tracing::debug!(
                                        "[MetadataService] Cover art not found: {}",
                                        e
                                    );
                                }
                            }
                        }
                        Ok(None) => {
                            tracing::debug!("[MetadataService] No release MBID found for cover art");
                        }
                        Err(e) => {
                            errors.push(format!("release_lookup: {}", e));
                        }
                    }
                }
            }
            Err(e) => {
                errors.push(format!("musicbrainz: {}", e));
                tracing::warn!("[MetadataService] MusicBrainz lookup failed: {}", e);
            }
        }

        // Step 4: Get BPM/Key from GetSongBPM (if configured and not already found)
        if self.getsongbpm.is_configured()
            && (metadata.bpm.is_none() || metadata.key.is_none())
        {
            match self.getsongbpm.search(query).await {
                Ok(bpm_result) => {
                    let found_bpm = bpm_result.bpm;
                    let found_key = bpm_result.key;
                    if metadata.bpm.is_none() {
                        metadata.bpm = found_bpm;
                    }
                    if metadata.key.is_none() {
                        metadata.key = found_key;
                    }
                    if !metadata.sources.contains(&"getsongbpm".to_string()) {
                        metadata.sources.push("getsongbpm".to_string());
                    }
                    tracing::info!(
                        "[MetadataService] GetSongBPM found: bpm={:?}, key={:?}",
                        metadata.bpm,
                        metadata.key
                    );
                }
                Err(e) => {
                    errors.push(format!("getsongbpm: {}", e));
                    tracing::warn!("[MetadataService] GetSongBPM lookup failed: {}", e);
                }
            }
        }

        // Set fetch timestamp
        metadata.fetched_at = Some(Utc::now());

        // Step 5: Store in database
        if let Err(e) = self.db.update_item_metadata(item_id, &metadata).await {
            errors.push(format!("db_store: {}", e));
            tracing::error!("[MetadataService] Failed to store metadata: {}", e);
        } else {
            tracing::info!("[MetadataService] Stored metadata for item_id={}", item_id);
        }

        // Step 6: Write tags to file (best effort)
        if metadata.has_core_fields() {
            if let Err(e) = tags::write_tags(file_path, &metadata) {
                errors.push(format!("tag_write: {}", e));
                tracing::warn!("[MetadataService] Failed to write file tags: {}", e);
            } else {
                tracing::info!("[MetadataService] Wrote tags to file: {}", file_path);
            }
        }

        // Log any partial failures
        if !errors.is_empty() {
            tracing::warn!(
                "[MetadataService] Completed with {} partial failures: {:?}",
                errors.len(),
                errors
            );
        }

        Ok(metadata)
    }

    /// Refresh metadata for an existing item
    ///
    /// Subject to 24-hour rate limit per track.
    pub async fn refresh_metadata(&self, item_id: i64) -> Result<TrackMetadata> {
        // Check rate limit
        if !self.can_refresh(item_id).await? {
            anyhow::bail!("Rate limit: metadata can only be refreshed once per 24 hours");
        }

        // Get item from database
        let item = self
            .db
            .get_item(item_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("Item not found: {}", item_id))?;

        // Perform lookup
        let metadata = self
            .lookup_and_store(
                item_id,
                &item.file_path,
                &item.original_query,
                item.original_artist.as_deref(),
                item.original_track.as_deref(),
            )
            .await?;

        // Update rate limit
        self.db.update_metadata_rate_limit(item_id).await?;

        Ok(metadata)
    }

    /// Check if metadata refresh is allowed (24-hour rate limit)
    pub async fn can_refresh(&self, item_id: i64) -> Result<bool> {
        self.db.check_metadata_rate_limit(item_id).await
    }

    /// Enforce MusicBrainz rate limit (1 request per second)
    async fn wait_for_rate_limit(&self) {
        let mut last_call = self.last_mb_call.lock().await;
        let elapsed = last_call.elapsed();
        let min_interval = Duration::from_secs(1);

        if elapsed < min_interval {
            let wait_time = min_interval - elapsed;
            tracing::debug!(
                "[MetadataService] Rate limiting: waiting {:?}",
                wait_time
            );
            tokio::time::sleep(wait_time).await;
        }

        *last_call = Instant::now();
    }

    /// Get metadata for an item from the database
    pub async fn get_metadata(&self, item_id: i64) -> Result<Option<TrackMetadata>> {
        self.db.get_item_metadata(item_id).await
    }

    /// Run metadata lookup for all items without metadata
    ///
    /// This is a background job that processes the entire library.
    /// Progress is reported via WebSocket events.
    pub async fn run_library_scan(&self) -> Result<LibraryScanResult> {
        tracing::info!("[MetadataService] Starting library metadata scan");

        let items = self.db.get_items_without_metadata().await?;
        let total = items.len();
        let mut completed = 0;
        let mut failed = 0;

        for item in items {
            // Broadcast progress
            let _ = self.ws_broadcast.send(crate::api::WsEvent::MetadataJobProgress {
                total: total as i64,
                completed,
                failed,
            });

            match self
                .lookup_and_store(
                    item.id,
                    &item.file_path,
                    &item.original_query,
                    item.original_artist.as_deref(),
                    item.original_track.as_deref(),
                )
                .await
            {
                Ok(_) => {
                    completed += 1;
                }
                Err(e) => {
                    failed += 1;
                    tracing::warn!(
                        "[MetadataService] Library scan failed for item {}: {}",
                        item.id,
                        e
                    );
                }
            }
        }

        // Final progress broadcast
        let _ = self.ws_broadcast.send(crate::api::WsEvent::MetadataJobProgress {
            total: total as i64,
            completed,
            failed,
        });

        tracing::info!(
            "[MetadataService] Library scan complete: total={}, completed={}, failed={}",
            total,
            completed,
            failed
        );

        Ok(LibraryScanResult {
            total: total as i64,
            completed,
            failed,
        })
    }
}

/// Result of a library-wide metadata scan
#[derive(Debug, Clone)]
pub struct LibraryScanResult {
    pub total: i64,
    pub completed: i64,
    pub failed: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_library_scan_result() {
        let result = LibraryScanResult {
            total: 100,
            completed: 95,
            failed: 5,
        };
        assert_eq!(result.total, 100);
    }
}
