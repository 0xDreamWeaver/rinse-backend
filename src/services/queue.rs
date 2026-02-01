//! Queue service for managing search queue and spawned file transfers
//!
//! This service provides:
//! - Enqueueing of single searches and lists
//! - A queue worker that processes searches sequentially (Soulseek server limitation)
//! - A transfer monitor that handles completion of spawned file transfers
//!
//! Architecture:
//! ```text
//! Client submits search/list
//!          ↓
//!     Search Queue (DB)
//!          ↓
//!     Queue Worker (sequential)
//!          ↓
//!     Search → Negotiate → Get FileConnection
//!          ↓
//!     Spawn file transfer (independent task)
//!          ↓
//!     Transfer Monitor ← completion notification
//!          ↓
//!     Update DB + Broadcast WebSocket
//! ```

use std::sync::Arc;
use std::time::Duration;
use std::path::{Path, PathBuf};
use tokio::sync::{broadcast, mpsc, RwLock};
use tokio::task::JoinHandle;
use anyhow::{Result, bail};

use crate::db::Database;
use crate::models::{QueuedSearch, List, Item, QueueStatus, QueueStatusResponse};
use crate::protocol::{SoulseekClient, DownloadInitResult, ArcProgressCallback};
use crate::api::WsEvent;
use crate::services::FuzzyMatcher;

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
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 500,
            search_timeout_secs: 10,
            storage_path: PathBuf::from("storage"),
        }
    }
}

/// Queue service for managing search queue and file transfers
pub struct QueueService {
    db: Database,
    slsk_client: Arc<RwLock<Option<SoulseekClient>>>,
    ws_broadcast: broadcast::Sender<WsEvent>,
    config: QueueConfig,
    /// Channel for receiving transfer completion notifications
    transfer_completion_tx: mpsc::Sender<TransferCompletion>,
    transfer_completion_rx: Arc<tokio::sync::Mutex<mpsc::Receiver<TransferCompletion>>>,
}

impl QueueService {
    /// Create a new queue service
    pub fn new(
        db: Database,
        slsk_client: Arc<RwLock<Option<SoulseekClient>>>,
        ws_broadcast: broadcast::Sender<WsEvent>,
        config: QueueConfig,
    ) -> Self {
        let (tx, rx) = mpsc::channel::<TransferCompletion>(100);

        Self {
            db,
            slsk_client,
            ws_broadcast,
            config,
            transfer_completion_tx: tx,
            transfer_completion_rx: Arc::new(tokio::sync::Mutex::new(rx)),
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
    pub async fn enqueue_search(
        &self,
        user_id: i64,
        query: &str,
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
        let queued = self.db.enqueue_search(user_id, query, format, None, None, client_id).await?;
        let position = self.db.get_queue_position(queued.id).await?;

        tracing::info!(
            "[QueueService] Enqueued search: id={}, query='{}', position={}, client_id={:?}",
            queued.id, query, position, client_id
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
    pub async fn enqueue_list(
        &self,
        user_id: i64,
        name: Option<String>,
        queries: Vec<String>,
        format: Option<String>,
    ) -> Result<(List, Vec<QueuedSearch>)> {
        let list_name = name.unwrap_or_else(|| {
            chrono::Utc::now().format("List %Y-%m-%d %H:%M").to_string()
        });

        // Create the list
        let list = self.db.create_list(&list_name, user_id, queries.len() as i32).await?;

        tracing::info!(
            "[QueueService] Created list: id={}, name='{}', items={}",
            list.id, list_name, queries.len()
        );

        // Enqueue all searches
        let queued_searches = self.db.enqueue_searches_for_list(
            user_id,
            &queries,
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
            total_items: queries.len() as i32,
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

    // ========================================================================
    // Worker and monitor startup
    // ========================================================================

    /// Start the queue worker
    ///
    /// The worker processes searches sequentially (Soulseek server limitation).
    /// It pulls from the queue, executes searches, and spawns file transfers.
    ///
    /// Returns a JoinHandle that can be used to monitor/abort the worker.
    pub fn start_worker(self: &Arc<Self>) -> JoinHandle<()> {
        let service = Arc::clone(self);

        tokio::spawn(async move {
            tracing::info!("[QueueWorker] Started");

            loop {
                // Check if connected to Soulseek
                let connected = {
                    let client_guard = service.slsk_client.read().await;
                    client_guard.is_some()
                };

                if !connected {
                    // Not connected, wait and retry
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }

                // Try to get next pending search
                match service.db.get_next_pending_search().await {
                    Ok(Some(queued)) => {
                        service.process_queued_search(queued).await;
                    }
                    Ok(None) => {
                        // No pending searches, wait before polling again
                        tokio::time::sleep(Duration::from_millis(service.config.poll_interval_ms)).await;
                    }
                    Err(e) => {
                        tracing::error!("[QueueWorker] Error getting next search: {}", e);
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
            "[QueueWorker] Processing search: id={}, query='{}', retry_count={}, client_id={:?}",
            queue_id, query, retry_count, client_id
        );

        // Mark as processing
        if let Err(e) = self.db.mark_search_processing(queue_id).await {
            tracing::error!("[QueueWorker] Failed to mark search as processing: {}", e);
            return;
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
                                tracing::error!(
                                    "[QueueWorker] Failed to add item {} to list {}: {}",
                                    item.id, list_id, e
                                );
                            } else {
                                tracing::info!(
                                    "[QueueWorker] Added item {} to list {} at position {}",
                                    item.id, list_id, pos
                                );
                            }
                            self.update_list_progress(list_id).await;
                        }

                        // Delete the completed queue entry
                        if let Err(e) = self.db.delete_completed_search(queue_id).await {
                            tracing::error!("[QueueWorker] Failed to delete completed search: {}", e);
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

                        tracing::info!(
                            "[QueueWorker] Download initiated (in progress): queue_id={}, item_id={}, status={}",
                            queue_id, item.id, item.download_status
                        );
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

                    // If this is part of a list, update list progress
                    if let Some(list_id) = queued.list_id {
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
    /// 2. Acquires slsk_client lock
    /// 3. Executes search
    /// 4. Selects best file
    /// 5. Creates item in DB
    /// 6. Calls initiate_download (non-blocking)
    /// 7. Returns item (download continues in background via listener)
    async fn execute_search_and_download(&self, queued: &QueuedSearch) -> Result<Item> {
        let query = &queued.query;
        let format = queued.format.as_deref();
        let queue_id = queued.id;
        let list_id = queued.list_id;
        let list_position = queued.list_position;
        let client_id = queued.client_id.clone();

        tracing::info!(
            "[QueueWorker] Executing search for: '{}' (format: {:?}, client_id: {:?})",
            query, format, client_id
        );

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

        // Acquire Soulseek client lock
        let mut client_guard = self.slsk_client.write().await;
        let client = client_guard.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Not connected to Soulseek"))?;

        // Execute search
        tracing::info!("[QueueWorker] Sending search query to Soulseek network...");
        let results = client.search(query, self.config.search_timeout_secs).await?;

        let total_files: usize = results.iter().map(|r| r.files.len()).sum();
        tracing::info!(
            "[QueueWorker] Search completed: {} users returned {} files",
            results.len(), total_files
        );

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

        // Select best file
        let scored_file = SoulseekClient::get_best_file(&results, query, format)
            .ok_or_else(|| {
                if format.is_some() {
                    anyhow::anyhow!("No {} files found matching query", format.unwrap().to_uppercase())
                } else {
                    anyhow::anyhow!("No suitable files found matching query")
                }
            })?;

        tracing::info!(
            "[QueueWorker] Selected '{}' from '{}' (score={:.1}, {:?} kbps)",
            scored_file.filename,
            scored_file.username,
            scored_file.score,
            scored_file.bitrate
        );

        // Broadcast search completed
        self.broadcast(WsEvent::SearchCompleted {
            item_id: 0,
            results_count: total_files,
            selected_file: Some(extract_basename(&scored_file.filename).to_string()),
            selected_user: Some(scored_file.username.clone()),
            client_id: client_id.clone(),
        });

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
                tracing::info!(
                    "[QueueWorker] Found existing {} item for same file_path, deleting for retry: id={}",
                    existing.download_status, existing.id
                );
                self.db.delete_item(existing.id).await?;
            } else {
                tracing::info!(
                    "[QueueWorker] Found existing completed item for same file_path: {}",
                    existing.filename
                );
                return Ok(existing);
            }
        }

        // Create item in database
        let mut item = self.db.create_item(
            &local_filename,
            query,
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
                tracing::error!(
                    "[QueueWorker] Failed to add item {} to list {}: {}",
                    item.id, lid, e
                );
            } else {
                tracing::info!(
                    "[QueueWorker] Added item {} to list {} at position {}",
                    item.id, lid, pos
                );
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
            DownloadInitResult::TransferSpawned { item_id: _, transfer_token } => {
                tracing::info!(
                    "[QueueWorker] Download initiated, transfer pending (token={})",
                    transfer_token
                );
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
    async fn update_list_progress(&self, list_id: i64) {
        // Get the list to know total_items
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

        let total = list.total_items;

        // Count completed and failed items from list_items table
        let (completed, failed, _items_in_list) = match self.db.count_list_items_by_status(list_id).await {
            Ok(counts) => counts,
            Err(e) => {
                tracing::error!("[QueueWorker] Failed to count list items: {}", e);
                return;
            }
        };

        // Determine list status
        let status = if completed + failed >= total {
            if failed == 0 {
                "completed"
            } else if completed == 0 {
                "failed"
            } else {
                "partial"
            }
        } else {
            "downloading"
        };

        // Update list
        if let Err(e) = self.db.update_list_status(list_id, status, completed, failed).await {
            tracing::error!("[QueueWorker] Failed to update list status: {}", e);
        }

        // Broadcast list progress
        self.broadcast(WsEvent::ListProgress {
            list_id,
            completed,
            failed,
            total,
            status: status.to_string(),
        });
    }

    /// Handle a transfer completion notification
    async fn handle_transfer_completion(&self, completion: TransferCompletion) {
        let TransferCompletion { item_id, queue_id, list_id, list_position: _, result, client_id } = completion;

        match result {
            Ok(bytes) => {
                tracing::info!(
                    "[TransferMonitor] Transfer completed: item_id={}, queue_id={}, bytes={}, client_id={:?}",
                    item_id, queue_id, bytes, client_id
                );

                // Update item status to completed
                if let Err(e) = self.db.update_item_status(item_id, "completed", 1.0, None).await {
                    tracing::error!("[TransferMonitor] Failed to update item status: {}", e);
                }

                // Note: Item was already added to list_items at creation time
                // (in execute_search_and_download), so we don't need to add it here

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
                        filename: item.filename,
                        status: "completed".to_string(),
                        progress: 1.0,
                    });
                }

                // Delete the queue entry now that transfer is complete
                if let Err(e) = self.db.delete_search(queue_id).await {
                    tracing::error!("[TransferMonitor] Failed to delete completed queue entry: {}", e);
                }

                // Update list progress if applicable
                if let Some(list_id) = list_id {
                    self.update_list_progress(list_id).await;
                }
            }
            Err(error_msg) => {
                tracing::warn!(
                    "[TransferMonitor] Transfer failed: item_id={}, queue_id={}, error={}, client_id={:?}",
                    item_id, queue_id, error_msg, client_id
                );

                // Update item status to failed
                if let Err(e) = self.db.update_item_status(
                    item_id, "failed", 0.0, Some(&error_msg)
                ).await {
                    tracing::error!("[TransferMonitor] Failed to update item status: {}", e);
                }

                // Get the queue entry to check retry_count
                let retry_count = match self.db.get_queued_search(queue_id).await {
                    Ok(Some(queued)) => queued.retry_count,
                    Ok(None) => {
                        tracing::warn!("[TransferMonitor] Queue entry {} not found, cannot retry", queue_id);
                        1 // Treat as already retried to avoid infinite loops
                    }
                    Err(e) => {
                        tracing::error!("[TransferMonitor] Failed to get queue entry: {}", e);
                        1 // Treat as already retried
                    }
                };

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
                    tracing::info!(
                        "[TransferMonitor] Transfer failed (will retry): queue_id={}, item_id={}",
                        queue_id, item_id
                    );

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
                    // Retry also failed - permanent failure
                    tracing::warn!(
                        "[TransferMonitor] Transfer failed permanently (retry exhausted): queue_id={}, item_id={}",
                        queue_id, item_id
                    );

                    // Delete the queue entry
                    if let Err(e) = self.db.delete_search(queue_id).await {
                        tracing::error!("[TransferMonitor] Failed to delete failed queue entry: {}", e);
                    }

                    // Broadcast permanent failure
                    self.broadcast(WsEvent::DownloadFailed {
                        item_id,
                        error: error_msg,
                        client_id,
                    });

                    // Update list progress if applicable
                    if let Some(list_id) = list_id {
                        self.update_list_progress(list_id).await;
                    }
                }
            }
        }
    }
}
