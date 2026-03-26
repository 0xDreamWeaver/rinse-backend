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

pub mod cover_cache;
pub mod coverart;
pub mod discogs;
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

use cover_cache::CoverCache;
use coverart::CoverArtClient;
use discogs::DiscogsClient;
use getsongbpm::GetSongBpmClient;
use musicbrainz::MusicBrainzClient;

/// Metadata service for enriching track information from external APIs
pub struct MetadataService {
    db: Database,
    ws_broadcast: broadcast::Sender<crate::api::WsEvent>,
    http_client: Client,
    musicbrainz: MusicBrainzClient,
    discogs: DiscogsClient,
    coverart: CoverArtClient,
    getsongbpm: GetSongBpmClient,
    cover_cache: CoverCache,
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
    /// * `storage_path` - Path to storage directory (for cover art cache)
    pub fn new(db: Database, ws_broadcast: broadcast::Sender<crate::api::WsEvent>, storage_path: &str) -> Self {
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
        let discogs = DiscogsClient::new(http_client.clone());
        let coverart = CoverArtClient::new(http_client.clone());
        let getsongbpm = GetSongBpmClient::new(http_client.clone(), getsongbpm_api_key);
        let cover_cache = CoverCache::new(storage_path, http_client.clone());

        // Initialize rate limiter with a time in the past
        let last_mb_call = Arc::new(Mutex::new(Instant::now() - Duration::from_secs(10)));

        tracing::info!(
            "[MetadataService] Initialized with contact_email={}, discogs_configured={}, getsongbpm_configured={}",
            contact_email,
            discogs.is_configured(),
            getsongbpm.is_configured()
        );

        Self {
            db,
            ws_broadcast,
            http_client,
            musicbrainz,
            discogs,
            coverart,
            getsongbpm,
            cover_cache,
            last_mb_call,
            contact_email,
        }
    }

    /// Get a reference to the cover cache
    pub fn cover_cache(&self) -> &CoverCache {
        &self.cover_cache
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
        let display_name = match (artist, track) {
            (Some(a), Some(t)) if !a.is_empty() && !t.is_empty() => format!("{} - {}", a, t),
            _ => query.to_string(),
        };
        tracing::info!("[Metadata] Lookup: item_id={} '{}'", item_id, display_name);

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

        // Step 2: Query MusicBrainz (with rate limiting), fallback to Discogs
        // Use precise search if we have both artist and track, otherwise use query
        let mut result_parts: Vec<String> = Vec::new();

        self.wait_for_rate_limit().await;
        let mb_result = match (artist, track) {
            (Some(a), Some(t)) if !a.is_empty() && !t.is_empty() => {
                self.musicbrainz.search_recording_precise(a, t).await
            }
            _ => {
                self.musicbrainz.search_recording(query).await
            }
        };

        let mut got_metadata_from_primary = false;
        match mb_result {
            Ok(mb_metadata) => {
                let release_mbid = mb_metadata.musicbrainz_id.clone();
                metadata.merge(mb_metadata);
                got_metadata_from_primary = true;
                result_parts.push("mb:found".to_string());

                // Step 3: Get album art from Cover Art Archive (only if we have MBID)
                if let Some(ref _mbid) = release_mbid {
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
                result_parts.push("mb:failed".to_string());
                tracing::warn!("[Metadata] MusicBrainz failed: {}", e);
            }
        }

        // Step 2b: Fallback to Discogs if MusicBrainz failed or returned incomplete data
        if !got_metadata_from_primary || metadata.artist.is_none() {
            let discogs_result = match (artist, track) {
                (Some(a), Some(t)) if !a.is_empty() && !t.is_empty() => {
                    self.discogs.search_release_precise(a, t).await
                }
                _ => {
                    self.discogs.search_release(query).await
                }
            };
            match discogs_result {
                Ok(discogs_metadata) => {
                    // Merge Discogs data, preferring existing MusicBrainz data
                    if metadata.artist.is_none() {
                        metadata.artist = discogs_metadata.artist;
                    }
                    if metadata.album.is_none() {
                        metadata.album = discogs_metadata.album;
                    }
                    if metadata.title.is_none() {
                        metadata.title = discogs_metadata.title;
                    }
                    if metadata.year.is_none() {
                        metadata.year = discogs_metadata.year;
                    }
                    if metadata.genre.is_none() {
                        metadata.genre = discogs_metadata.genre;
                    }
                    if metadata.label.is_none() {
                        metadata.label = discogs_metadata.label;
                    }
                    // Use Discogs album art if we don't have one from Cover Art Archive
                    if metadata.album_art_url.is_none() {
                        metadata.album_art_url = discogs_metadata.album_art_url;
                    }
                    if !metadata.sources.contains(&"discogs".to_string()) {
                        metadata.sources.push("discogs".to_string());
                    }
                    result_parts.push("discogs:merged".to_string());
                }
                Err(e) => {
                    errors.push(format!("discogs: {}", e));
                    result_parts.push("discogs:failed".to_string());
                    tracing::warn!("[Metadata] Discogs failed: {}", e);
                }
            }
        }

        // Cache cover art locally (after all art sources have been tried)
        if let Some(ref art_url) = metadata.album_art_url {
            match self.cover_cache.cache_cover(item_id, art_url).await {
                Ok(true) => {
                    let _ = self.db.set_cover_cached(item_id, true).await;
                    tracing::info!("[CoverCache] Cached cover art for item {}", item_id);
                }
                Ok(false) | Err(_) => {
                    tracing::debug!("[CoverCache] Cover caching skipped/failed for item {}", item_id);
                }
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
                    if let Some(bpm) = metadata.bpm {
                        result_parts.push(format!("bpm:{}", bpm));
                    }
                    if let Some(ref key) = metadata.key {
                        result_parts.push(format!("key:{}", key));
                    }
                }
                Err(e) => {
                    errors.push(format!("getsongbpm: {}", e));
                    tracing::warn!("[Metadata] GetSongBPM failed: {}", e);
                }
            }
        }

        // Set fetch timestamp
        metadata.fetched_at = Some(Utc::now());

        // Step 5: Store in database
        let mut db_ok = true;
        if let Err(e) = self.db.update_item_metadata(item_id, &metadata).await {
            errors.push(format!("db_store: {}", e));
            tracing::error!("[Metadata] Failed to store metadata: {}", e);
            db_ok = false;
        }

        // Step 6: Write tags to file (best effort)
        let mut tags_written = false;
        if metadata.has_core_fields() {
            if let Err(e) = tags::write_tags(file_path, &metadata) {
                errors.push(format!("tag_write: {}", e));
                tracing::warn!("[Metadata] Failed to write file tags: {}", e);
            } else {
                tags_written = true;
            }
        }

        // Build summary
        if tags_written {
            result_parts.push("tags:written".to_string());
        }
        if !db_ok {
            result_parts.push("db:failed".to_string());
        }

        tracing::info!(
            "[Metadata] Result: item_id={} — {}",
            item_id, result_parts.join(", ")
        );

        if !errors.is_empty() {
            tracing::debug!(
                "[Metadata] Partial failures for item_id={}: {:?}",
                item_id, errors
            );
        }

        Ok(metadata)
    }

    /// Refresh metadata for an existing item
    ///
    /// Subject to 24-hour rate limit per track.
    ///
    /// If `skip_existing` is true and the item already has core metadata
    /// (artist + title + album art), the API lookup is skipped. Audio
    /// properties (sample_rate, bit_depth) are always read regardless.
    pub async fn refresh_metadata(&self, item_id: i64, skip_existing: bool) -> Result<TrackMetadata> {
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

        // Always read audio properties from the file (backfills sample_rate/bit_depth)
        match tags::read_audio_properties(&item.file_path) {
            Ok(props) => {
                if let Err(e) = self
                    .db
                    .update_item_audio_properties(item_id, props.sample_rate, props.bit_depth)
                    .await
                {
                    tracing::warn!(
                        "[MetadataService] Failed to update audio properties for item {}: {}",
                        item_id, e
                    );
                } else {
                    tracing::debug!(
                        "[Metadata] Audio properties for item {}: sample_rate={:?}, bit_depth={:?}",
                        item_id, props.sample_rate, props.bit_depth
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    "[MetadataService] Failed to read audio properties for item {}: {}",
                    item_id, e
                );
            }
        }

        // If skip_existing is set and item already has core metadata, skip API lookup
        if skip_existing
            && item.meta_artist.is_some()
            && item.meta_title.is_some()
            && item.meta_album_art_url.is_some()
        {
            tracing::info!(
                "[MetadataService] Skipping API lookup for item {} (already has core metadata)",
                item_id
            );
            // Return existing metadata from database
            let existing = self.db.get_item_metadata(item_id).await?;
            return Ok(existing.unwrap_or_default());
        }

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
