use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast, Semaphore};
use anyhow::{Result, bail, Context};

use crate::db::Database;
use crate::models::{Item, List, DownloadStatus};
use crate::protocol::{SoulseekClient, DownloadResult};
use crate::services::FuzzyMatcher;
use crate::api::WsEvent;

/// Extract the basename from a Soulseek path.
/// Soulseek paths use Windows-style backslashes, so we need to handle both \ and /
fn extract_basename(soulseek_path: &str) -> &str {
    // Find the last occurrence of either \ or /
    let last_sep = soulseek_path
        .rfind(|c| c == '\\' || c == '/')
        .map(|i| i + 1)
        .unwrap_or(0);
    &soulseek_path[last_sep..]
}

/// Download service that manages file downloads from Soulseek
pub struct DownloadService {
    db: Database,
    slsk_client: Arc<RwLock<Option<SoulseekClient>>>,
    storage_path: PathBuf,
    fuzzy_matcher: FuzzyMatcher,
    ws_broadcast: broadcast::Sender<WsEvent>,
}

impl DownloadService {
    /// Create a new download service
    pub fn new(db: Database, storage_path: PathBuf, ws_broadcast: broadcast::Sender<WsEvent>) -> Self {
        Self {
            db,
            slsk_client: Arc::new(RwLock::new(None)),
            storage_path,
            fuzzy_matcher: FuzzyMatcher::new(70), // 70% similarity threshold
            ws_broadcast,
        }
    }

    /// Send a WebSocket event (ignores errors if no clients connected)
    fn broadcast(&self, event: WsEvent) {
        let _ = self.ws_broadcast.send(event);
    }

    /// Connect to Soulseek network
    pub async fn connect(&self, username: &str, password: &str) -> Result<()> {
        let client = SoulseekClient::connect(username, password).await?;
        *self.slsk_client.write().await = Some(client);
        tracing::info!("Connected to Soulseek network as {}", username);
        Ok(())
    }

    /// Check if connected to Soulseek
    pub async fn is_connected(&self) -> bool {
        self.slsk_client.read().await.is_some()
    }

    /// Search for a single item and download it
    /// format: Optional format filter (mp3, flac, m4a, wav)
    /// search_timeout_secs: Search timeout in seconds (default 10, use 5 for batch processing)
    pub async fn search_and_download_item(&self, query: &str, user_id: i64, format: Option<&str>) -> Result<Item> {
        self.search_and_download_item_with_timeout(query, user_id, format, 10).await
    }

    /// Search and download with configurable timeout
    pub async fn search_and_download_item_with_timeout(&self, query: &str, user_id: i64, format: Option<&str>, search_timeout_secs: u64) -> Result<Item> {
        tracing::info!("[DownloadService] Starting search for: '{}' (format: {:?})", query, format);

        // Broadcast search started (item_id 0 means not yet created)
        self.broadcast(WsEvent::SearchStarted {
            item_id: 0,
            query: query.to_string(),
        });

        // Check if we already have this item (fuzzy match) - only consider COMPLETED items
        let existing_items = self.db.get_all_items().await?;
        let completed_items: Vec<_> = existing_items.iter()
            .filter(|item| item.download_status == "completed")
            .cloned()
            .collect();
        tracing::debug!("[DownloadService] Checking {} completed items for duplicates (out of {} total)",
            completed_items.len(), existing_items.len());

        if let Some(existing) = self.fuzzy_matcher.find_duplicate(query, &completed_items) {
            tracing::info!("[DownloadService] Found existing completed item matching query '{}': {}", query, existing.filename);
            // Broadcast that we found an existing item (not a new search)
            self.broadcast(WsEvent::DuplicateFound {
                item_id: existing.id,
                filename: existing.filename.clone(),
                query: query.to_string(),
            });
            return Ok(existing.clone());
        }

        // Get Soulseek client (need write lock for search/download)
        let mut client_guard = self.slsk_client.write().await;
        let client = client_guard.as_mut()
            .ok_or_else(|| anyhow::anyhow!("Not connected to Soulseek"))?;

        tracing::info!("[DownloadService] Sending search query to Soulseek network...");

        // Search for the item with configurable timeout
        let results = client.search(query, search_timeout_secs).await?;

        // Count total files across all results
        let total_files: usize = results.iter().map(|r| r.files.len()).sum();
        tracing::info!("[DownloadService] Search completed. Results: {} users returned {} files", results.len(), total_files);

        // Broadcast search progress
        self.broadcast(WsEvent::SearchProgress {
            item_id: 0,
            results_count: total_files,
            users_count: results.len(),
        });

        if results.is_empty() {
            tracing::warn!("[DownloadService] No results found for query: '{}'", query);
            self.broadcast(WsEvent::SearchCompleted {
                item_id: 0,
                results_count: 0,
                selected_file: None,
                selected_user: None,
            });
            bail!("No results found for query: {}", query);
        }

        // Log result details
        for (i, result) in results.iter().enumerate() {
            tracing::info!(
                "[DownloadService] Result #{}: User '{}' - {} files, slots: {}, speed: {} KB/s",
                i + 1,
                result.username,
                result.files.len(),
                result.has_slots,
                result.avg_speed / 1024
            );
            for file in &result.files {
                tracing::debug!(
                    "[DownloadService]   - {} ({} bytes, bitrate: {:?})",
                    file.filename,
                    file.size,
                    file.bitrate()
                );
            }
        }

        // Get best file using sophisticated scoring with format filter
        let scored_file = SoulseekClient::get_best_file(&results, query, format)
            .ok_or_else(|| {
                if format.is_some() {
                    anyhow::anyhow!("No {} files found matching query", format.unwrap().to_uppercase())
                } else {
                    anyhow::anyhow!("No suitable files found matching query")
                }
            })?;

        tracing::info!(
            "[DownloadService] Selected '{}' from '{}' (score={:.1}, {:?} kbps)",
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
        });

        // Create item record in database
        // Use basename for local storage (Soulseek paths include full peer directory structure)
        let remote_filename = scored_file.filename.clone(); // Full path needed for download request
        let local_filename = extract_basename(&remote_filename).to_string(); // Just the file name
        let username = scored_file.username.clone();
        let extension = Path::new(&local_filename)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let file_path = self.storage_path.join(&local_filename);

        // Check if there's an existing item with this file_path that is failed/pending/queued
        // If so, delete it first to allow retry (file_path has UNIQUE constraint)
        if let Some(existing) = self.db.find_item_by_path(file_path.to_str().unwrap()).await? {
            if existing.download_status != "completed" {
                tracing::info!(
                    "[DownloadService] Found existing {} item for same file_path, deleting to allow retry: id={}",
                    existing.download_status, existing.id
                );
                self.db.delete_item(existing.id).await?;
            } else {
                // Existing completed item - return it
                tracing::info!("[DownloadService] Found existing completed item for same file_path: {}", existing.filename);
                return Ok(existing);
            }
        }

        let mut item = self.db.create_item(
            &local_filename,
            query,
            file_path.to_str().unwrap(),
            scored_file.size as i64,
            scored_file.bitrate.map(|b| b as i32),
            None, // duration not available from ScoredFile
            &extension,
            &username,
        ).await?;

        // Update status to downloading
        self.db.update_item_status(item.id, "downloading", 0.0, None).await?;

        // Broadcast download started
        self.broadcast(WsEvent::DownloadStarted {
            item_id: item.id,
            filename: local_filename.clone(),
            total_bytes: scored_file.size,
        });

        // Broadcast item update
        self.broadcast(WsEvent::ItemUpdated {
            item_id: item.id,
            filename: local_filename.clone(),
            status: "downloading".to_string(),
            progress: 0.0,
        });

        // Download the file (with retry on next best file)
        // Note: We pass remote_filename (full Soulseek path) for the download request,
        // but save to file_path which uses local_filename (basename only)

        // Create progress callback that broadcasts to WebSocket clients
        let item_id = item.id;
        let ws_tx = self.ws_broadcast.clone();
        let progress_callback: crate::protocol::peer::ProgressCallback = Box::new(move |bytes_downloaded, total_bytes, speed_kbps| {
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
            });
        });

        let download_result = client.download_file(
            &username,
            &remote_filename,
            scored_file.size,
            &scored_file.peer_ip,
            scored_file.peer_port,
            &file_path,
            Some(progress_callback),
        ).await;

        match download_result {
            DownloadResult::Completed { bytes } => {
                // Update status to completed
                self.db.update_item_status(item.id, "completed", 1.0, None).await?;
                item.download_status = "completed".to_string();
                item.download_progress = 1.0;
                tracing::info!("[DownloadService] Download completed: {} bytes", bytes);

                // Broadcast completion
                self.broadcast(WsEvent::DownloadCompleted {
                    item_id: item.id,
                    filename: local_filename.clone(),
                    total_bytes: bytes,
                });
                self.broadcast(WsEvent::ItemUpdated {
                    item_id: item.id,
                    filename: local_filename.clone(),
                    status: "completed".to_string(),
                    progress: 1.0,
                });

                Ok(item)
            }
            DownloadResult::TransferStarted { task_id } => {
                // Transfer is running in background - mark as in progress
                tracing::info!("[DownloadService] Transfer started in background (task_id={})", task_id);
                item.download_status = "downloading".to_string();
                Ok(item)
            }
            DownloadResult::Queued { transfer_token, position, reason } => {
                tracing::info!(
                    "[DownloadService] First peer queued us: token={}, position={:?}, reason={}. Trying next peer...",
                    transfer_token, position, reason
                );

                // Try next best file from a different peer - maybe they'll respond with Allowed
                let remaining_results: Vec<_> = results.iter()
                    .filter(|r| r.username != username)
                    .cloned()
                    .collect();

                if let Some(scored2) = SoulseekClient::get_best_file(&remaining_results, query, format) {
                    let local_filename2 = extract_basename(&scored2.filename).to_string();
                    let file_path2 = self.storage_path.join(&local_filename2);

                    // Create progress callback for retry
                    let ws_tx2 = self.ws_broadcast.clone();
                    let retry_progress: crate::protocol::peer::ProgressCallback = Box::new(move |bytes_downloaded, total_bytes, speed_kbps| {
                        let progress_pct = if total_bytes > 0 {
                            (bytes_downloaded as f64 / total_bytes as f64) * 100.0
                        } else {
                            0.0
                        };
                        let _ = ws_tx2.send(WsEvent::DownloadProgress {
                            item_id,
                            bytes_downloaded,
                            total_bytes,
                            progress_pct,
                            speed_kbps,
                        });
                    });

                    match client.download_file(&scored2.username, &scored2.filename, scored2.size, &scored2.peer_ip, scored2.peer_port, &file_path2, Some(retry_progress)).await {
                        DownloadResult::Completed { bytes } => {
                            // Second peer allowed and completed - update file info in DB
                            self.db.update_item_file_info(
                                item.id,
                                &local_filename2,
                                file_path2.to_str().unwrap(),
                                &scored2.username,
                                scored2.size as i64,
                                scored2.bitrate.map(|b| b as i32),
                            ).await?;
                            self.db.update_item_status(item.id, "completed", 1.0, None).await?;

                            item.filename = local_filename2.clone();
                            item.file_path = file_path2.to_string_lossy().to_string();
                            item.source_username = scored2.username.clone();
                            item.download_status = "completed".to_string();
                            item.download_progress = 1.0;

                            tracing::info!("[DownloadService] Download completed from alternate peer: {} bytes", bytes);

                            // Broadcast completion
                            self.broadcast(WsEvent::DownloadCompleted {
                                item_id: item.id,
                                filename: local_filename2.clone(),
                                total_bytes: bytes,
                            });
                            self.broadcast(WsEvent::ItemUpdated {
                                item_id: item.id,
                                filename: local_filename2.clone(),
                                status: "completed".to_string(),
                                progress: 1.0,
                            });

                            return Ok(item);
                        }
                        DownloadResult::TransferStarted { .. } => {
                            // Transfer started in background - treat as success
                            item.download_status = "downloading".to_string();
                            return Ok(item);
                        }
                        DownloadResult::Queued { .. } => {
                            // Second peer also queued - accept first queue
                            tracing::info!("[DownloadService] Second peer also queued. Accepting first queue.");
                        }
                        DownloadResult::Failed { reason: reason2 } => {
                            // Second peer failed - accept first queue
                            tracing::info!("[DownloadService] Second peer failed ({}). Accepting first queue.", reason2);
                        }
                    }
                }

                // No better option found - accept the queued status from first peer
                self.db.update_item_status(item.id, "queued", 0.0, None).await?;
                item.download_status = "queued".to_string();
                tracing::info!("[DownloadService] Download queued, waiting for peer to connect");

                // Broadcast queued status
                self.broadcast(WsEvent::DownloadQueued {
                    item_id: item.id,
                    position,
                    reason: reason.clone(),
                });
                self.broadcast(WsEvent::ItemUpdated {
                    item_id: item.id,
                    filename: local_filename.clone(),
                    status: "queued".to_string(),
                    progress: 0.0,
                });

                Ok(item)
            }
            DownloadResult::Failed { reason } => {
                tracing::warn!("[DownloadService] First download attempt failed: {}. Trying next best file...", reason);

                // Try next best file from a different peer
                let remaining_results: Vec<_> = results.iter()
                    .filter(|r| r.username != username)
                    .cloned()
                    .collect();

                if let Some(scored2) = SoulseekClient::get_best_file(&remaining_results, query, format) {
                    let local_filename2 = extract_basename(&scored2.filename).to_string();
                    let file_path2 = self.storage_path.join(&local_filename2);

                    // Create progress callback for retry
                    let ws_tx3 = self.ws_broadcast.clone();
                    let retry_progress2: crate::protocol::peer::ProgressCallback = Box::new(move |bytes_downloaded, total_bytes, speed_kbps| {
                        let progress_pct = if total_bytes > 0 {
                            (bytes_downloaded as f64 / total_bytes as f64) * 100.0
                        } else {
                            0.0
                        };
                        let _ = ws_tx3.send(WsEvent::DownloadProgress {
                            item_id,
                            bytes_downloaded,
                            total_bytes,
                            progress_pct,
                            speed_kbps,
                        });
                    });

                    match client.download_file(&scored2.username, &scored2.filename, scored2.size, &scored2.peer_ip, scored2.peer_port, &file_path2, Some(retry_progress2)).await {
                        DownloadResult::Completed { bytes } => {
                            // Update item with new file info from retry peer
                            self.db.update_item_file_info(
                                item.id,
                                &local_filename2,
                                file_path2.to_str().unwrap(),
                                &scored2.username,
                                scored2.size as i64,
                                scored2.bitrate.map(|b| b as i32),
                            ).await?;
                            self.db.update_item_status(item.id, "completed", 1.0, None).await?;

                            // Update the item struct to reflect new file info
                            item.filename = local_filename2.clone();
                            item.file_path = file_path2.to_string_lossy().to_string();
                            item.source_username = scored2.username.clone();
                            item.download_status = "completed".to_string();
                            item.download_progress = 1.0;

                            tracing::info!("[DownloadService] Retry download completed: {} bytes", bytes);

                            // Broadcast completion
                            self.broadcast(WsEvent::DownloadCompleted {
                                item_id: item.id,
                                filename: local_filename2.clone(),
                                total_bytes: bytes,
                            });
                            self.broadcast(WsEvent::ItemUpdated {
                                item_id: item.id,
                                filename: local_filename2.clone(),
                                status: "completed".to_string(),
                                progress: 1.0,
                            });

                            Ok(item)
                        }
                        DownloadResult::TransferStarted { .. } => {
                            // Transfer started in background
                            item.download_status = "downloading".to_string();
                            Ok(item)
                        }
                        DownloadResult::Queued { transfer_token, position, reason: queue_reason } => {
                            // Second attempt also queued
                            self.db.update_item_status(item.id, "queued", 0.0, None).await?;
                            item.download_status = "queued".to_string();
                            tracing::info!(
                                "[DownloadService] Retry download queued: token={}, position={:?}",
                                transfer_token, position
                            );

                            // Broadcast queued status
                            self.broadcast(WsEvent::DownloadQueued {
                                item_id: item.id,
                                position,
                                reason: queue_reason.clone(),
                            });
                            self.broadcast(WsEvent::ItemUpdated {
                                item_id: item.id,
                                filename: local_filename2.clone(),
                                status: "queued".to_string(),
                                progress: 0.0,
                            });

                            Ok(item)
                        }
                        DownloadResult::Failed { reason: reason2 } => {
                            // Both attempts failed
                            let error_msg = format!("Both download attempts failed: {} / {}", reason, reason2);
                            self.db.update_item_status(item.id, "failed", 0.0, Some(&error_msg)).await?;
                            item.download_status = "failed".to_string();
                            item.error_message = Some(error_msg.clone());

                            // Broadcast failure
                            self.broadcast(WsEvent::DownloadFailed {
                                item_id: item.id,
                                error: error_msg.clone(),
                            });
                            self.broadcast(WsEvent::ItemUpdated {
                                item_id: item.id,
                                filename: local_filename.clone(),
                                status: "failed".to_string(),
                                progress: 0.0,
                            });

                            bail!(error_msg);
                        }
                    }
                } else {
                    // No alternative files
                    let error_msg = format!("Download failed and no alternative files available: {}", reason);
                    self.db.update_item_status(item.id, "failed", 0.0, Some(&error_msg)).await?;
                    item.download_status = "failed".to_string();
                    item.error_message = Some(error_msg.clone());

                    // Broadcast failure
                    self.broadcast(WsEvent::DownloadFailed {
                        item_id: item.id,
                        error: error_msg.clone(),
                    });
                    self.broadcast(WsEvent::ItemUpdated {
                        item_id: item.id,
                        filename: local_filename.clone(),
                        status: "failed".to_string(),
                        progress: 0.0,
                    });

                    bail!(error_msg);
                }
            }
        }
    }

    /// Search for and download a list of items
    /// Uses concurrent processing with a semaphore to limit simultaneous operations
    pub async fn search_and_download_list(
        &self,
        queries: Vec<String>,
        list_name: Option<String>,
        user_id: i64,
        format: Option<String>,
    ) -> Result<List> {
        tracing::info!("[DownloadService] === LIST SEARCH STARTING ===");
        tracing::info!("[DownloadService] Query count: {}", queries.len());
        tracing::info!("[DownloadService] Format filter: {:?}", format);
        tracing::info!("[DownloadService] User ID: {}", user_id);

        let name = list_name.unwrap_or_else(|| {
            chrono::Utc::now().format("List %Y-%m-%d %H:%M").to_string()
        });
        tracing::info!("[DownloadService] List name: {}", name);

        // Create the list record
        tracing::info!("[DownloadService] Creating list record in database...");
        let mut list = self.db.create_list(&name, user_id, queries.len() as i32).await?;
        tracing::info!("[DownloadService] List created with ID: {}", list.id);

        // Use a semaphore to limit concurrent operations
        // This prevents overwhelming the network while still allowing parallelism
        let max_concurrent = 3;
        let semaphore = Arc::new(Semaphore::new(max_concurrent));
        tracing::info!("[DownloadService] Using semaphore with {} permits for concurrent processing", max_concurrent);

        // Download each item with controlled concurrency
        tracing::info!("[DownloadService] Spawning {} download tasks...", queries.len());
        let handles: Vec<_> = queries.into_iter()
            .enumerate()
            .map(|(index, query)| {
                let service = DownloadService {
                    db: self.db.clone(),
                    slsk_client: Arc::clone(&self.slsk_client),
                    storage_path: self.storage_path.clone(),
                    fuzzy_matcher: FuzzyMatcher::new(70),
                    ws_broadcast: self.ws_broadcast.clone(),
                };
                let list_id = list.id;
                let format_clone = format.clone();
                let sem = Arc::clone(&semaphore);

                tokio::spawn(async move {
                    // Acquire semaphore permit before starting
                    let _permit = sem.acquire().await.expect("Semaphore closed");
                    tracing::info!("[DownloadService] Task {} acquired permit, starting for query: '{}'", index, query);

                    // Use faster search timeout (5s) for batch processing
                    match service.search_and_download_item_with_timeout(&query, user_id, format_clone.as_deref(), 5).await {
                        Ok(item) => {
                            tracing::info!("[DownloadService] Task {} succeeded for query: '{}' -> item {}", index, query, item.id);
                            // Add item to list
                            if let Err(e) = service.db.add_item_to_list(list_id, item.id, index as i32).await {
                                tracing::error!("[DownloadService] Failed to add item {} to list {}: {}", item.id, list_id, e);
                            } else {
                                tracing::info!("[DownloadService] Added item {} to list {} at position {}", item.id, list_id, index);
                            }
                            Ok(item)
                        }
                        Err(e) => {
                            tracing::error!("[DownloadService] Task {} failed for query '{}': {}", index, query, e);
                            Err(e)
                        }
                    }
                    // Permit automatically released when _permit is dropped
                })
            })
            .collect();

        tracing::info!("[DownloadService] All {} tasks spawned (max {} concurrent), waiting for completion...", handles.len(), max_concurrent);

        // Wait for all downloads to complete
        let mut completed = 0;
        let mut failed = 0;

        for (i, handle) in handles.into_iter().enumerate() {
            match handle.await {
                Ok(Ok(_)) => {
                    completed += 1;
                    tracing::info!("[DownloadService] Handle {} completed ({}/{})", i, completed, completed + failed);
                }
                Ok(Err(e)) => {
                    failed += 1;
                    tracing::error!("[DownloadService] Handle {} failed: {} ({}/{})", i, e, completed, completed + failed);
                }
                Err(e) => {
                    tracing::error!("[DownloadService] Handle {} panicked: {}", i, e);
                    failed += 1;
                }
            }
        }

        tracing::info!("[DownloadService] === LIST SEARCH RESULTS ===");
        tracing::info!("[DownloadService] Completed: {}", completed);
        tracing::info!("[DownloadService] Failed: {}", failed);
        tracing::info!("[DownloadService] Total: {}", list.total_items);

        // Update list status
        let status = if completed == list.total_items {
            "completed"
        } else if completed > 0 {
            "partial"
        } else {
            "failed"
        };

        tracing::info!("[DownloadService] Final status: {}", status);
        self.db.update_list_status(list.id, status, completed, failed).await?;

        list.status = status.to_string();
        list.completed_items = completed;
        list.failed_items = failed;

        tracing::info!("[DownloadService] === LIST SEARCH COMPLETE ===");
        Ok(list)
    }

    /// Get item by ID
    pub async fn get_item(&self, id: i64) -> Result<Option<Item>> {
        self.db.get_item(id).await
    }

    /// Get all items
    pub async fn get_all_items(&self) -> Result<Vec<Item>> {
        self.db.get_all_items().await
    }

    /// Get list by ID
    pub async fn get_list(&self, id: i64) -> Result<Option<List>> {
        self.db.get_list(id).await
    }

    /// Get list with items
    pub async fn get_list_with_items(&self, id: i64) -> Result<Option<(List, Vec<Item>)>> {
        if let Some(list) = self.db.get_list(id).await? {
            let items = self.db.get_list_items(id).await?;
            Ok(Some((list, items)))
        } else {
            Ok(None)
        }
    }

    /// Get all lists for a user
    pub async fn get_user_lists(&self, user_id: i64) -> Result<Vec<List>> {
        self.db.get_user_lists(user_id).await
    }

    /// Delete list
    pub async fn delete_list(&self, id: i64) -> Result<()> {
        tracing::info!("[DownloadService] === DELETE LIST ===");
        tracing::info!("[DownloadService] List ID: {}", id);

        // Get list info before deletion for logging
        if let Ok(Some(list)) = self.db.get_list(id).await {
            tracing::info!("[DownloadService] List name: {}", list.name);
            tracing::info!("[DownloadService] List status: {}", list.status);
            tracing::info!("[DownloadService] List items: {}", list.total_items);
        }

        let result = self.db.delete_list(id).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] List {} deleted successfully from database", id),
            Err(e) => tracing::error!("[DownloadService] Failed to delete list {} from database: {}", id, e),
        }
        result
    }

    /// Delete item
    pub async fn delete_item(&self, id: i64) -> Result<()> {
        tracing::info!("[DownloadService] === DELETE ITEM ===");
        tracing::info!("[DownloadService] Item ID: {}", id);

        // Also delete the file from disk
        if let Some(item) = self.db.get_item(id).await? {
            tracing::info!("[DownloadService] Item filename: {}", item.filename);
            tracing::info!("[DownloadService] Item file_path: {}", item.file_path);
            tracing::info!("[DownloadService] Item status: {}", item.download_status);

            tracing::info!("[DownloadService] Attempting to delete file from disk...");
            match tokio::fs::remove_file(&item.file_path).await {
                Ok(_) => tracing::info!("[DownloadService] File deleted from disk: {}", item.file_path),
                Err(e) => tracing::warn!("[DownloadService] Failed to delete file {} from disk: {}", item.file_path, e),
            }
        } else {
            tracing::warn!("[DownloadService] Item {} not found in database", id);
        }

        tracing::info!("[DownloadService] Deleting item {} from database...", id);
        let result = self.db.delete_item(id).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] Item {} deleted successfully from database", id),
            Err(e) => tracing::error!("[DownloadService] Failed to delete item {} from database: {}", id, e),
        }
        result
    }

    /// Batch delete items
    pub async fn delete_items(&self, ids: Vec<i64>) -> Result<()> {
        tracing::info!("[DownloadService] === BATCH DELETE ITEMS ===");
        tracing::info!("[DownloadService] Item IDs: {:?}", ids);
        tracing::info!("[DownloadService] Count: {}", ids.len());

        // Delete files from disk
        let mut files_deleted = 0;
        let mut files_failed = 0;
        for id in &ids {
            if let Some(item) = self.db.get_item(*id).await? {
                tracing::info!("[DownloadService] Processing item {}: {} ({})", id, item.filename, item.file_path);
                match tokio::fs::remove_file(&item.file_path).await {
                    Ok(_) => {
                        files_deleted += 1;
                        tracing::info!("[DownloadService] Deleted file: {}", item.file_path);
                    }
                    Err(e) => {
                        files_failed += 1;
                        tracing::warn!("[DownloadService] Failed to delete file {}: {}", item.file_path, e);
                    }
                }
            } else {
                tracing::warn!("[DownloadService] Item {} not found in database", id);
            }
        }

        tracing::info!("[DownloadService] Files deleted: {}, Files failed: {}", files_deleted, files_failed);
        tracing::info!("[DownloadService] Deleting {} items from database...", ids.len());

        let result = self.db.delete_items(&ids).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] {} items deleted successfully from database", ids.len()),
            Err(e) => tracing::error!("[DownloadService] Failed to delete items from database: {}", e),
        }
        result
    }

    /// Batch delete lists
    pub async fn delete_lists(&self, ids: Vec<i64>) -> Result<()> {
        tracing::info!("[DownloadService] === BATCH DELETE LISTS ===");
        tracing::info!("[DownloadService] List IDs: {:?}", ids);
        tracing::info!("[DownloadService] Count: {}", ids.len());

        // Log each list before deletion
        for id in &ids {
            if let Ok(Some(list)) = self.db.get_list(*id).await {
                tracing::info!("[DownloadService] List {}: {} (status: {}, items: {})",
                    id, list.name, list.status, list.total_items);
            }
        }

        let result = self.db.delete_lists(&ids).await;
        match &result {
            Ok(_) => tracing::info!("[DownloadService] {} lists deleted successfully from database", ids.len()),
            Err(e) => tracing::error!("[DownloadService] Failed to delete lists from database: {}", e),
        }
        result
    }
}
