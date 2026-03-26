//! Soulseek client with concurrent search and download support.
//!
//! Uses a ServerRouter to multiplex the single server TCP connection,
//! enabling concurrent searches and downloads without requiring &mut self.
//! The server stream is split into read/write halves: the router's reader task
//! dispatches incoming messages, while the writer is shared via a Mutex.

use anyhow::{Result, bail, Context};
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use lru::LruCache;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex, RwLock};
use tokio::net::{TcpStream, TcpListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use bytes::{Buf, BufMut, BytesMut};

use super::messages::{SearchFile, FileAttribute, decode_string};
use super::peer::{
    FileConnection, IncomingConnection, handle_incoming_connection,
    connect_to_peer_for_dispatch,
    queue_upload, wait_for_transfer_request,
    NegotiationResult, ParsedSearchResponse,
    send_message, read_message, encode_string,
};
use super::file_selection::{find_best_file, find_best_files, ScoredFile};
use super::router::{ServerRouter, ConnectToPeerMsg};

use crate::services::sharing::SharingService;
use crate::services::upload::UploadService;

/// Default port for incoming peer connections
pub const PEER_LISTEN_PORT: u16 = 2234;
/// Obfuscated port for incoming peer connections (PEER_LISTEN_PORT + 1)
pub const PEER_OBFUSCATED_PORT: u16 = 2235;

const SERVER_HOST: &str = "server.slsknet.org";
const SERVER_PORT: u16 = 2242;

/// Maximum number of peer connections to cache for potential downloads
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
    pub peer_ip: String,
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
    Completed { bytes: u64 },
    TransferStarted { task_id: u64 },
    Queued { transfer_token: u32, position: Option<u32>, reason: String },
    Failed { reason: String },
}

/// Progress callback type for reporting download progress
/// Parameters: (downloaded_bytes, total_bytes)
/// Note: For the 3-arg version with speed, use peer::ProgressCallback
pub type ProgressCallback = Box<dyn Fn(u64, u64) + Send + Sync>;

/// Generic transfer completion notification
#[derive(Debug)]
pub struct TransferComplete {
    pub item_id: i64,
    pub queue_id: i64,
    pub list_id: Option<i64>,
    pub transfer_token: u32,
    pub result: Result<u64, String>,
}

/// Sender for transfer completion notifications
pub type TransferCompleteSender = mpsc::Sender<TransferComplete>;

/// A pending (queued) download waiting for F connection
pub struct PendingDownload {
    pub username: String,
    pub filename: String,
    pub filesize: u64,
    pub transfer_token: u32,
    pub output_path: std::path::PathBuf,
    pub queued_at: std::time::Instant,
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
    TransferSpawned { item_id: i64, transfer_token: u32 },
    Queued { transfer_token: u32, position: Option<u32>, reason: String },
    Failed { reason: String },
}

/// Main Soulseek client with concurrent search/download support.
///
/// All public methods take `&self` (not `&mut self`), allowing the client
/// to be shared via `Arc<SoulseekClient>` across concurrent tasks.
pub struct SoulseekClient {
    /// Server message router (handles read/write multiplexing)
    router: ServerRouter,
    /// Our username
    username: String,
    /// Sender for F connections (used by listener and file conn dispatcher)
    file_conn_tx: mpsc::Sender<FileConnection>,
    /// Stored peer streams for download (username -> stream)
    peer_streams: Arc<Mutex<LruCache<String, TcpStream>>>,
    /// Per-token search results from ConnectToPeer connections and listener
    /// Key is our search token, value is accumulated results
    search_results: Arc<Mutex<HashMap<u32, Vec<SearchResult>>>>,
    /// Download progress tracking
    downloads: Arc<Mutex<HashMap<String, DownloadProgress>>>,
    /// Pending downloads waiting for F connection (transfer_token -> PendingDownload)
    pending_downloads: Arc<Mutex<HashMap<u32, PendingDownload>>>,
    /// Per-download F connection waiters (transfer_token -> oneshot sender)
    /// The file conn dispatcher routes incoming F connections to the matching waiter
    file_conn_waiters: Arc<Mutex<HashMap<u32, oneshot::Sender<FileConnection>>>>,
    /// Set of currently active search tokens (for routing incoming results)
    active_search_tokens: Arc<Mutex<std::collections::HashSet<u32>>>,
    /// Notification channel for when new search results arrive
    result_notify: tokio::sync::broadcast::Sender<u32>,
    /// Sharing service for responding to peer file requests (set after construction)
    sharing_service: Arc<RwLock<Option<Arc<SharingService>>>>,
    /// Upload service for handling upload queue requests (set after construction)
    upload_service: Arc<RwLock<Option<Arc<UploadService>>>>,
    /// Our external IP as reported by the server (for loopback detection)
    external_ip: Option<String>,
}

impl SoulseekClient {
    /// Connect to Soulseek server and login.
    ///
    /// Returns `Arc<Self>` since the client is designed to be shared.
    pub async fn connect(username: &str, password: &str) -> Result<Arc<Self>> {
        let addr = format!("{}:{}", SERVER_HOST, SERVER_PORT);
        tracing::info!("[SoulseekClient] Connecting to {}", addr);

        let mut stream = TcpStream::connect(&addr)
            .await
            .context("Failed to connect to Soulseek server")?;

        stream.set_nodelay(true)?;

        // Login (returns our external IP as reported by the server)
        let external_ip = Self::login(&mut stream, username, password).await?;
        tracing::info!("[SoulseekClient] Logged in as '{}'", username);

        // Set shared counts
        Self::set_shared_counts(&mut stream, 0, 0).await?;

        // Set wait port
        Self::set_wait_port(&mut stream, PEER_LISTEN_PORT as u32, Some(PEER_OBFUSCATED_PORT as u32)).await?;
        tracing::info!("[SoulseekClient] Set wait port to {} (obfuscated: {})", PEER_LISTEN_PORT, PEER_OBFUSCATED_PORT);

        // Split the server stream for the router
        let (read_half, write_half) = stream.into_split();

        // Create channels
        let (file_conn_tx, file_conn_rx) = mpsc::channel::<FileConnection>(50);
        let (connect_to_peer_tx, connect_to_peer_rx) = mpsc::channel::<ConnectToPeerMsg>(200);

        // Create router
        let router = ServerRouter::new(
            read_half,
            write_half,
            file_conn_tx.clone(),
            connect_to_peer_tx,
            username.to_string(),
            external_ip.clone(),
        );

        // Create notification channel for search results
        let (result_notify, _) = tokio::sync::broadcast::channel(256);

        let client = Arc::new(Self {
            router,
            username: username.to_string(),
            file_conn_tx,
            peer_streams: Arc::new(Mutex::new(LruCache::new(
                NonZeroUsize::new(MAX_CACHED_PEER_CONNECTIONS).unwrap()
            ))),
            search_results: Arc::new(Mutex::new(HashMap::new())),
            downloads: Arc::new(Mutex::new(HashMap::new())),
            pending_downloads: Arc::new(Mutex::new(HashMap::new())),
            file_conn_waiters: Arc::new(Mutex::new(HashMap::new())),
            active_search_tokens: Arc::new(Mutex::new(std::collections::HashSet::new())),
            result_notify,
            sharing_service: Arc::new(RwLock::new(None)),
            upload_service: Arc::new(RwLock::new(None)),
            external_ip,
        });

        // Start peer listeners
        client.start_listener();

        // Start ConnectToPeer "P" handler (routes server-brokered peer connections)
        client.start_connect_to_peer_handler(connect_to_peer_rx);

        // Start file connection dispatcher (routes F connections to per-download waiters)
        client.start_file_conn_dispatcher(file_conn_rx);

        Ok(client)
    }

    /// Login to the server. Returns our external IP as reported by the server.
    async fn login(stream: &mut TcpStream, username: &str, password: &str) -> Result<Option<String>> {
        let combined = format!("{}{}", username, password);
        let md5_hash = format!("{:x}", md5::compute(combined.as_bytes()));

        let mut data = BytesMut::new();
        encode_string(&mut data, username);
        encode_string(&mut data, password);
        data.put_u32_le(160);
        encode_string(&mut data, &md5_hash);
        data.put_u32_le(1);

        send_message(stream, 1, &data).await?;

        let (code, response_data) = read_message(stream).await?;
        if code == 1 {
            let mut buf = BytesMut::from(&response_data[..]);
            let success = buf.get_u8() != 0;
            let greeting_or_reason = decode_string(&mut buf)?;

            if !success {
                bail!("Login failed: {}", greeting_or_reason);
            }

            // Parse our external IP from the login response (4 bytes, reversed)
            if buf.remaining() >= 4 {
                let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                let external_ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                tracing::info!("[SoulseekClient] Server reports our external IP: {}", external_ip);
                return Ok(Some(external_ip));
            }
        }

        Ok(None)
    }

    /// Set wait port
    async fn set_wait_port(stream: &mut TcpStream, port: u32, obfuscated_port: Option<u32>) -> Result<()> {
        let mut data = BytesMut::new();
        data.put_u32_le(port);
        if let Some(obfs_port) = obfuscated_port {
            data.put_u32_le(1);
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

    /// Set the sharing service (called after construction since services are created after connect)
    pub async fn set_sharing_service(&self, service: Arc<SharingService>) {
        let mut s = self.sharing_service.write().await;
        *s = Some(service);
    }

    /// Set the upload service (called after construction)
    pub async fn set_upload_service(&self, service: Arc<UploadService>) {
        let mut s = self.upload_service.write().await;
        *s = Some(service);
    }

    /// Get the server router (for sending messages)
    pub fn router(&self) -> &ServerRouter {
        &self.router
    }

    /// Get our username
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Get our external IP (for loopback detection)
    pub fn external_ip(&self) -> Option<&str> {
        self.external_ip.as_deref()
    }

    // ========================================================================
    // Background tasks
    // ========================================================================

    /// Start listeners for incoming P and F connections on both ports
    fn start_listener(self: &Arc<Self>) {
        self.start_listener_on_port(PEER_LISTEN_PORT, false);
        self.start_listener_on_port(PEER_OBFUSCATED_PORT, true);
    }

    /// Start a listener on a specific port
    fn start_listener_on_port(self: &Arc<Self>, port: u16, is_obfuscated: bool) {
        let file_conn_tx = self.file_conn_tx.clone();
        let peer_streams = Arc::clone(&self.peer_streams);
        let search_results = Arc::clone(&self.search_results);
        let active_search_tokens = Arc::clone(&self.active_search_tokens);
        let pending_downloads = Arc::clone(&self.pending_downloads);
        let downloads = Arc::clone(&self.downloads);
        let result_notify = self.result_notify.clone();
        let username = self.username.clone();
        let sharing_service = Arc::clone(&self.sharing_service);
        let upload_service = Arc::clone(&self.upload_service);
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
                        if let Err(e) = stream.set_nodelay(true) {
                            tracing::warn!("[Listener] Failed to set TCP_NODELAY: {}", e);
                        }
                        tracing::info!("[Listener] Accepted connection from {} on {} port", addr, port_label);

                        let file_conn_tx = file_conn_tx.clone();
                        let peer_streams = Arc::clone(&peer_streams);
                        let search_results = Arc::clone(&search_results);
                        let active_search_tokens = Arc::clone(&active_search_tokens);
                        let pending_downloads = Arc::clone(&pending_downloads);
                        let downloads = Arc::clone(&downloads);
                        let result_notify = result_notify.clone();
                        let our_username = username.clone();
                        let peer_addr = addr.to_string();
                        let port_label_owned = port_label.to_string();
                        let sharing_service = Arc::clone(&sharing_service);
                        let upload_service = Arc::clone(&upload_service);

                        tokio::spawn(async move {
                            let _active = ACTIVE_HANDLERS.fetch_add(1, Ordering::Relaxed) + 1;

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
                                            "[Listener] Resuming queued download: '{}' (token={})",
                                            pending_dl.filename, token
                                        );

                                        {
                                            let mut dl = downloads.lock().await;
                                            if let Some(p) = dl.get_mut(&pending_dl.filename) {
                                                p.status = DownloadStatus::InProgress;
                                            }
                                        }

                                        // Spawn the file transfer as a background task
                                        let downloads_clone = Arc::clone(&downloads);
                                        let filename = pending_dl.filename.clone();
                                        let dl_username = pending_dl.username.clone();
                                        let filesize = pending_dl.filesize;
                                        let output_path = pending_dl.output_path.clone();
                                        let progress_callback = pending_dl.progress_callback;
                                        let completion_tx = pending_dl.completion_tx;
                                        let item_id = pending_dl.item_id;
                                        let queue_id = pending_dl.queue_id;
                                        let list_id = pending_dl.list_id;
                                        let transfer_token = pending_dl.transfer_token;

                                        tokio::spawn(async move {
                                            let result = super::peer::receive_file_data(
                                                conn,
                                                filesize,
                                                output_path.clone(),
                                                progress_callback,
                                                &dl_username,
                                            ).await;

                                            match &result {
                                                Ok(bytes) => {
                                                    tracing::info!("[Transfer] Completed: {} ({} bytes)", filename, bytes);
                                                    let mut dl = downloads_clone.lock().await;
                                                    if let Some(p) = dl.get_mut(&filename) {
                                                        p.downloaded_bytes = *bytes;
                                                        p.status = DownloadStatus::Completed;
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::error!("[Transfer] Failed: {} - {}", filename, e);
                                                    let mut dl = downloads_clone.lock().await;
                                                    if let Some(p) = dl.get_mut(&filename) {
                                                        p.status = DownloadStatus::Failed(e.to_string());
                                                    }
                                                }
                                            }

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
                                                let _ = tx.send(completion).await;
                                            }
                                        });
                                    } else {
                                        // Not a pending download, send to file conn channel
                                        // (will be routed by file conn dispatcher)
                                        let _ = file_conn_tx.send(conn).await;
                                    }
                                }
                                Ok(IncomingConnection::Peer(mut stream, peer_username, _token)) => {
                                    tracing::info!(
                                        "[Listener] P connection from '{}' ({}, {}), handshake={:?}",
                                        peer_username, addr, port_label_owned, handshake_duration
                                    );

                                    // Dispatch by reading peer message code
                                    Self::handle_peer_message_dispatch(
                                        &mut stream,
                                        &peer_username,
                                        &peer_addr,
                                        task_start,
                                        Arc::clone(&peer_streams),
                                        Arc::clone(&search_results),
                                        Arc::clone(&active_search_tokens),
                                        result_notify.clone(),
                                        Arc::clone(&sharing_service),
                                        Arc::clone(&upload_service),
                                        &our_username,
                                    ).await;
                                }
                                Err(e) => {
                                    tracing::info!(
                                        "[Listener] Connection from {} failed handshake: {}, handshake={:?}",
                                        addr, e, handshake_duration
                                    );
                                }
                            }

                            let _remaining = ACTIVE_HANDLERS.fetch_sub(1, Ordering::Relaxed) - 1;
                        });
                    }
                    Err(e) => {
                        tracing::error!("[Listener] Accept error on {} port: {}", port_label, e);
                    }
                }
            }
        });
    }

    /// Read a peer message from a stream, handling the case where a PeerInit message
    /// (u8 code framing) arrives before the actual peer message (u32 code framing).
    ///
    /// PeerInit messages use: [length:u32][type:u8][body...]
    /// Peer messages use:     [length:u32][code:u32][body...]
    ///
    /// After PierceFirewall, some clients send a PeerInit before peer messages.
    /// We detect this by peeking at the 5th byte: if it's a known PeerInit type
    /// (0=PierceFirewall, 1=PeerInit) and the length is reasonable for an init
    /// message, we consume it and read the next message.
    async fn read_peer_message_with_init_handling(
        stream: &mut TcpStream,
        peer_username: &str,
    ) -> std::io::Result<(u32, Vec<u8>)> {
        use tokio::io::AsyncReadExt;

        // Peek at the first 5 bytes to determine message type
        let mut peek_buf = [0u8; 5];
        tracing::info!("[PeerDispatch] Reading first 5 bytes from '{}'...", peer_username);
        stream.read_exact(&mut peek_buf).await?;

        let msg_len = u32::from_le_bytes([peek_buf[0], peek_buf[1], peek_buf[2], peek_buf[3]]);
        let first_code_byte = peek_buf[4];

        tracing::info!(
            "[PeerDispatch] First 5 bytes from '{}': len={}, first_code_byte={} (raw: {:02x} {:02x} {:02x} {:02x} {:02x})",
            peer_username, msg_len, first_code_byte,
            peek_buf[0], peek_buf[1], peek_buf[2], peek_buf[3], peek_buf[4]
        );

        // PeerInit messages have u8 type codes 0 (PierceFirewall) or 1 (PeerInit)
        // and lengths typically < 200. Peer messages have u32 codes >= 4 and the
        // 5th byte (first byte of the u32 code) would be the code value.
        // For code 4 (GetSharedFileList): length=4, 5th byte=4
        // For PeerInit: length=14+username_len, 5th byte=1
        // For PierceFirewall: length=5, 5th byte=0

        if (first_code_byte == 0 || first_code_byte == 1) && msg_len > 4 && msg_len < 200 {
            // This looks like a PeerInit message (u8 type code)
            // Consume the rest of it and try reading the next message
            let remaining = (msg_len + 4 - 5) as usize; // total - already read
            if remaining > 0 {
                let mut discard = vec![0u8; remaining];
                stream.read_exact(&mut discard).await?;
            }
            tracing::info!(
                "[PeerDispatch] Consumed PeerInit (type={}, len={}) from '{}', reading actual peer message...",
                first_code_byte, msg_len, peer_username
            );

            // Now read the actual peer message
            return read_message(stream).await;
        }

        // This is a regular peer message. We already read 5 bytes (4 for length + 1st byte of code).
        // Read the remaining 3 bytes of the u32 code.
        let mut code_rest = [0u8; 3];
        stream.read_exact(&mut code_rest).await?;
        let code = u32::from_le_bytes([first_code_byte, code_rest[0], code_rest[1], code_rest[2]]);

        tracing::info!(
            "[PeerDispatch] Peer message from '{}': code={}, data_len={}",
            peer_username, code, if msg_len >= 4 { msg_len - 4 } else { 0 }
        );

        // Read the data
        let data_len = if msg_len >= 4 { (msg_len - 4) as usize } else { 0 };
        let mut data = vec![0u8; data_len];
        if data_len > 0 {
            stream.read_exact(&mut data).await?;
        }

        Ok((code, data))
    }

    /// Handle an incoming P connection by dispatching based on peer message code.
    ///
    /// This replaces the old approach of assuming all P connections carry search results.
    /// Now we read the actual message code and dispatch to the appropriate handler:
    /// - Code 4: GetSharedFileList -> respond with our file list
    /// - Code 8: FileSearchRequest -> query our word index, respond with matches
    /// - Code 9: FileSearchResult -> existing search result handling
    /// - Code 43: QueueUpload -> peer wants to download from us
    /// - Code 51: PlaceInQueueRequest -> respond with queue position
    /// - Code 40: TransferRequest (dir=0) -> legacy download request, treat like QueueUpload
    async fn handle_peer_message_dispatch(
        stream: &mut TcpStream,
        peer_username: &str,
        peer_addr: &str,
        task_start: std::time::Instant,
        peer_streams: Arc<Mutex<LruCache<String, TcpStream>>>,
        search_results: Arc<Mutex<HashMap<u32, Vec<SearchResult>>>>,
        active_search_tokens: Arc<Mutex<std::collections::HashSet<u32>>>,
        result_notify: tokio::sync::broadcast::Sender<u32>,
        sharing_service: Arc<RwLock<Option<Arc<SharingService>>>>,
        upload_service: Arc<RwLock<Option<Arc<UploadService>>>>,
        our_username: &str,
    ) {
        // Read first peer message with timeout.
        // Some clients may send a PeerInit message (u8 code framing) before the actual
        // peer message (u32 code framing). We need to detect and skip PeerInit if present.
        let msg = match tokio::time::timeout(Duration::from_secs(10), Self::read_peer_message_with_init_handling(stream, peer_username)).await {
            Ok(Ok((code, data))) => (code, data),
            Ok(Err(e)) => {
                tracing::info!("[PeerDispatch] Failed to read peer message from '{}': {}", peer_username, e);
                return;
            }
            Err(_) => {
                tracing::info!("[PeerDispatch] Timeout reading peer message from '{}' (10s)", peer_username);
                return;
            }
        };

        let (code, data) = msg;
        tracing::info!("[PeerDispatch] Received peer code {} from '{}' ({} bytes)", code, peer_username, data.len());

        match code {
            9 => {
                // FileSearchResult - existing search result handling
                use flate2::read::ZlibDecoder;
                use std::io::Read as _;

                let decompressed = match ZlibDecoder::new(&data[..]).bytes().collect::<Result<Vec<_>, _>>() {
                    Ok(d) => d,
                    Err(_) => data,
                };

                match super::peer::parse_file_search_response(&decompressed) {
                    Ok(response) => {
                        let search_token = response.token;
                        tracing::debug!(
                            "[Listener] Got {} files from '{}' ({}, {:.0} KB/s), token={}, total={:?}",
                            response.files.len(),
                            peer_username,
                            if response.slot_free { "FREE" } else { "BUSY" },
                            response.avg_speed as f64 / 1024.0,
                            search_token,
                            task_start.elapsed()
                        );

                        // Store peer stream for potential download
                        // We need to take ownership, but we only have &mut - this is a limitation
                        // For now, skip storing since we can't move out of &mut

                        let search_result = SearchResult {
                            username: peer_username.to_string(),
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
                            peer_port: 0,
                        };

                        let tokens = active_search_tokens.lock().await;
                        if tokens.contains(&search_token) {
                            let mut results = search_results.lock().await;
                            results.entry(search_token)
                                .or_insert_with(Vec::new)
                                .push(search_result);
                            let _ = result_notify.send(search_token);
                        } else {
                            tracing::debug!(
                                "[Listener] Dropping result from '{}' for inactive token {}",
                                peer_username, search_token
                            );
                        }
                    }
                    Err(e) => {
                        tracing::debug!("[Listener] Failed to parse search result from '{}': {}", peer_username, e);
                    }
                }
            }

            4 => {
                // GetSharedFileList - peer wants our complete file list
                tracing::info!("[Listener] GetSharedFileList from '{}'", peer_username);
                let sharing = sharing_service.read().await;
                if let Some(ref service) = *sharing {
                    let compressed = service.get_compressed_file_list().await;
                    if let Err(e) = send_message(stream, 5, &compressed).await {
                        tracing::debug!("[Listener] Failed to send SharedFileList to '{}': {}", peer_username, e);
                    } else {
                        tracing::info!("[Listener] Sent SharedFileList to '{}'", peer_username);
                    }
                } else {
                    // Send empty file list
                    let mut buf = BytesMut::new();
                    buf.put_u32_le(0); // 0 folders
                    if let Err(e) = send_message(stream, 5, &buf).await {
                        tracing::debug!("[Listener] Failed to send empty SharedFileList to '{}': {}", peer_username, e);
                    }
                }
            }

            8 => {
                // FileSearchRequest - peer is searching our files
                let mut buf = BytesMut::from(&data[..]);
                if buf.remaining() >= 8 {
                    let token = buf.get_u32_le();
                    if let Ok(query) = decode_string(&mut buf) {
                        tracing::info!("[Listener] FileSearchRequest from '{}': '{}' (token={})", peer_username, query, token);

                        let sharing = sharing_service.read().await;
                        if let Some(ref service) = *sharing {
                            let matches = service.search(&query, 50).await;
                            if !matches.is_empty() {
                                // Check upload slots
                                let upload = upload_service.read().await;
                                let free_slots = upload.as_ref().map_or(true, |u| {
                                    // We'll check properly later; for now assume free
                                    true
                                });

                                let response = crate::services::sharing::encode_search_response(
                                    our_username,
                                    token,
                                    &matches,
                                    free_slots,
                                    0, // avg speed - will be updated with real stats later
                                    0, // queue length
                                );
                                if let Err(e) = send_message(stream, 9, &response).await {
                                    tracing::debug!("[Listener] Failed to send search response to '{}': {}", peer_username, e);
                                } else {
                                    tracing::info!("[Listener] Sent {} results to '{}' for '{}'", matches.len(), peer_username, query);
                                }
                            }
                        }
                    }
                }
            }

            43 => {
                // QueueUpload - peer wants to download a file from us
                let mut buf = BytesMut::from(&data[..]);
                if let Ok(filename) = decode_string(&mut buf) {
                    tracing::info!("[Listener] QueueUpload from '{}': '{}'", peer_username, filename);

                    let upload = upload_service.read().await;
                    if let Some(ref service) = *upload {
                        match service.handle_queue_upload(peer_username, &filename).await {
                            Ok(position) => {
                                tracing::info!("[Listener] Queued upload for '{}': position={}", peer_username, position);
                                // Send PlaceInQueueReply (code 44)
                                let mut reply = BytesMut::new();
                                encode_string(&mut reply, &filename);
                                reply.put_u32_le(position);
                                if let Err(e) = send_message(stream, 44, &reply).await {
                                    tracing::debug!("[Listener] Failed to send PlaceInQueueReply to '{}': {}", peer_username, e);
                                }
                            }
                            Err(reason) => {
                                tracing::info!("[Listener] Rejecting QueueUpload from '{}': {}", peer_username, reason);
                                // Send QueueFailed (code 50)
                                let mut reply = BytesMut::new();
                                encode_string(&mut reply, &filename);
                                encode_string(&mut reply, &reason);
                                if let Err(e) = send_message(stream, 50, &reply).await {
                                    tracing::debug!("[Listener] Failed to send QueueFailed to '{}': {}", peer_username, e);
                                }
                            }
                        }
                    } else {
                        // Upload service not set, reject
                        let mut reply = BytesMut::new();
                        encode_string(&mut reply, &filename);
                        encode_string(&mut reply, "Sharing disabled");
                        let _ = send_message(stream, 50, &reply).await;
                    }
                }
            }

            51 => {
                // PlaceInQueueRequest - peer asking their queue position
                let mut buf = BytesMut::from(&data[..]);
                if let Ok(filename) = decode_string(&mut buf) {
                    tracing::info!("[Listener] PlaceInQueueRequest from '{}': '{}'", peer_username, filename);

                    let upload = upload_service.read().await;
                    let position = if let Some(ref service) = *upload {
                        service.get_queue_position(peer_username, &filename).await.unwrap_or(0)
                    } else {
                        0
                    };

                    let mut reply = BytesMut::new();
                    encode_string(&mut reply, &filename);
                    reply.put_u32_le(position);
                    let _ = send_message(stream, 44, &reply).await;
                }
            }

            40 => {
                // TransferRequest - could be legacy download request (dir=0) or upload notification (dir=1)
                let mut buf = BytesMut::from(&data[..]);
                if buf.remaining() >= 8 {
                    let direction = buf.get_u32_le();
                    let token = buf.get_u32_le();
                    if let Ok(filename) = decode_string(&mut buf) {
                        if direction == 0 {
                            // Legacy download request from peer - treat like QueueUpload
                            tracing::info!("[Listener] TransferRequest(dir=0) from '{}': '{}' (treating as QueueUpload)", peer_username, filename);
                            let upload = upload_service.read().await;
                            if let Some(ref service) = *upload {
                                match service.handle_queue_upload(peer_username, &filename).await {
                                    Ok(_position) => {
                                        // Send TransferReply (code 41) - queued
                                        let mut reply = BytesMut::new();
                                        reply.put_u32_le(token);
                                        reply.put_u8(0); // not allowed (queued)
                                        encode_string(&mut reply, "Queued");
                                        let _ = send_message(stream, 41, &reply).await;
                                    }
                                    Err(reason) => {
                                        let mut reply = BytesMut::new();
                                        reply.put_u32_le(token);
                                        reply.put_u8(0);
                                        encode_string(&mut reply, &reason);
                                        let _ = send_message(stream, 41, &reply).await;
                                    }
                                }
                            }
                        } else {
                            tracing::debug!("[Listener] TransferRequest(dir={}) from '{}' - ignoring in listener", direction, peer_username);
                        }
                    }
                }
            }

            15 => {
                // UserInfoRequest - peer wants our user info
                tracing::info!("[Listener] UserInfoRequest from '{}'", peer_username);

                let mut reply = BytesMut::new();

                // Description
                let sharing = sharing_service.read().await;
                let upload = upload_service.read().await;

                let (file_count, folder_count) = if let Some(ref service) = *sharing {
                    let counts = service.counts().await;
                    (counts.1, counts.0)
                } else {
                    (0, 0)
                };

                let description = format!(
                    "Rinse - Soulseek client\nSharing {} files in {} folders",
                    file_count, folder_count
                );
                encode_string(&mut reply, &description);

                // No picture
                reply.put_u8(0); // has_pic = false

                // Total uploads
                reply.put_u32_le(0);

                // Queue size
                let queue_size = if let Some(ref service) = *upload {
                    service.stats().await.queued_uploads as u32
                } else {
                    0
                };
                reply.put_u32_le(queue_size);

                // Slots available
                let slots_free = if let Some(ref service) = *upload {
                    service.has_free_slots().await
                } else {
                    true
                };
                reply.put_u8(if slots_free { 1 } else { 0 });

                // Upload allowed (1 = yes, everyone)
                reply.put_u32_le(1);

                if let Err(e) = send_message(stream, 16, &reply).await {
                    tracing::debug!("[Listener] Failed to send UserInfoResponse to '{}': {}", peer_username, e);
                } else {
                    tracing::info!("[Listener] Sent UserInfoResponse to '{}'", peer_username);
                }
            }

            _ => {
                tracing::info!("[PeerDispatch] Unhandled peer code {} from '{}' ({} bytes)", code, peer_username, data.len());
            }
        }
    }

    /// Start the ConnectToPeer "P" handler task.
    ///
    /// This task receives ConnectToPeer "P" messages from the router and spawns
    /// connection tasks that connect to peers and dispatch based on peer message code.
    /// Handles both search results (code 9) and sharing-related messages (codes 4, 8, 43, etc.).
    fn start_connect_to_peer_handler(self: &Arc<Self>, mut rx: mpsc::Receiver<ConnectToPeerMsg>) {
        let peer_streams = Arc::clone(&self.peer_streams);
        let search_results = Arc::clone(&self.search_results);
        let active_search_tokens = Arc::clone(&self.active_search_tokens);
        let result_notify = self.result_notify.clone();
        let router_username = self.username.clone();
        let sharing_service = Arc::clone(&self.sharing_service);
        let upload_service = Arc::clone(&self.upload_service);
        let router = self.router.clone();
        let our_external_ip = self.external_ip.clone();

        tokio::spawn(async move {
            tracing::info!("[ConnectToPeerHandler] Started");

            // Limit concurrent connection attempts
            let semaphore = Arc::new(tokio::sync::Semaphore::new(50));

            while let Some(msg) = rx.recv().await {
                // With sharing enabled, we should accept P connections even without active searches
                // (peers may want to request our file list or search our files)
                let has_active_searches = !active_search_tokens.lock().await.is_empty();
                let has_sharing = sharing_service.read().await.is_some();

                if !has_active_searches && !has_sharing {
                    tracing::debug!("[ConnectToPeerHandler] No active searches and sharing disabled, ignoring ConnectToPeer P from '{}'", msg.username);
                    continue;
                }

                let permit = match semaphore.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        tracing::debug!("[ConnectToPeerHandler] Too many concurrent connections, skipping");
                        continue;
                    }
                };

                let peer_streams = Arc::clone(&peer_streams);
                let search_results = Arc::clone(&search_results);
                let active_search_tokens = Arc::clone(&active_search_tokens);
                let result_notify = result_notify.clone();
                let our_username = router_username.clone();
                let sharing_service = Arc::clone(&sharing_service);
                let upload_service = Arc::clone(&upload_service);
                let router = router.clone();
                let our_external_ip = our_external_ip.clone();

                tokio::spawn(async move {
                    let _permit = permit; // held until task completes

                    // Loopback detection: if the peer's IP matches our external IP,
                    // they're on the same network/machine. Rewrite to 127.0.0.1 to
                    // avoid NAT hairpinning issues.
                    let connect_ip = if let Some(ref ext_ip) = our_external_ip {
                        if msg.ip == *ext_ip {
                            tracing::info!(
                                "[ConnectToPeerHandler] Peer '{}' IP {} matches our external IP, using 127.0.0.1 (loopback)",
                                msg.username, msg.ip
                            );
                            "127.0.0.1".to_string()
                        } else {
                            msg.ip.clone()
                        }
                    } else {
                        msg.ip.clone()
                    };

                    // Connect to the peer and send PierceFirewall
                    let peer_addr = format!("{}:{}", connect_ip, msg.port);
                    tracing::info!("[ConnectToPeerHandler] Connecting to '{}' at {} (token={})", msg.username, peer_addr, msg.token);
                    let mut peer_stream = match connect_to_peer_for_dispatch(
                        &connect_ip, msg.port, &msg.username, msg.token,
                    ).await {
                        Ok(stream) => {
                            tracing::info!("[ConnectToPeerHandler] Connected to '{}', PierceFirewall sent", msg.username);
                            stream
                        }
                        Err(e) => {
                            tracing::info!("[ConnectToPeerHandler] Failed to connect to '{}': {}", msg.username, e);
                            // Tell the server we can't connect, so it tells the peer to connect to us instead
                            if let Err(e2) = router.send_cant_connect_to_peer(msg.token, &msg.username).await {
                                tracing::warn!("[ConnectToPeerHandler] Failed to send CantConnectToPeer: {}", e2);
                            } else {
                                tracing::info!("[ConnectToPeerHandler] Sent CantConnectToPeer for '{}' (token={}) - peer should connect to us", msg.username, msg.token);
                            }
                            return;
                        }
                    };

                    let task_start = std::time::Instant::now();

                    // Use the general peer message dispatch which handles ALL peer message codes
                    // (search results, GetSharedFileList, QueueUpload, FileSearchRequest, etc.)
                    tracing::info!("[ConnectToPeerHandler] Waiting for peer message from '{}'...", msg.username);
                    Self::handle_peer_message_dispatch(
                        &mut peer_stream,
                        &msg.username,
                        &peer_addr,
                        task_start,
                        Arc::clone(&peer_streams),
                        Arc::clone(&search_results),
                        Arc::clone(&active_search_tokens),
                        result_notify.clone(),
                        sharing_service,
                        upload_service,
                        &our_username,
                    ).await;
                    tracing::info!("[ConnectToPeerHandler] Dispatch complete for '{}'", msg.username);
                });
            }

            tracing::warn!("[ConnectToPeerHandler] Channel closed");
        });
    }

    /// Start the file connection dispatcher task.
    ///
    /// Routes incoming F connections to per-download waiters by transfer token.
    fn start_file_conn_dispatcher(self: &Arc<Self>, mut file_conn_rx: mpsc::Receiver<FileConnection>) {
        let file_conn_waiters = Arc::clone(&self.file_conn_waiters);

        tokio::spawn(async move {
            tracing::debug!("[FileConnDispatcher] Started");

            while let Some(conn) = file_conn_rx.recv().await {
                let token = conn.transfer_token;
                let mut waiters = file_conn_waiters.lock().await;

                if let Some(tx) = waiters.remove(&token) {
                    tracing::debug!("[FileConnDispatcher] Routed F connection to waiter (token={})", token);
                    let _ = tx.send(conn);
                } else {
                    tracing::debug!(
                        "[FileConnDispatcher] No waiter for F connection token={}, dropping",
                        token
                    );
                }
            }

            tracing::warn!("[FileConnDispatcher] Channel closed");
        });
    }

    // ========================================================================
    // Search
    // ========================================================================

    /// Search for files on the network.
    ///
    /// Takes `&self` (not `&mut self`), allowing concurrent searches.
    pub async fn search(&self, query: &str, timeout_secs: u64) -> Result<Vec<SearchResult>> {
        let token: u32 = rand::random();

        tracing::info!("[Search] Started: '{}' (token={}, timeout={}s)", query, token, timeout_secs);

        // Register this search token as active
        self.active_search_tokens.lock().await.insert(token);
        // Initialize result bucket
        self.search_results.lock().await.insert(token, Vec::new());

        // Subscribe to result notifications
        let mut notify_rx = self.result_notify.subscribe();

        // Send search to server via router
        let mut search_data = BytesMut::new();
        search_data.put_u32_le(token);
        encode_string(&mut search_data, query);
        self.router.send_message(26, &search_data).await?;

        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        let min_wait = Duration::from_secs(3);
        let no_results_timeout = Duration::from_secs(7);
        let mut last_result_time = std::time::Instant::now();
        let mut first_result_time: Option<std::time::Instant> = None;

        // Wait for results with early-exit conditions
        while start.elapsed() < timeout {
            // Check current result count
            let results_with_files = {
                let results = self.search_results.lock().await;
                results.get(&token)
                    .map(|r| r.iter().filter(|r| !r.files.is_empty()).count())
                    .unwrap_or(0)
            };

            // Track first result
            if results_with_files > 0 && first_result_time.is_none() {
                first_result_time = Some(std::time::Instant::now());
                tracing::info!("[Search] First result at {:.2}s", start.elapsed().as_secs_f32());
            }

            // Early abort: no results at all after timeout
            if start.elapsed() > no_results_timeout && results_with_files == 0 {
                tracing::warn!("[Search] Early abort: no results after {:.1}s", start.elapsed().as_secs_f32());
                break;
            }

            // Early exit conditions (after minimum wait)
            if start.elapsed() > min_wait {
                if results_with_files >= 10 {
                    tracing::info!("[Search] Early exit: {} results with files", results_with_files);
                    break;
                }
                if results_with_files >= 5 && last_result_time.elapsed() > Duration::from_secs(2) {
                    tracing::info!("[Search] Early exit: {} results, no new results for 2s", results_with_files);
                    break;
                }
            }

            // Wait for a notification or timeout
            match tokio::time::timeout(Duration::from_millis(200), notify_rx.recv()).await {
                Ok(Ok(notified_token)) if notified_token == token => {
                    last_result_time = std::time::Instant::now();
                }
                _ => {
                    // Timeout or notification for different token, continue
                }
            }
        }

        // Collect results for this search token only (no cross-contamination)
        let mut results = {
            let mut all_results = self.search_results.lock().await;
            all_results.remove(&token).unwrap_or_default()
        };

        // Grace period: collect any last results
        let grace_end = std::time::Instant::now() + Duration::from_millis(500);
        while std::time::Instant::now() < grace_end {
            match tokio::time::timeout(Duration::from_millis(100), notify_rx.recv()).await {
                Ok(Ok(notified_token)) if notified_token == token => {
                    let mut all_results = self.search_results.lock().await;
                    if let Some(mut new_results) = all_results.remove(&token) {
                        results.append(&mut new_results);
                    }
                }
                _ => break,
            }
        }

        // Unregister this search token
        self.active_search_tokens.lock().await.remove(&token);
        // Clean up any remaining results for this token
        self.search_results.lock().await.remove(&token);

        // Final summary
        let total_time = start.elapsed();
        let cached_peers = self.peer_streams.lock().await.len();
        let total_files: usize = results.iter().map(|r| r.files.len()).sum();

        let first_result_str = first_result_time
            .map(|t| format!(", first result: {:.2}s", t.duration_since(start).as_secs_f32()))
            .unwrap_or_default();
        tracing::info!(
            "[Search] Complete: '{}' in {:.2}s — {} users, {} files, {} cached peers{}",
            query, total_time.as_secs_f32(), results.len(), total_files, cached_peers, first_result_str
        );

        if results.is_empty() {
            bail!("No results found for search '{}' - no peers responded", query);
        }

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

    // ========================================================================
    // Download
    // ========================================================================

    /// Helper to update download status
    async fn update_download_status(&self, filename: &str, status: DownloadStatus) {
        let mut downloads = self.downloads.lock().await;
        if let Some(p) = downloads.get_mut(filename) {
            p.status = status;
        }
    }

    /// Connect to peer for download negotiation.
    ///
    /// Tries stored P connection first, then direct connection using provided
    /// or server-resolved address.
    async fn connect_to_peer_for_download(
        &self,
        username: &str,
        peer_ip: &str,
        peer_port: u32,
    ) -> Result<TcpStream> {
        // Helper closure for loopback detection
        let resolve_ip = |ip: &str| -> String {
            if let Some(ref ext_ip) = self.external_ip {
                if ip == ext_ip.as_str() {
                    tracing::info!("[Download] Peer IP {} matches our external IP, using 127.0.0.1", ip);
                    return "127.0.0.1".to_string();
                }
            }
            ip.to_string()
        };

        if peer_port > 0 && !peer_ip.is_empty() && peer_ip != "0.0.0.0" {
            let connect_ip = resolve_ip(peer_ip);
            tracing::debug!("[Download] Connecting directly to {}:{}", connect_ip, peer_port);
            let addr = format!("{}:{}", connect_ip, peer_port);
            let mut stream = tokio::time::timeout(
                Duration::from_secs(10),
                TcpStream::connect(&addr)
            ).await
                .context("Connection timeout")?
                .context("Connection failed")?;

            stream.set_nodelay(true)?;
            super::peer::send_peer_init(&mut stream, &self.username, "P", 0).await?;
            return Ok(stream);
        }

        // No direct address - request from server via router
        tracing::debug!("[Download] Requesting peer address from server for '{}'", username);
        let (ip, port) = self.get_peer_address(username).await?;

        if port == 0 || ip.is_empty() {
            bail!("Peer address not available (port={})", port);
        }

        let connect_ip = resolve_ip(&ip);
        let addr = format!("{}:{}", connect_ip, port);
        let mut stream = tokio::time::timeout(
            Duration::from_secs(10),
            TcpStream::connect(&addr)
        ).await
            .context("Connection timeout")?
            .context("Connection failed")?;

        stream.set_nodelay(true)?;
        super::peer::send_peer_init(&mut stream, &self.username, "P", 0).await?;
        Ok(stream)
    }

    /// Request peer's address from server via router
    async fn get_peer_address(&self, username: &str) -> Result<(String, u32)> {
        let rx = self.router.get_peer_address(username).await?;

        match tokio::time::timeout(Duration::from_secs(10), rx).await {
            Ok(Ok(response)) => {
                tracing::debug!("[Download] GetPeerAddress response: {}:{}", response.ip, response.port);
                Ok((response.ip, response.port))
            }
            Ok(Err(_)) => bail!("Peer address channel closed"),
            Err(_) => bail!("Timeout getting peer address"),
        }
    }

    /// Wait for an F connection by registering a per-download waiter.
    ///
    /// The file conn dispatcher routes incoming F connections to waiters by token.
    async fn wait_for_f_connection_routed(
        &self,
        transfer_token: u32,
        timeout_secs: u64,
    ) -> Result<FileConnection> {
        let (tx, rx) = oneshot::channel();

        // Register waiter
        self.file_conn_waiters.lock().await.insert(transfer_token, tx);

        match tokio::time::timeout(Duration::from_secs(timeout_secs), rx).await {
            Ok(Ok(conn)) => {
                Ok(conn)
            }
            Ok(Err(_)) => {
                // Sender dropped
                self.file_conn_waiters.lock().await.remove(&transfer_token);
                bail!("F connection waiter cancelled")
            }
            Err(_) => {
                // Timeout
                self.file_conn_waiters.lock().await.remove(&transfer_token);
                bail!("Timeout waiting for F connection ({}s)", timeout_secs)
            }
        }
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

        stream.set_nodelay(true)?;

        tracing::debug!("[Download] Connected, sending PeerInit type F");

        let mut data = BytesMut::new();
        encode_string(&mut data, &self.username);
        encode_string(&mut data, "F");
        data.put_u32_le(0);

        let msg_len = 1 + data.len() as u32;
        stream.write_u32_le(msg_len).await?;
        stream.write_u8(1).await?;
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

    /// Initiate a download for the queue system.
    ///
    /// Takes `&self` (not `&mut self`), allowing concurrent downloads.
    ///
    /// This method:
    /// 1. Gets or establishes P connection with peer
    /// 2. Sends QueueUpload request
    /// 3. Waits for TransferRequest/TransferReply exchange
    /// 4. Waits for F connection (via per-download waiter or proactive connection)
    /// 5. Spawns file receive as background task
    pub async fn initiate_download(
        &self,
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
            "[Download] Starting: '{}' from '{}' ({:.2} MB) [item={}, queue={}]",
            filename, username, filesize as f64 / 1_048_576.0, item_id, queue_id
        );

        // Update internal status
        {
            let mut downloads = self.downloads.lock().await;
            downloads.insert(filename.to_string(), DownloadProgress {
                filename: filename.to_string(),
                total_bytes: filesize,
                downloaded_bytes: 0,
                status: DownloadStatus::Pending,
            });
        }

        // Step 1: Get or establish P connection
        let stored_stream = self.peer_streams.lock().await.pop(username);

        let mut peer_stream = match stored_stream {
            Some(stream) => {
                tracing::info!("[Download] P connection: stored from search");
                stream
            }
            None => {
                tracing::info!("[Download] P connection: establishing direct to '{}'", username);

                let (final_ip, final_port) = if !peer_ip.is_empty() && peer_port > 0 {
                    (peer_ip.to_string(), peer_port)
                } else {
                    match self.get_peer_address(username).await {
                        Ok((ip, port)) if port > 0 => (ip, port),
                        _ => {
                            let reason = "No stored P connection and cannot get peer address".to_string();
                            self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                            return DownloadInitResult::Failed { reason };
                        }
                    }
                };

                match self.connect_to_peer_for_download(username, &final_ip, final_port).await {
                    Ok(stream) => stream,
                    Err(e) => {
                        let reason = format!("Failed to connect to peer: {}", e);
                        self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                        return DownloadInitResult::Failed { reason };
                    }
                }
            }
        };

        // Step 2: Send QueueUpload request
        if let Err(e) = queue_upload(&mut peer_stream, filename).await {
            let reason = format!("Failed to send QueueUpload: {}", e);
            self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
            return DownloadInitResult::Failed { reason };
        }

        // Step 3: Wait for TransferRequest and send TransferReply
        let negotiation = match wait_for_transfer_request(&mut peer_stream, filename, 60).await {
            Ok(result) => result,
            Err(e) => {
                let reason = format!("Negotiation failed: {}", e);
                self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                return DownloadInitResult::Failed { reason };
            }
        };

        // Release P connection
        drop(peer_stream);

        // Handle negotiation result
        match negotiation {
            NegotiationResult::Allowed { filesize: actual_size, transfer_token } => {
                tracing::info!(
                    "[Download] Transfer allowed: {:.2} MB, token={}",
                    actual_size as f64 / 1_048_576.0, transfer_token
                );

                self.update_download_status(filename, DownloadStatus::InProgress).await;
                {
                    let mut downloads = self.downloads.lock().await;
                    if let Some(p) = downloads.get_mut(filename) {
                        p.total_bytes = actual_size;
                    }
                }

                // Step 5: Wait for F connection via per-download waiter

                let file_conn = self.wait_for_f_connection_routed(transfer_token, 30).await;

                // If routed wait failed, try proactive F connection as fallback
                let file_conn = match file_conn {
                    Ok(conn) => Ok(conn),
                    Err(e) => {
                        tracing::info!("[Download] Routed F connection failed: {}, trying proactive...", e);

                        // Try to get peer address for proactive connection
                        let (mut final_ip, final_port) = if !peer_ip.is_empty() && peer_port > 0 {
                            (peer_ip.to_string(), peer_port)
                        } else {
                            match self.get_peer_address(username).await {
                                Ok((ip, port)) if port > 0 => (ip, port),
                                _ => (String::new(), 0),
                            }
                        };

                        // Loopback detection for proactive F connections
                        if let Some(ref ext_ip) = self.external_ip {
                            if final_ip == *ext_ip {
                                tracing::info!("[Download] Peer IP {} matches our external IP, using 127.0.0.1", final_ip);
                                final_ip = "127.0.0.1".to_string();
                            }
                        }

                        if !final_ip.is_empty() && final_port > 0 {
                            self.proactive_f_connection(&final_ip, final_port, transfer_token).await
                        } else {
                            Err(e)
                        }
                    }
                };

                match file_conn {
                    Ok(conn) => {
                        tracing::info!("[Download] F connection established, spawning transfer");

                        // Spawn file receive as background task
                        let output_path = output_path.to_path_buf();
                        let downloads_clone = Arc::clone(&self.downloads);
                        let filename_clone = filename.to_string();
                        let username_clone = username.to_string();

                        tokio::spawn(async move {
                            let result = super::peer::receive_file_data(
                                conn,
                                actual_size,
                                output_path,
                                progress_callback,
                                &username_clone,
                            ).await;

                            match &result {
                                Ok(bytes) => {
                                    let mut dl = downloads_clone.lock().await;
                                    if let Some(p) = dl.get_mut(&filename_clone) {
                                        p.status = DownloadStatus::Completed;
                                        p.downloaded_bytes = *bytes;
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("[Transfer] Failed: '{}': {}", filename_clone, e);
                                    let mut dl = downloads_clone.lock().await;
                                    if let Some(p) = dl.get_mut(&filename_clone) {
                                        p.status = DownloadStatus::Failed(e.to_string());
                                    }
                                }
                            }

                            let _ = completion_tx.send(TransferComplete {
                                item_id,
                                queue_id,
                                list_id,
                                transfer_token,
                                result: result.map_err(|e| e.to_string()),
                            }).await;
                        });

                        DownloadInitResult::TransferSpawned { item_id, transfer_token }
                    }
                    Err(e) => {
                        let reason = format!("Failed to establish F connection: {}", e);
                        tracing::error!("[Download] Failed: {}", reason);
                        self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;

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
                tracing::info!("[Download] QUEUED by peer: {} (position: {:?})", reason, position);
                self.update_download_status(filename, DownloadStatus::Queued).await;

                // Register as pending download
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

                DownloadInitResult::Queued { transfer_token, position, reason }
            }

            NegotiationResult::Denied { reason } => {
                tracing::info!("[Download] DENIED by peer: {}", reason);
                self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;

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

    /// Download a file from a peer (legacy method, kept for compatibility)
    pub async fn download_file(
        &self,
        username: &str,
        filename: &str,
        filesize: u64,
        peer_ip: &str,
        peer_port: u32,
        output_path: &std::path::Path,
        progress_callback: Option<super::peer::ProgressCallback>,
    ) -> DownloadResult {
        tracing::info!("[Download] Starting: '{}' from '{}' ({:.2} MB)", filename, username, filesize as f64 / 1_048_576.0);

        {
            let mut downloads = self.downloads.lock().await;
            downloads.insert(filename.to_string(), DownloadProgress {
                filename: filename.to_string(),
                total_bytes: filesize,
                downloaded_bytes: 0,
                status: DownloadStatus::Pending,
            });
        }

        // Get stored P connection or establish new one
        let stored_stream = self.peer_streams.lock().await.pop(username);

        let mut peer_stream = match stored_stream {
            Some(stream) => {
                tracing::info!("[Download] Using stored P connection from search");
                stream
            }
            None => {
                tracing::info!("[Download] No stored P connection, attempting direct connection");

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
                    Ok(stream) => stream,
                    Err(e) => {
                        let reason = format!("Failed to connect to peer: {}", e);
                        self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                        return DownloadResult::Failed { reason };
                    }
                }
            }
        };

        // Send QueueUpload
        if let Err(e) = queue_upload(&mut peer_stream, filename).await {
            let reason = format!("Failed to send QueueUpload: {}", e);
            self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
            return DownloadResult::Failed { reason };
        }

        // Wait for TransferRequest
        let negotiation = match wait_for_transfer_request(&mut peer_stream, filename, 60).await {
            Ok(result) => result,
            Err(e) => {
                let reason = format!("Negotiation failed: {}", e);
                self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                return DownloadResult::Failed { reason };
            }
        };

        match negotiation {
            NegotiationResult::Allowed { filesize: actual_size, transfer_token } => {
                tracing::info!("[Download] Transfer ALLOWED: size={}, token={}", actual_size, transfer_token);
                drop(peer_stream);

                self.update_download_status(filename, DownloadStatus::InProgress).await;
                {
                    let mut downloads = self.downloads.lock().await;
                    if let Some(p) = downloads.get_mut(filename) {
                        p.total_bytes = actual_size;
                    }
                }

                // Wait for F connection via routed waiter
                match self.wait_for_f_connection_routed(transfer_token, 30).await {
                    Ok(conn) => {
                        // Receive file data
                        match super::peer::receive_file_data(
                            conn,
                            actual_size,
                            output_path.to_path_buf(),
                            progress_callback.map(|cb| {
                                // Convert ProgressCallback (Box) to ArcProgressCallback (Arc)
                                let arc_cb: super::peer::ArcProgressCallback = Arc::new(move |dl, total, speed| {
                                    cb(dl, total, speed);
                                });
                                arc_cb
                            }),
                            username,
                        ).await {
                            Ok(bytes) => {
                                self.update_download_status(filename, DownloadStatus::Completed).await;
                                {
                                    let mut downloads = self.downloads.lock().await;
                                    if let Some(p) = downloads.get_mut(filename) {
                                        p.downloaded_bytes = bytes;
                                    }
                                }
                                DownloadResult::Completed { bytes }
                            }
                            Err(e) => {
                                let reason = format!("File transfer failed: {}", e);
                                self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                                DownloadResult::Failed { reason }
                            }
                        }
                    }
                    Err(e) => {
                        let reason = format!("Failed to get F connection: {}", e);
                        self.update_download_status(filename, DownloadStatus::Failed(reason.clone())).await;
                        DownloadResult::Failed { reason }
                    }
                }
            }

            NegotiationResult::Queued { transfer_token, position, reason } => {
                tracing::info!("[Download] QUEUED: {} (position: {:?})", reason, position);
                self.update_download_status(filename, DownloadStatus::Queued).await;

                {
                    let mut pending = self.pending_downloads.lock().await;
                    pending.insert(transfer_token, PendingDownload {
                        username: username.to_string(),
                        filename: filename.to_string(),
                        filesize,
                        transfer_token,
                        output_path: output_path.to_path_buf(),
                        queued_at: std::time::Instant::now(),
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

    // ========================================================================
    // Utility methods
    // ========================================================================

    /// Get download progress
    pub async fn get_download_progress(&self, filename: &str) -> Option<DownloadProgress> {
        self.downloads.lock().await.get(filename).cloned()
    }

    /// Get all downloads
    pub async fn get_all_downloads(&self) -> Vec<DownloadProgress> {
        self.downloads.lock().await.values().cloned().collect()
    }

    /// Get the best file from search results
    pub fn get_best_file(results: &[SearchResult], query: &str, format_filter: Option<&str>) -> Option<ScoredFile> {
        find_best_file(results, query, format_filter)
    }

    /// Get all matching files sorted by score (best first)
    pub fn get_best_files(results: &[SearchResult], query: &str, format_filter: Option<&str>) -> Vec<ScoredFile> {
        find_best_files(results, query, format_filter)
    }

    /// Search and download best matching file
    pub async fn search_and_download(
        &self,
        query: &str,
        output_path: &std::path::Path,
    ) -> Result<(ScoredFile, DownloadResult)> {
        let results = self.search(query, 10).await?;

        if results.is_empty() {
            bail!("No results found for query: {}", query);
        }

        let scored_file = Self::get_best_file(&results, query, None)
            .ok_or_else(|| anyhow::anyhow!("No suitable files found matching query"))?;

        tracing::info!(
            "[SoulseekClient] Selected '{}' from '{}' (score={:.1}, {:?} kbps)",
            scored_file.filename, scored_file.username, scored_file.score, scored_file.bitrate
        );

        let result = self.download_file(
            &scored_file.username,
            &scored_file.filename,
            scored_file.size,
            &scored_file.peer_ip,
            scored_file.peer_port,
            output_path,
            None,
        ).await;

        Ok((scored_file, result))
    }
}
