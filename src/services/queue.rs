//! Queue service for managing search queue and spawned file transfers
//!
//! This service provides:
//! - Enqueueing of single searches and lists
//! - A queue worker that processes searches concurrently (semaphore-bounded)
//! - A transfer monitor that handles completion of spawned file transfers
//!
//! Architecture:
//! ```text
//! Client submits search/list
//!          ↓
//!     Search Queue (DB)
//!          ↓
//!     Queue Worker (concurrent, semaphore-bounded)
//!          ↓
//!     Search → Negotiate → Get FileConnection (parallel)
//!          ↓
//!     Spawn file transfer (independent task)
//!          ↓
//!     Transfer Monitor ← completion notification
//!          ↓
//!     Update DB + Broadcast WebSocket
//! ```

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::path::{Path, PathBuf};
use tokio::sync::{broadcast, mpsc, RwLock, Semaphore};
use tokio::task::JoinHandle;
use anyhow::{Result, bail};

use crate::db::Database;
use crate::models::{QueuedSearch, List, Item, QueueStatus, QueueStatusResponse};
use crate::protocol::{SoulseekClient, DownloadInitResult, ArcProgressCallback};
use crate::protocol::file_selection::ScoredFile;
use crate::api::WsEvent;
use crate::services::FuzzyMatcher;
use crate::services::MetadataService;

/// Extract the basename from a Soulseek path.
/// Soulseek paths use Windows-style backslashes, so we need to handle both \ and /
fn extract_basename(soulseek_path: &str) -> &str {
    let last_sep = soulseek_path
        .rfind(|c| c == '\\' || c == '/')
        .map(|i| i + 1)
        .unwrap_or(0);
    &soulseek_path[last_sep..]
}

/// Notification sent when a spawned file transfer completes
#[derive(Debug)]
pub struct TransferCompletion {
    pub item_id: i64,
    pub queue_id: i64,
    pub list_id: Option<i64>,
    pub list_position: Option<i32>,  // Position in list (for list_items association)
    pub result: Result<u64, String>,  // Ok(bytes) or Err(error message)
    pub client_id: Option<String>,    // Client-generated ID for frontend tracking
}

/// Queue service configuration
#[derive(Debug, Clone)]
pub struct QueueConfig {
    /// How long to wait between queue polls when idle (ms)
    pub poll_interval_ms: u64,
    /// Search timeout in seconds
    pub search_timeout_secs: u64,
    /// Storage path for downloaded files
    pub storage_path: PathBuf,
    /// Maximum number of concurrent searches (default: 8)
    pub max_concurrent_searches: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 500,
            search_timeout_secs: 10,
            storage_path: PathBuf::from("storage"),
            max_concurrent_searches: 8,
        }
    }
}

/// Queue service for managing search queue and file transfers
pub struct QueueService {
    db: Database,
    slsk_client: Arc<SoulseekClient>,
    ws_broadcast: broadcast::Sender<WsEvent>,
    config: QueueConfig,
    /// Channel for receiving transfer completion notifications
    transfer_completion_tx: mpsc::Sender<TransferCompletion>,
    transfer_completion_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<TransferCompletion>>>,
    /// Metadata service for enriching downloaded tracks
    metadata_service: Arc<MetadataService>,
    /// Sharing service for adding newly downloaded files to the share index
    sharing_service: Arc<crate::services::SharingService>,
    /// Cached search candidates by queue_id for retry without re-searching
    /// When a download fails, we try the next candidate from this list
    pending_candidates: Arc<RwLock<HashMap<i64, Vec<ScoredFile>>>>,
}

impl QueueService {
    /// Create a new queue service
    pub fn new(
        db: Database,
        slsk_client: Arc<SoulseekClient>,
        ws_broadcast: broadcast::Sender<WsEvent>,
        config: QueueConfig,
        metadata_service: Arc<MetadataService>,
        sharing_service: Arc<crate::services::SharingService>,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<TransferCompletion>(100);

        Self {
            db,
            slsk_client,
            ws_broadcast,
            config,
            transfer_completion_tx: tx,
            transfer_completion_rx: Arc::new(tokio::sync::Mutex::new(rx)),
            metadata_service,
            sharing_service,
            pending_candidates: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get the transfer completion sender (for spawned tasks to send completions)
    pub fn completion_sender(&self) -> mpsc::Sender<TransferCompletion> {
        self.transfer_completion_tx.clone()
    }

    /// Send a WebSocket event (ignores errors if no clients connected)
    fn broadcast(&self, event: WsEvent) {
        let _ = self.ws_broadcast.send(event);
    }

    // ========================================================================
    // Enqueueing methods
    // ========================================================================

    /// Enqueue a single search
    ///
    /// Returns the queued search entry and its position in the queue
    ///
    /// # Arguments
    /// * `query` - Combined search query for Soulseek
    /// * `artist` - Original artist from user input (optional)
    /// * `track` - Original track name from user input (optional)
    pub async fn enqueue_search(
        &self,
        user_id: i64,
        query: &str,
        artist: Option<&str>,
        track: Option<&str>,
        format: Option<&str>,
        client_id: Option<&str>,
    ) -> Result<(QueuedSearch, i64)> {
        // Check for existing completed item (duplicate detection)
        // Note: This uses fuzzy matching which needs improvement
        if let Some(existing) = self.db.find_completed_item_by_query(query).await? {
            tracing::info!(
                "[QueueService] Duplicate detected for query '{}', existing item: {}",
                query, existing.id
            );
            // For now, we still enqueue - duplicate detection happens during processing
            // This allows the user to force re-download if needed
        }

        // Enqueue the search
        let queued = self.db.enqueue_search(user_id, query, artist, track, format, None, None, client_id).await?;
        let position = self.db.get_queue_position(queued.id).await?;

        tracing::info!(
            "[QueueService] Enqueued search: id={}, artist={:?}, track={:?}, query='{}', position={}, client_id={:?}",
            queued.id, artist, track, query, position, client_id
        );

        // Broadcast queue event
        self.broadcast(WsEvent::SearchQueued {
            queue_id: queued.id,
            query: query.to_string(),
            position: position as i32,
            client_id: client_id.map(|s| s.to_string()),
        });

        Ok((queued, position))
    }

    /// Enqueue a list of searches
    ///
    /// Creates a list and enqueues all searches for it
    ///
    /// # Arguments
    /// * `tracks` - Vec of (query, artist, track) tuples
    pub async fn enqueue_list(
        &self,
        user_id: i64,
        name: Option<String>,
        tracks: Vec<(String, Option<String>, Option<String>, Option<String>)>,
        format: Option<String>,
    ) -> Result<(List, Vec<QueuedSearch>)> {
        let list_name = name.unwrap_or_else(|| {
            chrono::Utc::now().format("List %Y-%m-%d %H:%M").to_string()
        });

        // Create the list
        let list = self.db.create_list(&list_name, user_id, tracks.len() as i32).await?;

        tracing::info!(
            "[QueueService] Created list: id={}, name='{}', items={}",
            list.id, list_name, tracks.len()
        );

        // Enqueue all searches
        let queued_searches = self.db.enqueue_searches_for_list(
            user_id,
            &tracks,
            format.as_deref(),
            list.id,
        ).await?;

        tracing::info!(
            "[QueueService] Enqueued {} searches for list {}",
            queued_searches.len(), list.id
        );

        // Broadcast list created event
        self.broadcast(WsEvent::ListCreated {
            list_id: list.id,
            name: list_name.clone(),
            total_items: tracks.len() as i32,
        });

        Ok((list, queued_searches))
    }

    // ========================================================================
    // Queue status methods
    // ========================================================================

    /// Get overall queue status
    pub async fn get_queue_status(&self) -> Result<QueueStatusResponse> {
        let (pending, processing) = self.db.get_queue_status().await?;
        let active_downloads = self.db.get_active_downloads_count().await?;

        Ok(QueueStatusResponse {
            pending,
            processing,
            active_downloads,
            user_pending: 0,      // Set by caller if needed
            user_processing: 0,   // Set by caller if needed
        })
    }

    /// Get queue status for a specific user
    pub async fn get_user_queue_status(&self, user_id: i64) -> Result<QueueStatusResponse> {
        let (pending, processing) = self.db.get_queue_status().await?;
        let (user_pending, user_processing) = self.db.get_user_queue_status(user_id).await?;
        let active_downloads = self.db.get_active_downloads_count().await?;

        Ok(QueueStatusResponse {
            pending,
            processing,
            active_downloads,
            user_pending,
            user_processing,
        })
    }

    /// Get pending queue items for a user
    pub async fn get_user_pending_queue(&self, user_id: i64) -> Result<Vec<QueuedSearch>> {
        self.db.get_user_pending_queue(user_id).await
    }

    // ========================================================================
    // Cancellation methods
    // ========================================================================

    /// Cancel a pending search
    ///
    /// Returns true if cancelled, false if not found or not pending
    pub async fn cancel_search(&self, queue_id: i64) -> Result<bool> {
        let cancelled = self.db.cancel_pending_search(queue_id).await?;
        if cancelled {
            tracing::info!("[QueueService] Cancelled pending search: {}", queue_id);
        }
        Ok(cancelled)
    }

    /// Cancel all pending searches for a list
    pub async fn cancel_list(&self, list_id: i64) -> Result<u64> {
        let count = self.db.cancel_list_pending_searches(list_id).await?;
        tracing::info!(
            "[QueueService] Cancelled {} pending searches for list {}",
            count, list_id
        );
        Ok(count)
    }

    // ========================================================================
    // Recovery methods (called on startup)
    // ========================================================================

    /// Perform recovery operations after restart
    ///
    /// - Resets any 'processing' searches back to 'pending'
    /// - Marks any 'downloading' items as failed
    pub async fn recover_on_startup(&self) -> Result<(u64, u64)> {
        let searches_reset = self.db.recover_processing_searches().await?;
        let downloads_failed = self.db.mark_interrupted_downloads().await?;

        if searches_reset > 0 {
            tracing::info!(
                "[QueueService] Recovery: Reset {} processing searches to pending",
                searches_reset
            );
        }

        if downloads_failed > 0 {
            tracing::info!(
                "[QueueService] Recovery: Marked {} interrupted downloads as failed",
                downloads_failed
            );
        }

        Ok((searches_reset, downloads_failed))
    }

    /// Recalculate progress for all lists stuck in "downloading" status.
    /// Should be called after `recover_on_startup()` to fix lists whose items
    /// were marked failed by recovery but whose list status was never updated.
    pub async fn recalculate_downloading_lists(&self) {
        match self.db.get_lists_by_status("downloading").await {
            Ok(lists) => {
                if !lists.is_empty() {
                    tracing::info!(
                        "[QueueService] Recalculating progress for {} downloading lists",
                        lists.len()
                    );
                    for list in lists {
                        self.update_list_progress(list.id).await;
                    }
                }
            }
            Err(e) => {
                tracing::error!("[QueueService] Failed to recalculate list progress: {}", e);
            }
        }
    }

    // ========================================================================
    // Worker and monitor startup
    // ========================================================================

    /// Start the queue worker
    ///
    /// Start the queue worker with concurrent search processing.
    ///
    /// Uses a semaphore to limit concurrent searches to `max_concurrent_searches`.
    /// Each search is spawned as an independent task, enabling parallel processing.
    ///
    /// Returns a JoinHandle that can be used to monitor/abort the worker.
    pub fn start_worker(self: &Arc<Self>) -> JoinHandle<()> {
        let service = Arc::clone(self);
        let semaphore = Arc::new(Semaphore::new(service.config.max_concurrent_searches));

        tokio::spawn(async move {
            tracing::info!(
                "[QueueWorker] Started (max_concurrent_searches={})",
                service.config.max_concurrent_searches
            );

            loop {
                // Acquire semaphore permit (limits concurrency)
                let permit = match semaphore.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        tracing::error!("[QueueWorker] Semaphore closed");
                        break;
                    }
                };

                // Try to claim the next pending search atomically
                // (mark as processing BEFORE spawning to prevent TOCTOU race)
                match service.db.claim_next_pending_search().await {
                    Ok(Some(queued)) => {
                        let svc = Arc::clone(&service);
                        // Spawn each search as independent task
                        tokio::spawn(async move {
                            svc.process_queued_search(queued).await;
                            drop(permit); // Release semaphore
                        });
                    }
                    Ok(None) => {
                        // No pending searches, release permit and wait
                        drop(permit);
                        tokio::time::sleep(Duration::from_millis(service.config.poll_interval_ms)).await;
                    }
                    Err(e) => {
                        drop(permit);
                        tracing::error!("[QueueWorker] Error claiming next search: {}", e);
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        })
    }

    /// Start the transfer monitor
    ///
    /// The monitor receives completion notifications from spawned file transfers
    /// and updates the database and broadcasts WebSocket events.
    ///
    /// Returns a JoinHandle that can be used to monitor/abort the monitor.
    pub fn start_transfer_monitor(self: &Arc<Self>) -> JoinHandle<()> {
        let service = Arc::clone(self);
        let rx = Arc::clone(&service.transfer_completion_rx);

        tokio::spawn(async move {
            tracing::info!("[TransferMonitor] Started");

            let mut rx_guard = rx.lock().await;
            while let Some(completion) = rx_guard.recv().await {
                service.handle_transfer_completion(completion).await;
            }

            tracing::warn!("[TransferMonitor] Channel closed, stopping");
        })
    }

    // ========================================================================
    // Internal processing methods
    // ========================================================================

    /// Process a queued search
    ///
    /// This is called by the queue worker for each search.
    async fn process_queued_search(self: &Arc<Self>, queued: QueuedSearch) {
        let queue_id = queued.id;
        let query = queued.query.clone();
        let retry_count = queued.retry_count;
        let client_id = queued.client_id.clone();

        tracing::info!(
            "[QueueWorker] Processing: id={}, query='{}', retry={}",
            queue_id, query, retry_count
        );

        // Verify this search is still in 'processing' state (it was claimed atomically by the worker loop).
        // This guards against edge cases where the search was cancelled between claim and processing.
        match self.db.get_queued_search(queue_id).await {
            Ok(Some(q)) if q.status == "processing" => { /* good, proceed */ }
            Ok(Some(q)) => {
                tracing::warn!(
                    "[QueueWorker] Search {} has unexpected status '{}', skipping",
                    queue_id, q.status
                );
                return;
            }
            Ok(None) => {
                tracing::warn!("[QueueWorker] Search {} no longer exists, skipping", queue_id);
                return;
            }
            Err(e) => {
                tracing::error!("[QueueWorker] Failed to verify search status: {}", e);
                return;
            }
        }

        // Broadcast processing started
        self.broadcast(WsEvent::SearchProcessing {
            queue_id,
            query: query.clone(),
            client_id: client_id.clone(),
        });

        // Execute the search and download initiation
        let result = self.execute_search_and_download(&queued).await;

        match result {
            Ok(item) => {
                // Check the item's download status to determine what to do
                match item.download_status.as_str() {
                    "completed" => {
                        // Download already completed (e.g., duplicate found)
                        // Mark search as completed and delete queue entry
                        if let Err(e) = self.db.mark_search_completed(queue_id, item.id).await {
                            tracing::error!("[QueueWorker] Failed to mark search as completed: {}", e);
                        }

                        // If this is part of a list, add item to list and update progress
                        if let (Some(list_id), Some(pos)) = (queued.list_id, queued.list_position) {
                            if let Err(e) = self.db.add_item_to_list(list_id, item.id, pos).await {
                                tracing::error!("[QueueWorker] Failed to add item {} to list {}: {}", item.id, list_id, e);
                            }
                            self.update_list_progress(list_id).await;
                        }

                        // Delete the completed queue entry
                        if let Err(e) = self.db.delete_completed_search(queue_id).await {
                            tracing::error!("[QueueWorker] Failed to delete completed search: {}", e);
                        }

                        // Add to search history
                        if let Err(e) = self.db.insert_search_history(
                            queued.user_id,
                            &query,
                            queued.original_artist.as_deref(),
                            queued.original_track.as_deref(),
                            queued.format.as_deref(),
                            "completed",
                            Some(item.id),
                            None,
                        ).await {
                            tracing::error!("[QueueWorker] Failed to insert search history: {}", e);
                        }

                        tracing::info!(
                            "[QueueWorker] Search completed (immediate): queue_id={}, item_id={}",
                            queue_id, item.id
                        );
                    }
                    "downloading" | "queued" => {
                        // Download initiated but not yet complete
                        // Keep queue entry - TransferMonitor will handle completion
                        // Just update the item_id reference
                        if let Err(e) = self.db.mark_search_completed(queue_id, item.id).await {
                            tracing::error!("[QueueWorker] Failed to update search with item_id: {}", e);
                        }
                        // NOTE: Do NOT delete queue entry - TransferMonitor will do it
                    }
                    _ => {
                        // Unexpected status - treat as error
                        tracing::warn!(
                            "[QueueWorker] Unexpected item status '{}' for queue_id={}",
                            item.download_status, queue_id
                        );
                    }
                }
            }
            Err(e) => {
                let error_msg = e.to_string();

                // Check if this is a "no results" error - don't retry these
                let is_no_results = error_msg.contains("No results found for search");

                // Apply retry logic based on retry_count (skip retry for "no results" errors)
                if retry_count == 0 && !is_no_results {
                    // First failure - mark for retry (moves to back of queue)
                    tracing::info!(
                        "[QueueWorker] Search failed (will retry): queue_id={}, error={}",
                        queue_id, error_msg
                    );

                    if let Err(e) = self.db.mark_search_for_retry(queue_id, &error_msg).await {
                        tracing::error!("[QueueWorker] Failed to mark search for retry: {}", e);
                    }

                    // Update list progress so UI reflects the retry is pending
                    if let Some(list_id) = queued.list_id {
                        self.update_list_progress(list_id).await;
                    }

                    // Broadcast retry queued (not a permanent failure)
                    self.broadcast(WsEvent::SearchFailed {
                        queue_id,
                        query: query.clone(),
                        error: format!("{} (will retry)", error_msg),
                        client_id: client_id.clone(),
                    });
                } else {
                    // Retry also failed - permanent failure, delete entry
                    tracing::warn!(
                        "[QueueWorker] Search failed permanently (retry exhausted): queue_id={}, error={}",
                        queue_id, error_msg
                    );

                    // Delete the queue entry
                    if let Err(e) = self.db.delete_search(queue_id).await {
                        tracing::error!("[QueueWorker] Failed to delete failed search: {}", e);
                    }

                    // Add to search history as failed
                    if let Err(e) = self.db.insert_search_history(
                        queued.user_id,
                        &query,
                        queued.original_artist.as_deref(),
                        queued.original_track.as_deref(),
                        queued.format.as_deref(),
                        "failed",
                        None,
                        Some(&error_msg),
                    ).await {
                        tracing::error!("[QueueWorker] Failed to insert search history: {}", e);
                    }

                    // If this is part of a list, create a placeholder failed item and update progress
                    if let Some(list_id) = queued.list_id {
                        let placeholder_filename = match (queued.original_artist.as_deref(), queued.original_track.as_deref()) {
                            (Some(artist), Some(track)) => format!("{} - {}", artist, track),
                            _ => query.clone(),
                        };

                        // Use a unique placeholder path to avoid UNIQUE constraint on file_path
                        let placeholder_path = format!("__failed_placeholder_{}", queue_id);

                        match self.db.create_item(
                            &placeholder_filename,
                            &query,
                            queued.original_artist.as_deref(),
                            queued.original_track.as_deref(),
                            &placeholder_path,
                            0,   // no file size
                            None, // no bitrate
                            None, // no duration
                            "",  // no extension
                            "",  // no source username
                        ).await {
                            Ok(item) => {
                                // Mark the placeholder as failed
                                if let Err(e) = self.db.update_item_status(
                                    item.id, "failed", 0.0, Some(&error_msg)
                                ).await {
                                    tracing::debug!("[QueueWorker] Failed to set placeholder item status: {}", e);
                                }
                                // Add to the list
                                if let Err(e) = self.db.add_item_to_list(
                                    list_id, item.id, queued.list_position.unwrap_or(0)
                                ).await {
                                    tracing::debug!("[QueueWorker] Failed to add placeholder item to list: {}", e);
                                }
                                tracing::debug!(
                                    "[QueueWorker] Created failed placeholder item {} for list {} ('{}')",
                                    item.id, list_id, placeholder_filename
                                );
                            }
                            Err(e) => {
                                tracing::warn!("[QueueWorker] Failed to create placeholder item for list {}: {}", list_id, e);
                            }
                        }

                        self.update_list_progress(list_id).await;
                    }

                    // Broadcast permanent failure
                    self.broadcast(WsEvent::SearchFailed {
                        queue_id,
                        query,
                        error: error_msg,
                        client_id,
                    });
                }
            }
        }
    }

    /// Execute the actual search and download for a queued entry
    ///
    /// This method:
    /// 1. Checks for duplicates (fuzzy matching)
    /// 2. Executes search (concurrent-safe, no exclusive lock needed)
    /// 3. Selects best file
    /// 4. Creates item in DB
    /// 5. Calls initiate_download (non-blocking)
    /// 6. Returns item (download continues in background via listener)
    async fn execute_search_and_download(&self, queued: &QueuedSearch) -> Result<Item> {
        let query = &queued.query;
        let format = queued.format.as_deref();
        let queue_id = queued.id;
        let list_id = queued.list_id;
        let list_position = queued.list_position;
        let client_id = queued.client_id.clone();
        let original_artist = queued.original_artist.clone();
        let original_track = queued.original_track.clone();

        // Broadcast search started
        self.broadcast(WsEvent::SearchStarted {
            item_id: 0,
            query: query.clone(),
            client_id: client_id.clone(),
        });

        // Check for duplicates (fuzzy matching against completed items)
        let fuzzy_matcher = FuzzyMatcher::new(70); // 70% similarity threshold
        let existing_items = self.db.get_all_items().await?;
        let completed_items: Vec<_> = existing_items.iter()
            .filter(|item| item.download_status == "completed")
            .cloned()
            .collect();

        if let Some(existing) = fuzzy_matcher.find_duplicate(query, &completed_items) {
            tracing::info!(
                "[QueueWorker] Found existing completed item matching query '{}': {}",
                query, existing.filename
            );
            self.broadcast(WsEvent::DuplicateFound {
                item_id: existing.id,
                filename: existing.filename.clone(),
                query: query.clone(),
                client_id: client_id.clone(),
            });
            return Ok(existing.clone());
        }

        // No lock needed - SoulseekClient methods take &self
        let client = &self.slsk_client;

        // Check for cached candidates from previous failed attempt
        let mut scored_file: Option<ScoredFile> = None;
        let mut used_cached = false;

        {
            let mut candidates = self.pending_candidates.write().await;
            if let Some(cached) = candidates.get_mut(&queue_id) {
                if !cached.is_empty() {
                    // Pop the first candidate (best remaining)
                    scored_file = Some(cached.remove(0));
                    used_cached = true;
                    tracing::info!(
                        "[QueueWorker] Using cached candidate #{} for queue_id={} ({} remaining)",
                        queued.retry_count + 1,
                        queue_id,
                        cached.len()
                    );
                }
            }
        }

        // If no cached candidates, do a fresh search
        if scored_file.is_none() {
            // Execute search
            let results = client.search(query, self.config.search_timeout_secs).await?;

            let total_files: usize = results.iter().map(|r| r.files.len()).sum();

            // Broadcast search progress
            self.broadcast(WsEvent::SearchProgress {
                item_id: 0,
                results_count: total_files,
                users_count: results.len(),
                client_id: client_id.clone(),
            });

            if results.is_empty() {
                self.broadcast(WsEvent::SearchCompleted {
                    item_id: 0,
                    results_count: 0,
                    selected_file: None,
                    selected_user: None,
                    client_id: client_id.clone(),
                });
                bail!("No results found for query: {}", query);
            }

            // Get ALL candidates sorted by score
            let mut all_candidates = SoulseekClient::get_best_files(&results, query, format);

            if all_candidates.is_empty() {
                bail!(if format.is_some() {
                    format!("No {} files found matching query", format.unwrap().to_uppercase())
                } else {
                    "No suitable files found matching query".to_string()
                });
            }

            // Take the first (best) candidate
            let selected = all_candidates.remove(0);

            // Broadcast search completed with actual results count
            self.broadcast(WsEvent::SearchCompleted {
                item_id: 0,
                results_count: total_files,
                selected_file: Some(extract_basename(&selected.filename).to_string()),
                selected_user: Some(selected.username.clone()),
                client_id: client_id.clone(),
            });

            scored_file = Some(selected);

            // Cache remaining candidates for retry (limit to top 5 alternatives)
            let candidates_to_cache: Vec<ScoredFile> = all_candidates.into_iter().take(5).collect();
            let cached_count = candidates_to_cache.len();
            if !candidates_to_cache.is_empty() {
                self.pending_candidates.write().await.insert(queue_id, candidates_to_cache);
            }

            let sf = scored_file.as_ref().unwrap();
            tracing::info!(
                "[QueueWorker] Selected '{}' from '{}' (score={:.1}, {:?} kbps) [{} alternatives cached]",
                sf.filename, sf.username, sf.score, sf.bitrate, cached_count
            );
        }

        let scored_file = scored_file.unwrap();

        // Prepare file info
        let remote_filename = scored_file.filename.clone();
        let local_filename = extract_basename(&remote_filename).to_string();
        let username = scored_file.username.clone();
        let extension = Path::new(&local_filename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let file_path = self.config.storage_path.join(&local_filename);

        // Check for existing item with same file_path (retry scenario)
        if let Some(existing) = self.db.find_item_by_path(file_path.to_str().unwrap()).await? {
            if existing.download_status != "completed" {
                tracing::debug!("[QueueWorker] Replacing existing {} item id={} for retry", existing.download_status, existing.id);
                if let Some(lid) = list_id {
                    let _ = self.db.remove_item_from_list(lid, existing.id).await;
                }
                self.db.delete_item(existing.id).await?;
            } else {
                tracing::debug!("[QueueWorker] Found existing completed item: {}", existing.filename);
                return Ok(existing);
            }
        }

        // Create item in database
        let mut item = self.db.create_item(
            &local_filename,
            query,
            original_artist.as_deref(),
            original_track.as_deref(),
            file_path.to_str().unwrap(),
            scored_file.size as i64,
            scored_file.bitrate.map(|b| b as i32),
            None, // duration
            &extension,
            &username,
        ).await?;

        // Update status to downloading
        self.db.update_item_status(item.id, "downloading", 0.0, None).await?;

        // If this is part of a list, associate the item with the list now
        // (this captures both successful and failed downloads)
        if let (Some(lid), Some(pos)) = (list_id, list_position) {
            if let Err(e) = self.db.add_item_to_list(lid, item.id, pos).await {
                tracing::error!("[QueueWorker] Failed to add item {} to list {}: {}", item.id, lid, e);
            }
        }

        // Broadcast download started
        self.broadcast(WsEvent::DownloadStarted {
            item_id: item.id,
            filename: local_filename.clone(),
            total_bytes: scored_file.size,
            client_id: client_id.clone(),
        });
        self.broadcast(WsEvent::ItemUpdated {
            item_id: item.id,
            filename: local_filename.clone(),
            status: "downloading".to_string(),
            progress: 0.0,
        });

        // Create progress callback
        let item_id = item.id;
        let ws_tx = self.ws_broadcast.clone();
        let client_id_for_progress = client_id.clone();
        let progress_callback: ArcProgressCallback = Arc::new(move |bytes_downloaded, total_bytes, speed_kbps| {
            let progress_pct = if total_bytes > 0 {
                (bytes_downloaded as f64 / total_bytes as f64) * 100.0
            } else {
                0.0
            };
            let _ = ws_tx.send(WsEvent::DownloadProgress {
                item_id,
                bytes_downloaded,
                total_bytes,
                progress_pct,
                speed_kbps,
                client_id: client_id_for_progress.clone(),
            });
        });

        // Get completion sender
        let completion_tx = self.transfer_completion_tx.clone();

        // Create protocol-level completion sender
        let (proto_tx, mut proto_rx) = mpsc::channel::<crate::protocol::TransferComplete>(1);

        // Spawn a task to bridge protocol completion to service completion
        let item_id_for_bridge = item.id;
        let queue_id_for_bridge = queue_id;
        let list_id_for_bridge = list_id;
        let list_position_for_bridge = list_position;
        let client_id_for_bridge = client_id.clone();
        let completion_tx_clone = completion_tx.clone();
        tokio::spawn(async move {
            if let Some(proto_completion) = proto_rx.recv().await {
                let service_completion = TransferCompletion {
                    item_id: item_id_for_bridge,
                    queue_id: queue_id_for_bridge,
                    list_id: list_id_for_bridge,
                    list_position: list_position_for_bridge,
                    result: proto_completion.result,
                    client_id: client_id_for_bridge,
                };
                let _ = completion_tx_clone.send(service_completion).await;
            }
        });

        // Initiate download (non-blocking)
        let download_result = client.initiate_download(
            &username,
            &remote_filename,
            scored_file.size,
            &scored_file.peer_ip,
            scored_file.peer_port,
            &file_path,
            item.id,
            queue_id,
            list_id,
            Some(progress_callback),
            proto_tx,
        ).await;

        match download_result {
            DownloadInitResult::TransferSpawned { item_id: _, transfer_token: _ } => {
                item.download_status = "downloading".to_string();
                Ok(item)
            }
            DownloadInitResult::Queued { transfer_token, position, reason } => {
                tracing::info!(
                    "[QueueWorker] Download queued by peer: token={}, position={:?}, reason={}",
                    transfer_token, position, reason
                );

                // Update status to queued
                self.db.update_item_status(item.id, "queued", 0.0, None).await?;
                item.download_status = "queued".to_string();

                self.broadcast(WsEvent::DownloadQueued {
                    item_id: item.id,
                    position,
                    reason: reason.clone(),
                    client_id: client_id.clone(),
                });
                self.broadcast(WsEvent::ItemUpdated {
                    item_id: item.id,
                    filename: local_filename.clone(),
                    status: "queued".to_string(),
                    progress: 0.0,
                });

                Ok(item)
            }
            DownloadInitResult::Failed { reason } => {
                tracing::warn!("[QueueWorker] Download initiation failed: {}", reason);
                self.db.update_item_status(item.id, "failed", 0.0, Some(&reason)).await?;
                item.download_status = "failed".to_string();

                self.broadcast(WsEvent::DownloadFailed {
                    item_id: item.id,
                    error: reason.clone(),
                    client_id,
                });

                bail!("Download failed: {}", reason);
            }
        }
    }

    /// Update list progress after a search completes or fails
    ///
    /// Uses a robust check: instead of comparing against `total_items` (which can drift
    /// due to retries/removals), we check whether there are any non-terminal items in the
    /// list AND any pending searches in the queue for this list. Only when both are zero
    /// do we transition to a final status.
    async fn update_list_progress(&self, list_id: i64) {
        // Get the list for display total
        let list = match self.db.get_list(list_id).await {
            Ok(Some(list)) => list,
            Ok(None) => {
                tracing::error!("[QueueWorker] List {} not found", list_id);
                return;
            }
            Err(e) => {
                tracing::error!("[QueueWorker] Failed to get list {}: {}", list_id, e);
                return;
            }
        };

        // Count completed and failed items from list_items table
        let (completed, failed, total_in_list) = match self.db.count_list_items_by_status(list_id).await {
            Ok(counts) => counts,
            Err(e) => {
                tracing::error!("[QueueWorker] Failed to count list items: {}", e);
                return;
            }
        };

        // Items in non-terminal states (downloading, queued, pending)
        let unresolved_items = total_in_list - completed - failed;

        // Pending/processing/retry searches still in the queue for this list
        let pending_searches = self.db.count_pending_searches_for_list(list_id).await.unwrap_or(0);

        // Determine list status
        let status = if unresolved_items == 0 && pending_searches == 0 {
            // All work is done — determine final status
            if completed == 0 && failed == 0 {
                "pending"
            } else if failed == 0 {
                "completed"
            } else if completed == 0 {
                "failed"
            } else {
                "partial"
            }
        } else {
            "downloading"
        };

        // Use the larger of total_items and actual items for display
        let display_total = std::cmp::max(list.total_items, total_in_list);

        // Update list
        if let Err(e) = self.db.update_list_status(list_id, status, completed, failed).await {
            tracing::error!("[QueueWorker] Failed to update list status: {}", e);
        }

        // Broadcast list progress
        self.broadcast(WsEvent::ListProgress {
            list_id,
            completed,
            failed,
            total: display_total,
            status: status.to_string(),
        });
    }

    /// Handle a transfer completion notification
    async fn handle_transfer_completion(&self, completion: TransferCompletion) {
        let TransferCompletion { item_id, queue_id, list_id, list_position: _, result, client_id } = completion;

        // Get the queued search data before we delete it (needed for history)
        let queued = match self.db.get_queued_search(queue_id).await {
            Ok(Some(q)) => Some(q),
            Ok(None) => {
                tracing::warn!("[TransferMonitor] Queue entry {} not found", queue_id);
                None
            }
            Err(e) => {
                tracing::error!("[TransferMonitor] Failed to get queue entry: {}", e);
                None
            }
        };

        match result {
            Ok(bytes) => {
                tracing::info!(
                    "[TransferMonitor] Completed: item_id={}, {:.2} MB",
                    item_id, bytes as f64 / 1_048_576.0
                );

                // Update item status to completed
                if let Err(e) = self.db.update_item_status(item_id, "completed", 1.0, None).await {
                    tracing::error!("[TransferMonitor] Failed to update item status: {}", e);
                }

                // Note: Item was already added to list_items at creation time
                // (in execute_search_and_download), so we don't need to add it here

                // Read audio properties (sample rate, bit depth) from the downloaded file
                if let Ok(Some(ref item)) = self.db.get_item(item_id).await {
                    if !item.file_path.is_empty() {
                        match crate::services::metadata::tags::read_audio_properties(&item.file_path) {
                            Ok(props) => {
                                if let Err(e) = self.db.update_item_audio_properties(
                                    item_id, props.sample_rate, props.bit_depth
                                ).await {
                                    tracing::error!("[TransferMonitor] Failed to update audio properties: {}", e);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("[TransferMonitor] Failed to read audio props for item {}: {}", item_id, e);
                            }
                        }
                    }
                }

                // Add newly downloaded file to share index
                if let Ok(Some(ref item)) = self.db.get_item(item_id).await {
                    if !item.file_path.is_empty() {
                        let file_path = std::path::Path::new(&item.file_path);
                        if let Err(e) = self.sharing_service.add_file(file_path).await {
                            tracing::warn!("[TransferMonitor] Failed to add file to share index: {}", e);
                        }
                    }
                }

                // Get item info for broadcast
                if let Ok(Some(item)) = self.db.get_item(item_id).await {
                    self.broadcast(WsEvent::DownloadCompleted {
                        item_id,
                        filename: item.filename.clone(),
                        total_bytes: bytes,
                        client_id: client_id.clone(),
                    });
                    self.broadcast(WsEvent::ItemUpdated {
                        item_id,
                        filename: item.filename.clone(),
                        status: "completed".to_string(),
                        progress: 1.0,
                    });

                    // Spawn metadata lookup as background task (non-blocking)
                    let metadata_service: Arc<crate::services::MetadataService> = Arc::clone(&self.metadata_service);
                    let file_path = item.file_path.clone();
                    let original_query = item.original_query.clone();
                    let original_artist = item.original_artist.clone();
                    let original_track = item.original_track.clone();
                    let ws_broadcast = self.ws_broadcast.clone();

                    tokio::spawn(async move {
                        let _ = ws_broadcast.send(WsEvent::MetadataLookupStarted { item_id });

                        match metadata_service
                            .lookup_and_store(
                                item_id,
                                &file_path,
                                &original_query,
                                original_artist.as_deref(),
                                original_track.as_deref(),
                            )
                            .await
                        {
                            Ok(metadata) => {
                                tracing::debug!(
                                    "[Metadata] Lookup complete for item_id={}: artist={:?}, bpm={:?}",
                                    item_id, metadata.artist, metadata.bpm
                                );
                                let _ = ws_broadcast.send(WsEvent::MetadataUpdated {
                                    item_id,
                                    metadata: Some(metadata),
                                    error: None,
                                });
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "[MetadataService] Metadata lookup failed for item_id={}: {}",
                                    item_id,
                                    e
                                );
                                let _ = ws_broadcast.send(WsEvent::MetadataUpdated {
                                    item_id,
                                    metadata: None,
                                    error: Some(e.to_string()),
                                });
                            }
                        }
                    });
                }

                // Delete the queue entry now that transfer is complete
                if let Err(e) = self.db.delete_search(queue_id).await {
                    tracing::error!("[TransferMonitor] Failed to delete completed queue entry: {}", e);
                }

                // Clean up cached candidates (no longer needed)
                self.pending_candidates.write().await.remove(&queue_id);

                // Add to search history
                if let Some(ref q) = queued {
                    if let Err(e) = self.db.insert_search_history(
                        q.user_id,
                        &q.query,
                        q.original_artist.as_deref(),
                        q.original_track.as_deref(),
                        q.format.as_deref(),
                        "completed",
                        Some(item_id),
                        None,
                    ).await {
                        tracing::error!("[TransferMonitor] Failed to insert search history: {}", e);
                    }
                }

                // Update list progress if applicable
                if let Some(list_id) = list_id {
                    self.update_list_progress(list_id).await;
                }
            }
            Err(error_msg) => {
                tracing::warn!(
                    "[TransferMonitor] Failed: item_id={}, queue_id={}, error={}",
                    item_id, queue_id, error_msg
                );

                // Update item status to failed
                if let Err(e) = self.db.update_item_status(
                    item_id, "failed", 0.0, Some(&error_msg)
                ).await {
                    tracing::error!("[TransferMonitor] Failed to update item status: {}", e);
                }

                // Update list progress immediately so the UI reflects the failure,
                // even if this item will be retried (a later success will correct the counts)
                if let Some(list_id) = list_id {
                    self.update_list_progress(list_id).await;
                }

                // Get retry_count from pre-fetched queued search
                let retry_count = queued.as_ref().map(|q| q.retry_count).unwrap_or(1);

                // Get item info for broadcast
                let item_filename = if let Ok(Some(item)) = self.db.get_item(item_id).await {
                    let filename = item.filename.clone();
                    self.broadcast(WsEvent::ItemUpdated {
                        item_id,
                        filename: item.filename,
                        status: "failed".to_string(),
                        progress: 0.0,
                    });
                    filename
                } else {
                    String::new()
                };

                // Apply retry logic
                if retry_count == 0 {
                    // First failure - mark for retry
                    tracing::info!("[TransferMonitor] Will retry: queue_id={}", queue_id);

                    if let Err(e) = self.db.mark_search_for_retry(queue_id, &error_msg).await {
                        tracing::error!("[TransferMonitor] Failed to mark for retry: {}", e);
                    }

                    // Broadcast failure with retry note
                    self.broadcast(WsEvent::DownloadFailed {
                        item_id,
                        error: format!("{} (will retry)", error_msg),
                        client_id: client_id.clone(),
                    });
                } else {
                    // Check if we have more cached candidates to try
                    let has_more_candidates = {
                        let candidates = self.pending_candidates.read().await;
                        candidates.get(&queue_id).map(|c| !c.is_empty()).unwrap_or(false)
                    };

                    if has_more_candidates {
                        // We have more candidates - mark for retry to try next one
                        tracing::info!("[TransferMonitor] Trying next cached candidate: queue_id={}", queue_id);

                        if let Err(e) = self.db.mark_search_for_retry(queue_id, &error_msg).await {
                            tracing::error!("[TransferMonitor] Failed to mark for retry: {}", e);
                        }

                        // Broadcast failure with retry note
                        self.broadcast(WsEvent::DownloadFailed {
                            item_id,
                            error: format!("{} (trying next candidate)", error_msg),
                            client_id: client_id.clone(),
                        });
                    } else {
                        // No more candidates - permanent failure
                        tracing::warn!("[TransferMonitor] No more candidates, permanent failure: queue_id={}", queue_id);

                        // Clean up cached candidates
                        self.pending_candidates.write().await.remove(&queue_id);

                        // Delete the queue entry
                        if let Err(e) = self.db.delete_search(queue_id).await {
                            tracing::error!("[TransferMonitor] Failed to delete failed queue entry: {}", e);
                        }

                        // Add to search history as failed
                        if let Some(ref q) = queued {
                            if let Err(e) = self.db.insert_search_history(
                                q.user_id,
                                &q.query,
                                q.original_artist.as_deref(),
                                q.original_track.as_deref(),
                                q.format.as_deref(),
                                "failed",
                                Some(item_id),
                                Some(&error_msg),
                            ).await {
                                tracing::error!("[TransferMonitor] Failed to insert search history: {}", e);
                            }
                        }

                        // Broadcast permanent failure
                        self.broadcast(WsEvent::DownloadFailed {
                            item_id,
                            error: error_msg,
                            client_id,
                        });
                    }
                }
            }
        }
    }
}
