//! Simplified Soulseek client using proven protocol logic from E2E tests.
//!
//! This implementation uses raw TCP streams and direct message handling,
//! matching the working patterns from our E2E test suite.

use anyhow::{Result, bail, Context};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use lru::LruCache;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::net::{TcpStream, TcpListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use bytes::{Buf, BufMut, BytesMut};

use super::messages::{SearchFile, FileAttribute, decode_string};
use super::peer::{
    FileConnection, IncomingConnection, handle_incoming_connection,
    connect_to_peer_for_results, receive_search_results_from_peer,
    queue_upload, wait_for_transfer_request,  // Modern QueueUpload method
    NegotiationResult, ParsedSearchResponse,
    send_message, read_message, encode_string,
    download_file as peer_download_file,  // Working download implementation
};
use super::file_selection::{find_best_file, find_best_files, ScoredFile};

/// Default port for incoming peer connections
pub const PEER_LISTEN_PORT: u16 = 2234;
/// Obfuscated port for incoming peer connections (PEER_LISTEN_PORT + 1)
pub const PEER_OBFUSCATED_PORT: u16 = 2235;

const SERVER_HOST: &str = "server.slsknet.org";
const SERVER_PORT: u16 = 2242;

/// Maximum number of peer connections to cache for potential downloads
/// This prevents file descriptor exhaustion from accumulating connections
const MAX_CACHED_PEER_CONNECTIONS: usize = 100;

/// Counter for tracking active connection handler tasks (for diagnostics)
static ACTIVE_HANDLERS: AtomicU32 = AtomicU32::new(0);

/// Search result from the network
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub username: String,
    pub files: Vec<SearchFile>,
    pub has_slots: bool,
    pub avg_speed: u32,
    pub queue_length: u64,
    /// IP address (if known from ConnectToPeer)
    pub peer_ip: String,
    /// Port (if known from ConnectToPeer)
    pub peer_port: u32,
}

/// Download progress
#[derive(Debug, Clone)]
pub struct DownloadProgress {
    pub filename: String,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub status: DownloadStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadStatus {
    Pending,
    Queued,
    InProgress,
    Completed,
    Failed(String),
}

/// Result of a download attempt
#[derive(Debug, Clone)]
pub enum DownloadResult {
    /// Download completed successfully
    Completed { bytes: u64 },
    /// File transfer has been spawned as a background task
    /// The transfer will complete asynchronously and update status via callback
    TransferStarted {
        /// Handle to wait for completion if needed
        task_id: u64,
    },
    /// Download is queued at the peer - will receive F connection later
    Queued {
        transfer_token: u32,
        position: Option<u32>,
        reason: String,
    },
    /// Download failed
    Failed { reason: String },
}

/// Progress callback type for reporting download progress
/// Parameters: (downloaded_bytes, total_bytes)
pub type ProgressCallback = Box<dyn Fn(u64, u64) + Send + Sync>;

/// Generic transfer completion notification
/// Used to notify when a spawned file transfer completes
#[derive(Debug)]
pub struct TransferComplete {
    pub item_id: i64,
    pub queue_id: i64,
    pub list_id: Option<i64>,
    pub transfer_token: u32,
    pub result: Result<u64, String>,  // Ok(bytes) or Err(error message)
}

/// Sender for transfer completion notifications
pub type TransferCompleteSender = mpsc::Sender<TransferComplete>;

/// A pending (queued) download waiting for F connection
/// Note: Not Clone due to callback fields
pub struct PendingDownload {
    pub username: String,
    pub filename: String,
    pub filesize: u64,
    pub transfer_token: u32,
    pub output_path: std::path::PathBuf,
    pub queued_at: std::time::Instant,
    // Queue integration fields (optional - only set when using queue system)
    pub item_id: Option<i64>,
    pub queue_id: Option<i64>,
    pub list_id: Option<i64>,
    pub progress_callback: Option<super::peer::ArcProgressCallback>,
    pub completion_tx: Option<TransferCompleteSender>,
}

impl std::fmt::Debug for PendingDownload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingDownload")
            .field("username", &self.username)
            .field("filename", &self.filename)
            .field("filesize", &self.filesize)
            .field("transfer_token", &self.transfer_token)
            .field("output_path", &self.output_path)
            .field("queued_at", &self.queued_at)
            .field("item_id", &self.item_id)
            .field("queue_id", &self.queue_id)
            .field("list_id", &self.list_id)
            .field("has_progress_callback", &self.progress_callback.is_some())
            .field("has_completion_tx", &self.completion_tx.is_some())
            .finish()
    }
}

/// Result of initiating a download (for queue-based downloads)
#[derive(Debug)]
pub enum DownloadInitResult {
    /// Transfer has been spawned as a background task
    TransferSpawned {
        item_id: i64,
        transfer_token: u32,
    },
    /// Download is queued at peer - F connection will arrive later
    Queued {
        transfer_token: u32,
        position: Option<u32>,
        reason: String,
    },
    /// Download failed during negotiation
    Failed {
        reason: String,
    },
}

/// Main Soulseek client - simplified architecture matching E2E test
pub struct SoulseekClient {
    /// Raw server stream
    server_stream: TcpStream,
    /// Our username
    username: String,
    /// Channel for receiving F connections from listener
    file_conn_rx: Arc<Mutex<mpsc::Receiver<FileConnection>>>,
    /// Sender for F connections (used by listener)
    file_conn_tx: mpsc::Sender<FileConnection>,
    /// Stored peer streams for download (username -> stream)
    /// Uses LRU cache to prevent file descriptor exhaustion
    peer_streams: Arc<Mutex<LruCache<String, TcpStream>>>,
    /// Incoming search results from listener (peers connecting to us)
    incoming_results: Arc<Mutex<Vec<SearchResult>>>,
    /// Download progress tracking
    downloads: Arc<Mutex<HashMap<String, DownloadProgress>>>,
    /// Pending downloads waiting for F connection (token -> PendingDownload)
    pending_downloads: Arc<Mutex<HashMap<u32, PendingDownload>>>,
}

impl SoulseekClient {
    /// Connect to Soulseek server and login
    pub async fn connect(username: &str, password: &str) -> Result<Self> {
        // Connect to server
        let addr = format!("{}:{}", SERVER_HOST, SERVER_PORT);
        tracing::info!("[SoulseekClient] Connecting to {}", addr);

        let mut stream = TcpStream::connect(&addr)
            .await
            .context("Failed to connect to Soulseek server")?;

        // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
        stream.set_nodelay(true)?;

        // Login
        Self::login(&mut stream, username, password).await?;
        tracing::info!("[SoulseekClient] Logged in as '{}'", username);

        // Set shared counts (0 for now)
        Self::set_shared_counts(&mut stream, 0, 0).await?;

        // Set wait port (with obfuscated port)
        Self::set_wait_port(&mut stream, PEER_LISTEN_PORT as u32, Some(PEER_OBFUSCATED_PORT as u32)).await?;
        tracing::info!("[SoulseekClient] Set wait port to {} (obfuscated: {})", PEER_LISTEN_PORT, PEER_OBFUSCATED_PORT);

        // Create channels
        let (file_conn_tx, file_conn_rx) = mpsc::channel::<FileConnection>(50);

        let client = Self {
            server_stream: stream,
            username: username.to_string(),
            file_conn_rx: Arc::new(Mutex::new(file_conn_rx)),
            file_conn_tx,
            peer_streams: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(MAX_CACHED_PEER_CONNECTIONS).unwrap()
            ))),
            incoming_results: Arc::new(Mutex::new(Vec::new())),
            downloads: Arc::new(Mutex::new(HashMap::new())),
            pending_downloads: Arc::new(Mutex::new(HashMap::new())),
        };

        // Start listener
        client.start_listener();

        Ok(client)
    }

    /// Login to the server
    async fn login(stream: &mut TcpStream, username: &str, password: &str) -> Result<()> {
        let combined = format!("{}{}", username, password);
        let md5_hash = format!("{:x}", md5::compute(combined.as_bytes()));

        let mut data = BytesMut::new();
        encode_string(&mut data, username);
        encode_string(&mut data, password);
        data.put_u32_le(160); // version
        encode_string(&mut data, &md5_hash);
        data.put_u32_le(1); // minor version

        send_message(stream, 1, &data).await?;

        let (code, response_data) = read_message(stream).await?;
        if code == 1 {
            let mut buf = BytesMut::from(&response_data[..]);
            let success = buf.get_u8() != 0;
            let reason = decode_string(&mut buf)?;

            if !success {
                bail!("Login failed: {}", reason);
            }
        }

        Ok(())
    }

    /// Set wait port (with optional obfuscated port)
    async fn set_wait_port(stream: &mut TcpStream, port: u32, obfuscated_port: Option<u32>) -> Result<()> {
        let mut data = BytesMut::new();
        data.put_u32_le(port);
        if let Some(obfs_port) = obfuscated_port {
            data.put_u32_le(1); // Obfuscation type: 1 = Rotated
            data.put_u32_le(obfs_port);
        }
        send_message(stream, 2, &data).await?;
        Ok(())
    }

    /// Set shared folder/file counts
    async fn set_shared_counts(stream: &mut TcpStream, folders: u32, files: u32) -> Result<()> {
        let mut data = BytesMut::new();
        data.put_u32_le(folders);
        data.put_u32_le(files);
        send_message(stream, 35, &data).await?;
        Ok(())
    }

    /// Send file search
    async fn send_search(stream: &mut TcpStream, token: u32, query: &str) -> Result<()> {
        let mut data = BytesMut::new();
        data.put_u32_le(token);
        encode_string(&mut data, query);
        send_message(stream, 26, &data).await?;
        Ok(())
    }

    /// Start listener for incoming P and F connections on both regular and obfuscated ports
    fn start_listener(&self) {
        // Start listener on main port (2234)
        self.start_listener_on_port(PEER_LISTEN_PORT, false);
        // Start listener on obfuscated port (2235)
        self.start_listener_on_port(PEER_OBFUSCATED_PORT, true);
    }

    /// Start a listener on a specific port
    fn start_listener_on_port(&self, port: u16, is_obfuscated: bool) {
        let file_conn_tx = self.file_conn_tx.clone();
        let peer_streams = Arc::clone(&self.peer_streams);
        let incoming_results = Arc::clone(&self.incoming_results);
        let pending_downloads = Arc::clone(&self.pending_downloads);
        let downloads = Arc::clone(&self.downloads);
        let username = self.username.clone();
        let port_label = if is_obfuscated { "obfuscated" } else { "main" };

        tokio::spawn(async move {
            let addr = format!("0.0.0.0:{}", port);
            let listener = match TcpListener::bind(&addr).await {
                Ok(l) => {
                    tracing::info!("[Listener] Listening on {} ({} port)", addr, port_label);
                    l
                }
                Err(e) => {
                    tracing::error!("[Listener] Failed to bind {} port {}: {}", port_label, port, e);
                    return;
                }
            };

            loop {
                match listener.accept().await {
                    Ok((stream, addr)) => {
                        // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
                        if let Err(e) = stream.set_nodelay(true) {
                            tracing::warn!("[Listener] Failed to set TCP_NODELAY: {}", e);
                        }
                        tracing::info!("[Listener] Accepted connection from {} on {} port", addr, port_label);

                        let file_conn_tx = file_conn_tx.clone();
                        let peer_streams = Arc::clone(&peer_streams);
                        let incoming_results = Arc::clone(&incoming_results);
                        let pending_downloads = Arc::clone(&pending_downloads);
                        let downloads = Arc::clone(&downloads);
                        let our_username = username.clone();
                        let peer_addr = addr.to_string();
                        let port_label_owned = port_label.to_string();

                        tokio::spawn(async move {
                            let active = ACTIVE_HANDLERS.fetch_add(1, Ordering::Relaxed) + 1;
                            tracing::debug!("[Listener] Handler started, active_handlers={}", active);

                            let task_start = std::time::Instant::now();
                            let handshake_start = std::time::Instant::now();
                            let handshake_result = handle_incoming_connection(stream, &our_username).await;
                            let handshake_duration = handshake_start.elapsed();

                            match handshake_result {
                                Ok(IncomingConnection::File(conn)) => {
                                    let token = conn.transfer_token;
                                    tracing::info!(
                                        "[Listener] F connection from {} ({}), token={}, handshake={:?}",
                                        addr, port_label_owned, token, handshake_duration
                                    );

                                    // Check if this matches a pending (queued) download
                                    let pending = pending_downloads.lock().await.remove(&token);
                                    if let Some(pending_dl) = pending {
                                        tracing::info!(
                                            "[Listener] Resuming pending download for '{}' (item_id={:?})",
                                            pending_dl.filename,
                                            pending_dl.item_id
                                        );

                                        // Update status to in progress
                                        {
                                            let mut dl = downloads.lock().await;
                                            if let Some(p) = dl.get_mut(&pending_dl.filename) {
                                                p.status = DownloadStatus::InProgress;
                                            }
                                        }

                                        // Spawn the file transfer as a background task
                                        let downloads_clone = Arc::clone(&downloads);
                                        let filename = pending_dl.filename.clone();
                                        let filesize = pending_dl.filesize;
                                        let output_path = pending_dl.output_path.clone();
                                        let progress_callback = pending_dl.progress_callback;
                                        let completion_tx = pending_dl.completion_tx;
                                        let item_id = pending_dl.item_id;
                                        let queue_id = pending_dl.queue_id;
                                        let list_id = pending_dl.list_id;
                                        let transfer_token = pending_dl.transfer_token;

                                        tokio::spawn(async move {
                                            tracing::info!(
                                                "[Transfer] Starting file receive: {} ({} bytes)",
                                                filename, filesize
                                            );

                                            // Use receive_file_data from peer module
                                            let result = super::peer::receive_file_data(
                                                conn,
                                                filesize,
                                                output_path.clone(),
                                                progress_callback,
                                            ).await;

                                            // Update internal download status
                                            match &result {
                                                Ok(bytes) => {
                                                    tracing::info!(
                                                        "[Transfer] Completed: {} ({} bytes)",
                                                        filename, bytes
                                                    );
                                                    let mut dl = downloads_clone.lock().await;
                                                    if let Some(p) = dl.get_mut(&filename) {
                                                        p.downloaded_bytes = *bytes;
                                                        p.status = DownloadStatus::Completed;
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::error!(
                                                        "[Transfer] Failed: {} - {}",
                                                        filename, e
                                                    );
                                                    let mut dl = downloads_clone.lock().await;
                                                    if let Some(p) = dl.get_mut(&filename) {
                                                        p.status = DownloadStatus::Failed(e.to_string());
                                                    }
                                                }
                                            }

                                            // Send completion notification if channel provided
                                            if let (Some(tx), Some(item_id), Some(queue_id)) =
                                                (completion_tx, item_id, queue_id)
                                            {
                                                let completion = TransferComplete {
                                                    item_id,
                                                    queue_id,
                                                    list_id,
                                                    transfer_token,
                                                    result: result.map_err(|e| e.to_string()),
                                                };
                                                if let Err(e) = tx.send(completion).await {
                                                    tracing::warn!(
                                                        "[Transfer] Failed to send completion: {}",
                                                        e
                                                    );
                                                }
                                            }
                                        });
                                    } else {
                                        // Not a pending download, send to channel for active downloads
                                        let _ = file_conn_tx.send(conn).await;
                                    }
                                }
                                Ok(IncomingConnection::Peer(mut stream, peer_username, _token)) => {
                                    tracing::info!(
                                        "[Listener] P connection from '{}' ({}, {}), handshake={:?}",
                                        peer_username, addr, port_label_owned, handshake_duration
                                    );

                                    // Try to receive search results
                                    let recv_start = std::time::Instant::now();
                                    match receive_search_results_from_peer(&mut stream).await {
                                        Ok(response) => {
                                            let recv_duration = recv_start.elapsed();
                                            tracing::info!(
                                                "[Listener] Got {} files from '{}' ({}, {:.0} KB/s), recv={:?}, total={:?}",
                                                response.files.len(),
                                                peer_username,
                                                if response.slot_free { "FREE" } else { "BUSY" },
                                                response.avg_speed as f64 / 1024.0,
                                                recv_duration,
                                                task_start.elapsed()
                                            );

                                            // Store stream for potential download
                                            peer_streams.lock().await.put(peer_username.clone(), stream);

                                            // Store result for search aggregation
                                            let search_result = SearchResult {
                                                username: peer_username,
                                                files: response.files.iter().map(|(filename, size, bitrate)| {
                                                    SearchFile {
                                                        code: 1,
                                                        filename: filename.clone(),
                                                        size: *size,
                                                        extension: std::path::Path::new(filename)
                                                            .extension()
                                                            .and_then(|e| e.to_str())
                                                            .unwrap_or("")
                                                            .to_string(),
                                                        attributes: bitrate.map(|b| vec![
                                                            FileAttribute { attribute_type: 0, value: b }
                                                        ]).unwrap_or_default(),
                                                    }
                                                }).collect(),
                                                has_slots: response.slot_free,
                                                avg_speed: response.avg_speed,
                                                queue_length: response.queue_length as u64,
                                                peer_ip: peer_addr.split(':').next().unwrap_or("").to_string(),
                                                peer_port: 0, // Incoming connections - we don't know their listen port
                                            };
                                            incoming_results.lock().await.push(search_result);
                                        }
                                        Err(e) => {
                                            let recv_duration = recv_start.elapsed();
                                            tracing::debug!(
                                                "[Listener] No results from '{}': {}, recv={:?}, total={:?}",
                                                peer_username, e, recv_duration, task_start.elapsed()
                                            );
                                            // Don't store connection - we won't download from peers with no results
                                            // The stream will be dropped and connection closed
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!(
                                        "[Listener] Connection from {} failed handshake: {}, handshake={:?}",
                                        addr, e, handshake_duration
                                    );
                                }
                            }

                            let remaining = ACTIVE_HANDLERS.fetch_sub(1, Ordering::Relaxed) - 1;
                            tracing::debug!(
                                "[Listener] Handler finished, active_handlers={}, total_time={:?}",
                                remaining, task_start.elapsed()
                            );
                        });
                    }
                    Err(e) => {
                        tracing::error!("[Listener] Accept error on {} port: {}", port_label, e);
                    }
                }
            }
        });
    }

    /// Search for files on the network
    pub async fn search(&mut self, query: &str, timeout_secs: u64) -> Result<Vec<SearchResult>> {
        let token: u32 = rand::random();

        tracing::info!("[SoulseekClient] === SEARCH START ===");
        tracing::info!("[SoulseekClient] Query: '{}'", query);
        tracing::info!("[SoulseekClient] Token: {}, Timeout: {}s", token, timeout_secs);

        // Clear previous incoming results (from listener)
        self.incoming_results.lock().await.clear();

        // Send search
        Self::send_search(&mut self.server_stream, token, query).await?;
        tracing::info!("[SoulseekClient] Search sent to server, waiting for ConnectToPeer messages...");

        // Channel for collecting results from background ConnectToPeer tasks
        let (result_tx, mut result_rx) = mpsc::channel::<(String, String, u32, TcpStream, ParsedSearchResponse)>(50);

        // Channel for collecting failed connections (to send CantConnectToPeer)
        let (failure_tx, mut failure_rx) = mpsc::channel::<(u32, String)>(100);

        let mut results: Vec<SearchResult> = Vec::new();
        let mut connect_to_peer_count = 0;
        let mut first_connect_to_peer_time: Option<std::time::Instant> = None;
        let mut first_result_time: Option<std::time::Instant> = None;
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        let min_wait = Duration::from_secs(3); // Wait at least 3 seconds for results
        let mut last_result_time = std::time::Instant::now();

        // Collect results from server ConnectToPeer messages
        // Early exit: once we have enough results and no new ones for 2 seconds, return
        let no_results_timeout = Duration::from_secs(7); // Increased from 5s to give more time

        while start.elapsed() < timeout {
            // Early exit conditions (after minimum wait time):
            // 1. We have at least 5 results with files AND no new results for 2 seconds
            // 2. We have at least 10 results with files (good enough, return immediately)
            // 3. After timeout with ZERO results AND no ConnectToPeer messages received - abort
            let results_with_files = results.iter().filter(|r| !r.files.is_empty()).count();

            // Only abort early if we have no results AND no ConnectToPeer messages at all
            // If we got ConnectToPeer messages but no results yet, keep waiting (connections may be slow)
            if start.elapsed() > no_results_timeout && results_with_files == 0 && connect_to_peer_count == 0 {
                tracing::warn!("[Search] Early abort: no ConnectToPeer messages received after {:.1}s", start.elapsed().as_secs_f32());
                tracing::warn!("[Search] This may indicate: network issues, query didn't match indexed files, or server is slow");
                bail!("No results found for search '{}' - no peers responded", query);
            }

            // Log progress periodically (every 2 seconds)
            let elapsed_secs = start.elapsed().as_secs();
            if elapsed_secs > 0 && elapsed_secs % 2 == 0 && start.elapsed().subsec_millis() < 150 {
                tracing::debug!(
                    "[Search] Progress: {:.1}s elapsed, {} ConnectToPeer msgs, {} results with files",
                    start.elapsed().as_secs_f32(), connect_to_peer_count, results_with_files
                );
            }

            if start.elapsed() > min_wait {
                if results_with_files >= 10 {
                    tracing::info!("[Search] Early exit: {} results with files (enough results)", results_with_files);
                    break;
                }
                if results_with_files >= 5 && last_result_time.elapsed() > Duration::from_secs(2) {
                    tracing::info!("[Search] Early exit: {} results, no new results for 2s", results_with_files);
                    break;
                }
            }
            match tokio::time::timeout(
                Duration::from_millis(100),
                read_message(&mut self.server_stream)
            ).await {
                Ok(Ok((18, data))) => {
                    // ConnectToPeer
                    connect_to_peer_count += 1;
                    let mut buf = BytesMut::from(&data[..]);
                    let peer_username = decode_string(&mut buf)?;
                    let conn_type = decode_string(&mut buf)?;
                    let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                    let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                    let port = buf.get_u32_le();
                    let peer_token = buf.get_u32_le();

                    if conn_type == "P" && connect_to_peer_count <= 150 {
                        // Track time to first ConnectToPeer
                        if first_connect_to_peer_time.is_none() {
                            first_connect_to_peer_time = Some(std::time::Instant::now());
                            tracing::info!("[Search] First ConnectToPeer received at {:.2}s", start.elapsed().as_secs_f32());
                        }
                        tracing::debug!("[Search] ConnectToPeer #{}: {} at {}:{} (token={})",
                            connect_to_peer_count, peer_username, ip, port, peer_token);

                        // Spawn ConnectToPeer attempt in background so we don't block the search loop
                        let tx = result_tx.clone();
                        let fail_tx = failure_tx.clone();
                        let our_username = self.username.clone();
                        let ip_clone = ip.clone();
                        let peer_username_clone = peer_username.clone();
                        tokio::spawn(async move {
                            match connect_to_peer_for_results(&ip_clone, port, &peer_username_clone, &our_username, peer_token).await {
                                Ok((peer_stream, response)) => {
                                    tracing::info!(
                                        "[Search] Got {} files from '{}' ({}, {:.0} KB/s)",
                                        response.files.len(),
                                        peer_username_clone,
                                        if response.slot_free { "FREE" } else { "BUSY" },
                                        response.avg_speed as f64 / 1024.0
                                    );
                                    // Send result back to main task
                                    let _ = tx.send((peer_username_clone, ip_clone, port, peer_stream, response)).await;
                                }
                                Err(e) => {
                                    tracing::debug!("[Search] Failed to get results from '{}': {}", peer_username_clone, e);
                                    // Report failure for CantConnectToPeer (Nicotine+ does this)
                                    let _ = fail_tx.send((peer_token, peer_username_clone)).await;
                                }
                            }
                        });
                    } else if conn_type == "F" {
                        // Indirect F connection - uploader wants us to connect
                        tracing::info!("[Search] ConnectToPeer F from '{}' at {}:{}", peer_username, ip, port);

                        // Handle in background
                        let file_tx = self.file_conn_tx.clone();
                        let fail_tx = failure_tx.clone();
                        let our_username = self.username.clone();
                        let peer_username_clone = peer_username.clone();
                        tokio::spawn(async move {
                            match super::peer::connect_for_indirect_transfer(&ip, port, peer_token, &our_username).await {
                                Ok(conn) => {
                                    let _ = file_tx.send(conn).await;
                                }
                                Err(e) => {
                                    tracing::debug!("[Search] Indirect F failed: {}", e);
                                    // Report failure for CantConnectToPeer
                                    let _ = fail_tx.send((peer_token, peer_username_clone)).await;
                                }
                            }
                        });
                    }
                }
                Ok(Ok((_code, _))) => {
                    // Other server messages, ignore
                }
                Ok(Err(e)) => {
                    tracing::warn!("[Search] Server read error: {}", e);
                    break;
                }
                Err(_) => {
                    // Timeout on server read, continue
                }
            }

            // Check for completed ConnectToPeer results (non-blocking)
            while let Ok((peer_username, ip, port, peer_stream, response)) = result_rx.try_recv() {
                // Track time to first result
                if first_result_time.is_none() && !response.files.is_empty() {
                    first_result_time = Some(std::time::Instant::now());
                    tracing::info!("[Search] First result with files at {:.2}s from '{}'",
                        start.elapsed().as_secs_f32(), peer_username);
                }
                // Store peer stream for later download
                tracing::debug!("[Search] Storing P connection for '{}' (has {} files)", peer_username, response.files.len());
                self.peer_streams.lock().await.put(peer_username.clone(), peer_stream);
                // Convert to SearchResult
                results.push(Self::convert_response(&peer_username, &ip, port, &response));
                last_result_time = std::time::Instant::now();
            }
        }

        // Drop the sender so remaining tasks don't block
        drop(result_tx);
        tracing::debug!("[Search] Dropped result_tx, entering grace period collection");

        // Collect any final results from background tasks (with short grace period)
        let mut grace_results = 0;
        let grace_end = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < grace_end {
            match tokio::time::timeout(Duration::from_millis(100), result_rx.recv()).await {
                Ok(Some((peer_username, ip, port, peer_stream, response))) => {
                    tracing::debug!("[Search] Grace period: storing P connection for '{}' ({} files)", peer_username, response.files.len());
                    self.peer_streams.lock().await.put(peer_username.clone(), peer_stream);
                    results.push(Self::convert_response(&peer_username, &ip, port, &response));
                    grace_results += 1;
                }
                _ => break,
            }
        }
        if grace_results > 0 {
            tracing::debug!("[Search] Collected {} results during grace period", grace_results);
        }

        // Collect incoming results from listener (peers that connected to us)
        let incoming = self.incoming_results.lock().await.drain(..).collect::<Vec<_>>();
        let incoming_count = incoming.len();
        tracing::debug!("[Search] Collected {} incoming results from listener", incoming_count);
        results.extend(incoming);

        // Drop failure sender and drain failure channel to send CantConnectToPeer messages
        drop(failure_tx);
        let mut cant_connect_count = 0;
        while let Ok(Some((token, username))) = tokio::time::timeout(
            Duration::from_millis(10),
            failure_rx.recv()
        ).await {
            if let Err(e) = super::peer::send_cant_connect_to_peer(
                &mut self.server_stream,
                token,
                &username,
            ).await {
                tracing::debug!("[Search] Failed to send CantConnectToPeer for '{}': {}", username, e);
            } else {
                cant_connect_count += 1;
            }
        }
        if cant_connect_count > 0 {
            tracing::debug!("[Search] Sent {} CantConnectToPeer messages", cant_connect_count);
        }

        // Final summary with timing metrics
        let total_time = start.elapsed();
        let cached_peers = self.peer_streams.lock().await.len();
        let total_files: usize = results.iter().map(|r| r.files.len()).sum();

        tracing::info!("[SoulseekClient] === SEARCH COMPLETE ===");
        tracing::info!("[SoulseekClient] Query: '{}'", query);
        tracing::info!("[SoulseekClient] Total time: {:.2}s", total_time.as_secs_f32());
        if let Some(t) = first_connect_to_peer_time {
            tracing::info!("[SoulseekClient] Time to first ConnectToPeer: {:.2}s", (t.duration_since(start)).as_secs_f32());
        } else {
            tracing::warn!("[SoulseekClient] No ConnectToPeer messages received!");
        }
        if let Some(t) = first_result_time {
            tracing::info!("[SoulseekClient] Time to first result: {:.2}s", (t.duration_since(start)).as_secs_f32());
        } else if connect_to_peer_count > 0 {
            tracing::warn!("[SoulseekClient] Got {} ConnectToPeer but no successful file results!", connect_to_peer_count);
        }
        tracing::info!("[SoulseekClient] ConnectToPeer messages received: {}", connect_to_peer_count);
        tracing::info!("[SoulseekClient] Connection failures (CantConnectToPeer sent): {}", cant_connect_count);
        tracing::info!("[SoulseekClient] Successful peer connections: {} users", results.len());
        tracing::info!("[SoulseekClient] Incoming connections (peers connected to us): {}", incoming_count);
        tracing::info!("[SoulseekClient] Total files found: {}", total_files);
        tracing::info!("[SoulseekClient] Cached peer connections: {}", cached_peers);

        Ok(results)
    }

    /// Convert ParsedSearchResponse to SearchResult
    fn convert_response(username: &str, ip: &str, port: u32, response: &ParsedSearchResponse) -> SearchResult {
        SearchResult {
            username: username.to_string(),
            files: response.files.iter().map(|(filename, size, bitrate)| {
                SearchFile {
                    code: 1,
                    filename: filename.clone(),
                    size: *size,
                    extension: std::path::Path::new(filename)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_string(),
                    attributes: bitrate.map(|b| vec![
                        FileAttribute { attribute_type: 0, value: b }
                    ]).unwrap_or_default(),
                }
            }).collect(),
            has_slots: response.slot_free,
            avg_speed: response.avg_speed,
            queue_length: response.queue_length as u64,
            peer_ip: ip.to_string(),
            peer_port: port,
        }
    }

    /// Download a file from a peer using the modern QueueUpload method
    ///
    /// Returns DownloadResult which can be:
    /// - Completed: download finished successfully
    /// - Queued: peer has us in queue, will initiate F connection later
    /// - Failed: download failed
    ///
    /// The modern flow is:
    /// 1. Send QueueUpload to peer on P connection
    /// 2. Wait for peer's TransferRequest (direction=1)
    /// 3. Send TransferReply (allowed=true) automatically
    /// 4. Use peer::download_file to wait for F connection (via channel) and receive file
    ///
    /// This implementation directly copies the working test logic from e2e_search.rs
    ///
    /// progress_callback: Optional callback (bytes_downloaded, total_bytes, speed_kbps) for progress updates
    pub async fn download_file(
        &mut self,
        username: &str,
        filename: &str,
        filesize: u64,
        peer_ip: &str,
        peer_port: u32,
        output_path: &std::path::Path,
        progress_callback: Option<super::peer::ProgressCallback>,
    ) -> DownloadResult {
        tracing::info!("[Download] Starting download from '{}'", username);
        tracing::info!("[Download] File: {}", filename);
        tracing::info!("[Download] Peer address hint: {}:{}", peer_ip, peer_port);

        // Update status to pending
        {
            let mut downloads = self.downloads.lock().await;
            downloads.insert(filename.to_string(), DownloadProgress {
                filename: filename.to_string(),
                total_bytes: filesize,
                downloaded_bytes: 0,
                status: DownloadStatus::Pending,
            });
        }

        // Get stored P connection from search phase
        // The peer connected to US during search (they can reach us via UPnP)
        // This is critical - we can't connect to most peers due to their firewalls
        let stored_stream = self.peer_streams.lock().await.pop(username);

        let mut peer_stream = match stored_stream {
            Some(stream) => {
                tracing::info!("[Download] Using stored P connection from search");
                stream
            }
            None => {
                // No stored connection - try connecting to peer directly
                tracing::info!("[Download] No stored P connection, attempting direct connection");

                // Get peer address (from hint or from server)
                let (final_ip, final_port) = if !peer_ip.is_empty() && peer_port > 0 {
                    (peer_ip.to_string(), peer_port)
                } else {
                    match self.get_peer_address(username).await {
                        Ok((ip, port)) if port > 0 => (ip, port),
                        _ => {
                            let reason = "No stored P connection and cannot get peer address".to_string();
                            self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                            return DownloadResult::Failed { reason };
                        }
                    }
                };

                match self.connect_to_peer_for_download(username, &final_ip, final_port).await {
                    Ok(stream) => {
                        tracing::info!("[Download] Direct P connection established");
                        stream
                    }
                    Err(e) => {
                        let reason = format!("Failed to connect to peer: {}", e);
                        self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                        return DownloadResult::Failed { reason };
                    }
                }
            }
        };

        // MODERN METHOD: Use QueueUpload (what Nicotine+ and official clients use)
        // Step 1: Send QueueUpload - tells peer we want to download this file
        tracing::info!("[Download] Sending QueueUpload for '{}'...", filename);
        if let Err(e) = queue_upload(&mut peer_stream, filename).await {
            let reason = format!("Failed to send QueueUpload: {}", e);
            self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
            return DownloadResult::Failed { reason };
        }

        // Step 2: Wait for peer to send TransferRequest (direction=1) when ready
        // wait_for_transfer_request also sends our TransferReply (allowed=true) automatically
        tracing::info!("[Download] Waiting for peer's TransferRequest...");
        let negotiation = match wait_for_transfer_request(&mut peer_stream, filename, 60).await {
            Ok(result) => result,
            Err(e) => {
                let reason = format!("Negotiation failed: {}", e);
                self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                return DownloadResult::Failed { reason };
            }
        };

        // Handle the negotiation result
        match negotiation {
            NegotiationResult::Allowed { filesize: actual_size, transfer_token } => {
                tracing::info!("[Download] Transfer ALLOWED: size={} bytes, token={}", actual_size, transfer_token);

                // IMPORTANT: Release peer stream lock BEFORE calling peer_download_file
                // (following the pattern from e2e_search.rs line 601)
                drop(peer_stream);

                // Update status to in progress
                self.update_download_status(filename, DownloadStatus::InProgress).await;
                {
                    let mut downloads = self.downloads.lock().await;
                    if let Some(p) = downloads.get_mut(filename) {
                        p.total_bytes = actual_size;
                    }
                }

                // Get peer address - either from hint or request from server
                // (following e2e_search.rs lines 604-616)
                let (final_ip, final_port) = if peer_ip.is_empty() || peer_port == 0 {
                    tracing::info!("[Download] No peer address cached, requesting from server...");
                    match self.get_peer_address(username).await {
                        Ok((ip, port)) => (ip, port),
                        Err(e) => {
                            tracing::warn!("[Download] Could not get peer address: {} - continuing with empty", e);
                            (String::new(), 0)
                        }
                    }
                } else {
                    (peer_ip.to_string(), peer_port)
                };

                // Step 3: Use peer::download_file to wait for F connection and receive file
                // This is the WORKING implementation from the test
                tracing::info!("[Download] Calling peer_download_file (token={})...", transfer_token);

                // Get mutable access to the file connection receiver
                let mut file_conn_rx = self.file_conn_rx.lock().await;

                match peer_download_file(
                    &mut self.server_stream,
                    &final_ip,
                    final_port,
                    username,
                    &self.username,
                    filename,
                    actual_size,
                    transfer_token,
                    &mut file_conn_rx,
                    output_path,
                    progress_callback,
                ).await {
                    Ok(bytes) => {
                        self.update_download_status(filename, DownloadStatus::Completed).await;
                        {
                            let mut downloads = self.downloads.lock().await;
                            if let Some(p) = downloads.get_mut(filename) {
                                p.downloaded_bytes = bytes;
                            }
                        }
                        tracing::info!("[Download] COMPLETE: {} bytes downloaded", bytes);
                        DownloadResult::Completed { bytes }
                    }
                    Err(e) => {
                        let reason = format!("File transfer failed: {}", e);
                        self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                        DownloadResult::Failed { reason }
                    }
                }
            }

            NegotiationResult::Queued { transfer_token, position, reason } => {
                tracing::info!("[Download] QUEUED: {} (position: {:?})", reason, position);

                // Update status to queued
                self.update_download_status(filename, DownloadStatus::Queued).await;

                // Store as pending download - listener will handle F connection later
                {
                    let mut pending = self.pending_downloads.lock().await;
                    pending.insert(transfer_token, PendingDownload {
                        username: username.to_string(),
                        filename: filename.to_string(),
                        filesize,
                        transfer_token,
                        output_path: output_path.to_path_buf(),
                        queued_at: std::time::Instant::now(),
                        // Legacy path - no queue integration
                        item_id: None,
                        queue_id: None,
                        list_id: None,
                        progress_callback: None,
                        completion_tx: None,
                    });
                }

                DownloadResult::Queued { transfer_token, position, reason }
            }

            NegotiationResult::Denied { reason } => {
                tracing::info!("[Download] DENIED: {}", reason);
                self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                DownloadResult::Failed { reason }
            }
        }
    }

    /// Helper to update download status
    async fn update_download_status(&self, filename: &str, status: DownloadStatus) {
        let mut downloads = self.downloads.lock().await;
        if let Some(p) = downloads.get_mut(filename) {
            p.status = status;
        }
    }

    /// Initiate a download for the queue system
    ///
    /// This method performs the complete download negotiation flow:
    /// 1. Establishes/reuses P connection with peer
    /// 2. Sends QueueUpload request
    /// 3. Waits for TransferRequest/TransferReply exchange
    /// 4. Waits for F connection (direct or server-brokered)
    /// 5. Spawns file receive as background task
    ///
    /// The method blocks during negotiation and F connection establishment,
    /// but the actual file transfer runs concurrently in the background.
    ///
    /// # Arguments
    /// - username, filename, filesize, peer_ip, peer_port: File info from search
    /// - output_path: Where to save the downloaded file
    /// - item_id, queue_id, list_id: Queue integration IDs for tracking
    /// - progress_callback: Optional callback for progress updates
    /// - completion_tx: Channel to send completion notifications
    ///
    /// # Returns
    /// - TransferSpawned: File transfer started in background
    /// - Queued: Peer queued us, transfer will happen later (registered as pending)
    /// - Failed: Negotiation or connection failed
    pub async fn initiate_download(
        &mut self,
        username: &str,
        filename: &str,
        filesize: u64,
        peer_ip: &str,
        peer_port: u32,
        output_path: &std::path::Path,
        item_id: i64,
        queue_id: i64,
        list_id: Option<i64>,
        progress_callback: Option<super::peer::ArcProgressCallback>,
        completion_tx: TransferCompleteSender,
    ) -> DownloadInitResult {
        tracing::info!(
            "[Download] === INITIATE DOWNLOAD START === item_id={}, queue_id={}",
            item_id, queue_id
        );
        tracing::info!("[Download] User: '{}', IP: {}, Port: {}", username, peer_ip, peer_port);
        tracing::info!("[Download] File: {} ({} bytes)", filename, filesize);

        // Update internal status to pending
        {
            let mut downloads = self.downloads.lock().await;
            downloads.insert(filename.to_string(), DownloadProgress {
                filename: filename.to_string(),
                total_bytes: filesize,
                downloaded_bytes: 0,
                status: DownloadStatus::Pending,
            });
        }

        // Step 1: Get or establish P connection with peer
        tracing::debug!("[Download] Step 1: Getting P connection for '{}'", username);
        let cached_count = self.peer_streams.lock().await.len();
        tracing::debug!("[Download] peer_streams cache has {} entries", cached_count);
        let stored_stream = self.peer_streams.lock().await.pop(username);

        let mut peer_stream = match stored_stream {
            Some(stream) => {
                tracing::info!("[Download] Using stored P connection from search for '{}'", username);
                stream
            }
            None => {
                tracing::info!("[Download] No stored P connection for '{}', attempting direct connection", username);
                tracing::debug!("[Download] Available peers in cache: {:?}",
                    self.peer_streams.lock().await.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>());

                let (final_ip, final_port) = if !peer_ip.is_empty() && peer_port > 0 {
                    tracing::debug!("[Download] Using provided address: {}:{}", peer_ip, peer_port);
                    (peer_ip.to_string(), peer_port)
                } else {
                    tracing::debug!("[Download] No address provided, requesting from server");
                    match self.get_peer_address(username).await {
                        Ok((ip, port)) if port > 0 => {
                            tracing::debug!("[Download] Got address from server: {}:{}", ip, port);
                            (ip, port)
                        }
                        Ok((ip, port)) => {
                            tracing::debug!("[Download] Server returned invalid address: {}:{}", ip, port);
                            let reason = "No stored P connection and cannot get peer address".to_string();
                            self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                            return DownloadInitResult::Failed { reason };
                        }
                        Err(e) => {
                            tracing::debug!("[Download] Failed to get address from server: {}", e);
                            let reason = "No stored P connection and cannot get peer address".to_string();
                            self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                            return DownloadInitResult::Failed { reason };
                        }
                    }
                };

                tracing::debug!("[Download] Connecting to peer at {}:{}", final_ip, final_port);
                match self.connect_to_peer_for_download(username, &final_ip, final_port).await {
                    Ok(stream) => {
                        tracing::info!("[Download] Direct P connection established to '{}'", username);
                        stream
                    }
                    Err(e) => {
                        tracing::info!("[Download] Direct P connection FAILED to '{}': {}", username, e);
                        let reason = format!("Failed to connect to peer: {}", e);
                        self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                        return DownloadInitResult::Failed { reason };
                    }
                }
            }
        };

        // Step 2: Send QueueUpload request
        tracing::info!("[Download] Step 2: Sending QueueUpload for '{}'", filename);
        if let Err(e) = queue_upload(&mut peer_stream, filename).await {
            tracing::info!("[Download] QueueUpload FAILED: {}", e);
            let reason = format!("Failed to send QueueUpload: {}", e);
            self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
            return DownloadInitResult::Failed { reason };
        }
        tracing::debug!("[Download] QueueUpload sent successfully");

        // Step 3: Wait for TransferRequest and send TransferReply
        tracing::info!("[Download] Step 3: Waiting for peer's TransferRequest (60s timeout)...");
        let negotiation = match wait_for_transfer_request(&mut peer_stream, filename, 60).await {
            Ok(result) => {
                tracing::debug!("[Download] Negotiation result: {:?}", result);
                result
            }
            Err(e) => {
                tracing::info!("[Download] Negotiation FAILED: {}", e);
                let reason = format!("Negotiation failed: {}", e);
                self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                return DownloadInitResult::Failed { reason };
            }
        };

        // Release P connection - negotiation is complete
        tracing::debug!("[Download] Releasing P connection after negotiation");
        drop(peer_stream);

        // Handle negotiation result
        match negotiation {
            NegotiationResult::Allowed { filesize: actual_size, transfer_token } => {
                tracing::info!(
                    "[Download] Step 4: Transfer ALLOWED - size={} bytes, token={}",
                    actual_size, transfer_token
                );

                // Update status to in progress
                self.update_download_status(filename, DownloadStatus::InProgress).await;
                {
                    let mut downloads = self.downloads.lock().await;
                    if let Some(p) = downloads.get_mut(filename) {
                        p.total_bytes = actual_size;
                    }
                }

                // Step 5: Wait for F connection (direct or server-brokered)
                // This is critical - we must actively wait for the connection,
                // not just register and hope the listener catches it
                tracing::info!("[Download] Step 5: Waiting for F connection (token={}, 30s timeout)...", transfer_token);

                let file_conn = {
                    let mut file_conn_rx = self.file_conn_rx.lock().await;
                    tracing::debug!("[Download] Acquired file_conn_rx lock, calling wait_for_file_connection");
                    super::peer::wait_for_file_connection(
                        &mut self.server_stream,
                        username,
                        &self.username,
                        transfer_token,
                        &mut file_conn_rx,
                        30, // 30 second timeout for F connection
                    ).await
                };

                match file_conn {
                    Ok(conn) => {
                        tracing::info!("[Download] Step 6: F connection established (token={}), spawning file transfer", conn.transfer_token);

                        // Step 6: Spawn file receive as background task
                        let output_path = output_path.to_path_buf();
                        let downloads_clone = Arc::clone(&self.downloads);
                        let filename_clone = filename.to_string();

                        tokio::spawn(async move {
                            tracing::info!("[Transfer] Starting receive_file_data for '{}'", filename_clone);
                            let result = super::peer::receive_file_data(
                                conn,
                                actual_size,
                                output_path,
                                progress_callback,
                            ).await;

                            // Update download status
                            match &result {
                                Ok(bytes) => {
                                    tracing::info!("[Transfer] === DOWNLOAD COMPLETE === {} bytes for '{}'", bytes, filename_clone);
                                    let mut dl = downloads_clone.lock().await;
                                    if let Some(p) = dl.get_mut(&filename_clone) {
                                        p.status = DownloadStatus::Completed;
                                        p.downloaded_bytes = *bytes;
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("[Transfer] === DOWNLOAD FAILED === '{}': {}", filename_clone, e);
                                    let mut dl = downloads_clone.lock().await;
                                    if let Some(p) = dl.get_mut(&filename_clone) {
                                        p.status = DownloadStatus::Failed(e.to_string());
                                    }
                                }
                            }

                            // Send completion notification
                            let transfer_result = result.map(|b| b).map_err(|e| e.to_string());
                            let _ = completion_tx.send(TransferComplete {
                                item_id,
                                queue_id,
                                list_id,
                                transfer_token,
                                result: transfer_result,
                            }).await;
                        });

                        tracing::info!("[Download] === INITIATE DOWNLOAD END === TransferSpawned for item_id={}", item_id);
                        DownloadInitResult::TransferSpawned {
                            item_id,
                            transfer_token,
                        }
                    }
                    Err(e) => {
                        let reason = format!("Failed to establish F connection: {}", e);
                        tracing::error!("[Download] === INITIATE DOWNLOAD FAILED === F connection error: {}", reason);
                        self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;

                        // Notify of failure
                        let _ = completion_tx.send(TransferComplete {
                            item_id,
                            queue_id,
                            list_id,
                            transfer_token,
                            result: Err(reason.clone()),
                        }).await;

                        DownloadInitResult::Failed { reason }
                    }
                }
            }

            NegotiationResult::Queued { transfer_token, position, reason } => {
                // Peer has queued our request - they'll initiate transfer later
                // We register as pending so the listener can handle when F connection arrives
                tracing::info!("[Download] QUEUED by peer: {} (position: {:?})", reason, position);

                self.update_download_status(filename, DownloadStatus::Queued).await;

                // Register as pending download - listener will handle F connection later
                {
                    let mut pending = self.pending_downloads.lock().await;
                    pending.insert(transfer_token, PendingDownload {
                        username: username.to_string(),
                        filename: filename.to_string(),
                        filesize,
                        transfer_token,
                        output_path: output_path.to_path_buf(),
                        queued_at: std::time::Instant::now(),
                        item_id: Some(item_id),
                        queue_id: Some(queue_id),
                        list_id,
                        progress_callback,
                        completion_tx: Some(completion_tx),
                    });
                }

                DownloadInitResult::Queued {
                    transfer_token,
                    position,
                    reason,
                }
            }

            NegotiationResult::Denied { reason } => {
                tracing::info!("[Download] DENIED by peer: {}", reason);
                self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;

                // Notify of failure
                let _ = completion_tx.send(TransferComplete {
                    item_id,
                    queue_id,
                    list_id,
                    transfer_token: 0,
                    result: Err(reason.clone()),
                }).await;

                DownloadInitResult::Failed { reason }
            }
        }
    }

    /// Connect to peer for download negotiation
    /// If we have the peer's address, connect directly
    /// Otherwise, request their address from the server
    async fn connect_to_peer_for_download(
        &mut self,
        username: &str,
        peer_ip: &str,
        peer_port: u32,
    ) -> Result<TcpStream> {
        // If we have a valid peer address, connect directly
        if peer_port > 0 && !peer_ip.is_empty() && peer_ip != "0.0.0.0" {
            tracing::info!("[Download] Connecting directly to {}:{}", peer_ip, peer_port);
            let addr = format!("{}:{}", peer_ip, peer_port);
            let mut stream = tokio::time::timeout(
                Duration::from_secs(10),
                TcpStream::connect(&addr)
            ).await
                .context("Connection timeout")?
                .context("Connection failed")?;

            // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
            stream.set_nodelay(true)?;

            // Send PeerInit
            super::peer::send_peer_init(&mut stream, &self.username, "P", 0).await?;

            return Ok(stream);
        }

        // No direct address - request from server
        tracing::info!("[Download] Requesting peer address from server for '{}'", username);

        // Send GetPeerAddress request
        let mut request = BytesMut::new();
        request.put_u32_le(3); // GetPeerAddress code
        super::peer::encode_string(&mut request, username);

        let msg_len = request.len() as u32;
        self.server_stream.write_u32_le(msg_len).await?;
        self.server_stream.write_all(&request).await?;
        self.server_stream.flush().await?;

        // Wait for PeerAddress response
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(10);

        while start.elapsed() < timeout {
            match tokio::time::timeout(
                Duration::from_millis(200),
                read_message(&mut self.server_stream)
            ).await {
                Ok(Ok((3, data))) => {
                    // PeerAddress response
                    // Format: username (string), IP (4 bytes network order), port (u32 LE)
                    let mut buf = BytesMut::from(&data[..]);
                    let resp_username = decode_string(&mut buf)?;

                    // IP is 4 bytes in network byte order (big-endian)
                    if buf.remaining() < 8 {
                        tracing::warn!("[Download] Invalid PeerAddress response - not enough bytes");
                        continue;
                    }
                    let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                    let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                    let port = buf.get_u32_le();

                    if resp_username.to_lowercase() == username.to_lowercase() {
                        tracing::info!("[Download] Got peer address: {}:{}", ip, port);

                        if port == 0 || ip.is_empty() {
                            bail!("Peer address not available (port={})", port);
                        }

                        // Connect to peer
                        let addr = format!("{}:{}", ip, port);
                        let mut stream = tokio::time::timeout(
                            Duration::from_secs(10),
                            TcpStream::connect(&addr)
                        ).await
                            .context("Connection timeout")?
                            .context("Connection failed")?;

                        // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
                        stream.set_nodelay(true)?;

                        // Send PeerInit
                        super::peer::send_peer_init(&mut stream, &self.username, "P", 0).await?;

                        return Ok(stream);
                    }
                }
                Ok(Ok((18, data))) => {
                    // ConnectToPeer - might be the server telling us to connect
                    let mut buf = BytesMut::from(&data[..]);
                    let peer_username = decode_string(&mut buf)?;
                    let conn_type = decode_string(&mut buf)?;
                    let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                    let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                    let port = buf.get_u32_le();
                    let peer_token = buf.get_u32_le();

                    if peer_username.to_lowercase() == username.to_lowercase() && conn_type == "P" {
                        tracing::info!("[Download] ConnectToPeer received: {}:{}", ip, port);

                        // Connect using ConnectToPeer info
                        match super::peer::connect_to_peer_for_results(&ip, port, &peer_username, &self.username, peer_token).await {
                            Ok((stream, _)) => return Ok(stream),
                            Err(e) => {
                                tracing::warn!("[Download] ConnectToPeer connect failed: {}", e);
                                // Continue waiting
                            }
                        }
                    }
                }
                Ok(Ok(_)) => {
                    // Other message, ignore
                }
                Ok(Err(e)) => {
                    bail!("Server error: {}", e);
                }
                Err(_) => {
                    // Timeout, continue
                }
            }
        }

        bail!("Timeout waiting for peer address")
    }

    /// Wait for F connection - monitors both listener channel and server stream
    async fn wait_for_f_connection(
        &mut self,
        peer_username: &str,
        peer_ip: &str,
        peer_port: u32,
        transfer_token: u32,
        timeout: Duration,
    ) -> Result<FileConnection> {
        let start = std::time::Instant::now();

        // Phase 1: Wait for direct F connection from listener OR ConnectToPeer F from server
        while start.elapsed() < timeout {
            tokio::select! {
                // Check for direct F connection from listener
                result = async {
                    let mut rx = self.file_conn_rx.lock().await;
                    tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
                } => {
                    if let Ok(Some(conn)) = result {
                        if conn.transfer_token == transfer_token {
                            tracing::info!("[Download] Got direct F connection");
                            return Ok(conn);
                        } else {
                            tracing::debug!("[Download] Token mismatch: expected {}, got {}",
                                transfer_token, conn.transfer_token);
                        }
                    }
                }

                // Check server for ConnectToPeer F message (indirect path)
                result = tokio::time::timeout(
                    Duration::from_millis(100),
                    read_message(&mut self.server_stream)
                ) => {
                    match result {
                        Ok(Ok((18, data))) => {
                            // ConnectToPeer message
                            let mut buf = BytesMut::from(&data[..]);
                            let username = decode_string(&mut buf)?;
                            let conn_type = decode_string(&mut buf)?;
                            let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                            let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                            let port = buf.get_u32_le();
                            let indirect_token = buf.get_u32_le();

                            if conn_type == "F" && username == peer_username {
                                tracing::info!("[Download] ConnectToPeer F - connecting to {}:{}", ip, port);
                                match super::peer::connect_for_indirect_transfer(&ip, port, indirect_token, &self.username).await {
                                    Ok(conn) => return Ok(conn),
                                    Err(e) => tracing::warn!("[Download] Indirect F failed: {}", e),
                                }
                            } else if conn_type == "P" {
                                // Peer search connection during download - ignore
                                tracing::debug!("[Download] Ignoring P connection from {}", username);
                            }
                        }
                        Ok(Ok((code, _))) => {
                            tracing::trace!("[Download] Ignoring server message {}", code);
                        }
                        Ok(Err(e)) => {
                            tracing::warn!("[Download] Server read error: {}", e);
                        }
                        Err(_) => {
                            // Timeout, continue loop
                        }
                    }
                }
            }
        }

        // Phase 2: Fallback - try proactive F connection to peer
        // If we have their address, connect directly
        if !peer_ip.is_empty() && peer_port > 0 {
            tracing::info!("[Download] Trying proactive F connection to {}:{}", peer_ip, peer_port);
            return self.proactive_f_connection(peer_ip, peer_port, transfer_token).await;
        }

        // If we don't have their port, request it from server
        tracing::info!("[Download] Requesting peer address for proactive F connection...");
        if let Ok((ip, port)) = self.get_peer_address(peer_username).await {
            if port > 0 {
                tracing::info!("[Download] Got address {}:{}, trying proactive F connection", ip, port);
                return self.proactive_f_connection(&ip, port, transfer_token).await;
            }
        }

        bail!("Timeout waiting for F connection (peer address not available)")
    }

    /// Request peer's address from server
    async fn get_peer_address(&mut self, username: &str) -> Result<(String, u32)> {
        // Send GetPeerAddress request (code 3)
        let mut request = BytesMut::new();
        request.put_u32_le(3); // GetPeerAddress code
        encode_string(&mut request, username);

        let msg_len = request.len() as u32;
        self.server_stream.write_u32_le(msg_len).await?;
        self.server_stream.write_all(&request).await?;
        self.server_stream.flush().await?;

        // Wait for PeerAddress response (code 3)
        let timeout = Duration::from_secs(10);
        let start = std::time::Instant::now();

        while start.elapsed() < timeout {
            match tokio::time::timeout(
                Duration::from_millis(200),
                read_message(&mut self.server_stream)
            ).await {
                Ok(Ok((3, data))) => {
                    // PeerAddress response
                    // Format: username (string), IP (4 bytes network order), port (u32 LE)
                    let mut buf = BytesMut::from(&data[..]);
                    let resp_username = decode_string(&mut buf)?;

                    // IP is 4 bytes in network byte order (big-endian)
                    if buf.remaining() < 8 {
                        tracing::warn!("[Download] Invalid PeerAddress response - not enough bytes");
                        continue;
                    }
                    let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                    let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                    let port = buf.get_u32_le();

                    if resp_username.to_lowercase() == username.to_lowercase() {
                        tracing::info!("[Download] GetPeerAddress response: {}:{}", ip, port);
                        return Ok((ip, port));
                    }
                }
                Ok(Ok(_)) => {
                    // Other message, continue
                }
                Ok(Err(e)) => {
                    bail!("Server error: {}", e);
                }
                Err(_) => {
                    // Timeout, continue
                }
            }
        }

        bail!("Timeout getting peer address")
    }

    /// Try proactive F connection to peer (fallback when they can't connect to us)
    async fn proactive_f_connection(
        &self,
        peer_ip: &str,
        peer_port: u32,
        transfer_token: u32,
    ) -> Result<FileConnection> {
        let addr = format!("{}:{}", peer_ip, peer_port);
        let mut stream = tokio::time::timeout(
            Duration::from_secs(10),
            TcpStream::connect(&addr)
        ).await
            .context("Connection timeout")?
            .context("Connection failed")?;

        // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
        stream.set_nodelay(true)?;

        tracing::debug!("[Download] Connected, sending PeerInit type F");

        // Send PeerInit with type F
        let mut data = BytesMut::new();
        encode_string(&mut data, &self.username);
        encode_string(&mut data, "F");
        data.put_u32_le(0); // token 0 for F connection

        let msg_len = 1 + data.len() as u32;
        stream.write_u32_le(msg_len).await?;
        stream.write_u8(1).await?; // PeerInit message type
        stream.write_all(&data).await?;
        stream.flush().await?;

        // Wait for peer's response
        match tokio::time::timeout(Duration::from_secs(5), stream.read_u32_le()).await {
            Ok(Ok(msg_len)) => {
                if msg_len > 5 && msg_len < 200 {
                    let msg_type = stream.read_u8().await?;
                    if msg_type == 1 {
                        let remaining = (msg_len - 1) as usize;
                        let mut data = vec![0u8; remaining];
                        stream.read_exact(&mut data).await?;
                        tracing::debug!("[Download] Got PeerInit response");
                    }
                }
            }
            _ => tracing::debug!("[Download] No PeerInit response, continuing"),
        }

        // Send FileTransferInit with transfer token
        tracing::debug!("[Download] Sending FileTransferInit token={}", transfer_token);
        stream.write_u32_le(transfer_token).await?;
        stream.flush().await?;

        Ok(FileConnection { stream, transfer_token })
    }

    /// Receive file data from F connection
    async fn receive_file(
        &self,
        mut conn: FileConnection,
        filesize: u64,
        output_path: &std::path::Path,
        filename: &str,  // For progress tracking
    ) -> Result<u64> {
        // Send FileOffset (start from 0)
        conn.stream.write_u64_le(0).await?;
        conn.stream.flush().await?;

        // Create output directory
        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut file = tokio::fs::File::create(output_path).await?;
        let mut total = 0u64;
        let mut buffer = vec![0u8; 65536];
        let start = std::time::Instant::now();
        let mut last_update = 0u64;

        loop {
            match tokio::time::timeout(Duration::from_secs(30), conn.stream.read(&mut buffer)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    tokio::io::AsyncWriteExt::write_all(&mut file, &buffer[..n]).await?;
                    total += n as u64;

                    // Update progress every 100KB (for real-time WebSocket updates)
                    if total - last_update >= 102400 {
                        last_update = total;
                        let mut downloads = self.downloads.lock().await;
                        if let Some(p) = downloads.get_mut(filename) {
                            p.downloaded_bytes = total;
                        }
                    }

                    // Log progress every MB
                    if total % (1024 * 1024) < 65536 {
                        let elapsed = start.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.0 { total as f64 / elapsed / 1024.0 } else { 0.0 };
                        tracing::info!(
                            "[Download] {:.2}/{:.2} MB ({:.1}%) - {:.1} KB/s",
                            total as f64 / 1_048_576.0,
                            filesize as f64 / 1_048_576.0,
                            (total as f64 / filesize as f64) * 100.0,
                            speed
                        );
                    }

                    if total >= filesize {
                        break;
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("[Download] Read error: {}", e);
                    break;
                }
                Err(_) => {
                    tracing::warn!("[Download] Timeout");
                    break;
                }
            }
        }

        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        tracing::info!("[Download] Complete: {} bytes to {:?}", total, output_path);

        Ok(total)
    }

    /// Static version of receive_file for use in spawned tasks
    async fn receive_file_static(
        mut conn: FileConnection,
        filesize: u64,
        output_path: &std::path::Path,
        filename: &str,
        downloads: Arc<Mutex<HashMap<String, DownloadProgress>>>,
    ) -> Result<u64> {
        // Send FileOffset (start from 0)
        conn.stream.write_u64_le(0).await?;
        conn.stream.flush().await?;

        // Create output directory
        if let Some(parent) = output_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut file = tokio::fs::File::create(output_path).await?;
        let mut total = 0u64;
        let mut buffer = vec![0u8; 65536];
        let start = std::time::Instant::now();
        let mut last_update = 0u64;

        loop {
            match tokio::time::timeout(Duration::from_secs(30), conn.stream.read(&mut buffer)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    tokio::io::AsyncWriteExt::write_all(&mut file, &buffer[..n]).await?;
                    total += n as u64;

                    // Update progress every 100KB (for real-time WebSocket updates)
                    if total - last_update >= 102400 {
                        last_update = total;
                        let mut dl = downloads.lock().await;
                        if let Some(p) = dl.get_mut(filename) {
                            p.downloaded_bytes = total;
                        }
                    }

                    // Log progress every MB
                    if total % (1024 * 1024) < 65536 {
                        let elapsed = start.elapsed().as_secs_f64();
                        let speed = if elapsed > 0.0 { total as f64 / elapsed / 1024.0 } else { 0.0 };
                        tracing::info!(
                            "[Queued Download] {:.2}/{:.2} MB ({:.1}%) - {:.1} KB/s",
                            total as f64 / 1_048_576.0,
                            filesize as f64 / 1_048_576.0,
                            (total as f64 / filesize as f64) * 100.0,
                            speed
                        );
                    }

                    if total >= filesize {
                        break;
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!("[Queued Download] Read error: {}", e);
                    break;
                }
                Err(_) => {
                    tracing::warn!("[Queued Download] Timeout");
                    break;
                }
            }
        }

        tokio::io::AsyncWriteExt::flush(&mut file).await?;
        tracing::info!("[Queued Download] Complete: {} bytes to {:?}", total, output_path);

        Ok(total)
    }

    /// Get download progress
    pub async fn get_download_progress(&self, filename: &str) -> Option<DownloadProgress> {
        self.downloads.lock().await.get(filename).cloned()
    }

    /// Get all downloads
    pub async fn get_all_downloads(&self) -> Vec<DownloadProgress> {
        self.downloads.lock().await.values().cloned().collect()
    }

    /// Get the best file from search results using sophisticated scoring
    /// Returns ScoredFile with username, filename, size, bitrate, score, peer_ip, peer_port
    /// format_filter: Optional format filter ("mp3", "flac", "m4a", "wav")
    pub fn get_best_file(results: &[SearchResult], query: &str, format_filter: Option<&str>) -> Option<ScoredFile> {
        find_best_file(results, query, format_filter)
    }

    /// Get all matching files sorted by score (best first)
    /// Use this when you need to try multiple candidates on failure
    pub fn get_best_files(results: &[SearchResult], query: &str, format_filter: Option<&str>) -> Vec<ScoredFile> {
        find_best_files(results, query, format_filter)
    }

    /// Search and download best matching file
    /// Search and download - returns (ScoredFile, DownloadResult)
    pub async fn search_and_download(
        &mut self,
        query: &str,
        output_path: &std::path::Path,
    ) -> Result<(ScoredFile, DownloadResult)> {
        // Search
        let results = self.search(query, 10).await?;

        if results.is_empty() {
            bail!("No results found for query: {}", query);
        }

        // Find best file using sophisticated scoring (no format filter for this internal method)
        let scored_file = Self::get_best_file(&results, query, None)
            .ok_or_else(|| anyhow::anyhow!("No suitable files found matching query"))?;

        tracing::info!(
            "[SoulseekClient] Selected '{}' from '{}' (score={:.1}, {:?} kbps)",
            scored_file.filename,
            scored_file.username,
            scored_file.score,
            scored_file.bitrate
        );

        // Download
        let result = self.download_file(
            &scored_file.username,
            &scored_file.filename,
            scored_file.size,
            &scored_file.peer_ip,
            scored_file.peer_port,
            output_path,
            None,  // No progress callback for this internal method
        ).await;

        Ok((scored_file, result))
    }
}
