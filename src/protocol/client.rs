use anyhow::{Result, bail, Context};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use rand::Rng;

use super::connection::{ServerConnection, PeerConnection, FileTransferConnection};
use super::messages::{ServerMessage, PeerMessage, SearchFile, FileAttribute, decode_string};

/// Search result from the network
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub username: String,
    pub files: Vec<SearchFile>,
    pub has_slots: bool,
    pub avg_speed: u32,
    pub queue_length: u64,
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
    Queued,
    InProgress,
    Completed,
    Failed(String),
}

/// Main Soulseek client
pub struct SoulseekClient {
    server: Arc<ServerConnection>,
    username: String,
    /// Token -> Search results
    search_results: Arc<RwLock<HashMap<u32, Vec<SearchResult>>>>,
    /// Token -> Oneshot sender for notifying search completion
    search_waiters: Arc<Mutex<HashMap<u32, oneshot::Sender<()>>>>,
    /// Download progress tracking
    downloads: Arc<RwLock<HashMap<String, DownloadProgress>>>,
    /// Active peer connections
    peer_connections: Arc<Mutex<HashMap<String, PeerConnection>>>,
}

/// Default port for incoming peer connections
const PEER_LISTEN_PORT: u16 = 2234;

impl SoulseekClient {
    /// Create a new client and connect to the server
    pub async fn connect(username: &str, password: &str) -> Result<Self> {
        let server = ServerConnection::connect().await?;
        server.login(username, password).await?;

        // Set shared folders/files to 0 for now (we'll update this later)
        server.set_shared_counts(0, 0).await?;

        // Tell server what port we listen on for peer connections
        server.set_wait_port(PEER_LISTEN_PORT as u32).await?;
        tracing::info!("[SoulseekClient] Set wait port to {}", PEER_LISTEN_PORT);

        let client = Self {
            server: Arc::new(server),
            username: username.to_string(),
            search_results: Arc::new(RwLock::new(HashMap::new())),
            search_waiters: Arc::new(Mutex::new(HashMap::new())),
            downloads: Arc::new(RwLock::new(HashMap::new())),
            peer_connections: Arc::new(Mutex::new(HashMap::new())),
        };

        // Start background task to receive server messages
        client.start_message_receiver();

        // Start peer listener for incoming search results
        client.start_peer_listener();

        Ok(client)
    }

    /// Start background task to receive and process server messages
    fn start_message_receiver(&self) {
        let server = Arc::clone(&self.server);
        let search_results = Arc::clone(&self.search_results);
        let search_waiters = Arc::clone(&self.search_waiters);

        tokio::spawn(async move {
            loop {
                match server.receive().await {
                    Ok(Some(msg)) => {
                        match msg {
                            ServerMessage::ServerPong => {
                                // Server keep-alive
                                tracing::trace!("[MessageReceiver] Received server pong");
                            }
                            ServerMessage::Relogged => {
                                tracing::warn!("[MessageReceiver] Account logged in from another location");
                            }
                            ServerMessage::EmbeddedMessage { message_type, data } => {
                                tracing::info!(
                                    "[MessageReceiver] Received EmbeddedMessage: type={}, {} bytes",
                                    message_type,
                                    data.len()
                                );

                                // Type 3 = Distributed search result
                                if message_type == 3 {
                                    // Try to parse as search result
                                    match Self::parse_distributed_search_result(&data) {
                                        Ok((token, result)) => {
                                            tracing::info!(
                                                "[MessageReceiver] Search result from '{}': {} files (token={})",
                                                result.username,
                                                result.files.len(),
                                                token
                                            );

                                            // Store the result
                                            let mut results = search_results.write().await;
                                            if let Some(list) = results.get_mut(&token) {
                                                list.push(result);
                                            } else {
                                                tracing::debug!("[MessageReceiver] No waiter for token {}", token);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "[MessageReceiver] Failed to parse search result: {}",
                                                e
                                            );
                                        }
                                    }
                                }
                            }
                            ServerMessage::ConnectToPeerRequest { username, connection_type, ip, port, token, .. } => {
                                tracing::info!(
                                    "[MessageReceiver] ConnectToPeer request: user='{}', type='{}', addr={}:{}, token={}",
                                    username, connection_type, ip, port, token
                                );

                                // For peer connections (search results), connect to the peer
                                // Type 'P' = Peer connection (for search results, browsing, etc.)
                                // Type 'F' = File transfer
                                // Type 'D' = Distributed network
                                if connection_type == "P" {
                                    let search_results_clone = Arc::clone(&search_results);
                                    let username_clone = username.clone();
                                    let ip_clone = ip.clone();

                                    tokio::spawn(async move {
                                        match Self::connect_and_receive_search_result(&ip_clone, port, &username_clone, token).await {
                                            Ok(Some((result_token, result))) => {
                                                tracing::info!(
                                                    "[MessageReceiver] Got search result from '{}': {} files",
                                                    result.username,
                                                    result.files.len()
                                                );

                                                let mut results = search_results_clone.write().await;
                                                if let Some(list) = results.get_mut(&result_token) {
                                                    list.push(result);
                                                }
                                            }
                                            Ok(None) => {
                                                tracing::debug!("[MessageReceiver] No search result from peer");
                                            }
                                            Err(e) => {
                                                tracing::warn!("[MessageReceiver] Failed to get search result from peer: {}", e);
                                            }
                                        }
                                    });
                                }
                            }
                            ServerMessage::Unknown { code, data } => {
                                tracing::debug!(
                                    "[MessageReceiver] Unknown message code={} ({} bytes)",
                                    code,
                                    data.len()
                                );
                            }
                            _ => {
                                tracing::debug!("[MessageReceiver] Received: {:?}", msg);
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::error!("[MessageReceiver] Server connection closed");
                        break;
                    }
                    Err(e) => {
                        tracing::error!("[MessageReceiver] Error receiving from server: {}", e);
                        break;
                    }
                }
            }
        });
    }

    /// Parse a distributed search result from embedded message data
    fn parse_distributed_search_result(data: &[u8]) -> Result<(u32, SearchResult)> {
        use bytes::{Buf, BytesMut};

        let mut buf = BytesMut::from(data);

        if buf.remaining() < 4 {
            bail!("Not enough data for username");
        }

        let username = decode_string(&mut buf)?;
        let token = buf.get_u32_le();
        let file_count = buf.get_u32_le();

        tracing::debug!(
            "[Protocol] Parsing search result: user='{}', token={}, files={}",
            username,
            token,
            file_count
        );

        let mut files = Vec::new();
        for i in 0..file_count {
            if buf.remaining() < 1 {
                tracing::warn!("[Protocol] Ran out of data at file {}/{}", i, file_count);
                break;
            }

            let code = buf.get_u8();
            let filename = decode_string(&mut buf)?;
            let size = buf.get_u64_le();
            let extension = decode_string(&mut buf)?;
            let attr_count = buf.get_u32_le();

            let mut attributes = Vec::new();
            for _ in 0..attr_count {
                if buf.remaining() < 8 {
                    break;
                }
                let attribute_type = buf.get_u32_le();
                let value = buf.get_u32_le();
                attributes.push(FileAttribute { attribute_type, value });
            }

            files.push(SearchFile { code, filename, size, extension, attributes });
        }

        // Read trailing info if available
        let has_slots = if buf.remaining() >= 1 { buf.get_u8() != 0 } else { false };
        let avg_speed = if buf.remaining() >= 4 { buf.get_u32_le() } else { 0 };
        let queue_length = if buf.remaining() >= 8 { buf.get_u64_le() } else { 0 };

        Ok((token, SearchResult {
            username,
            files,
            has_slots,
            avg_speed,
            queue_length,
        }))
    }

    /// Start peer listener for incoming connections (search results)
    fn start_peer_listener(&self) {
        let search_results = Arc::clone(&self.search_results);
        let username = self.username.clone();

        tokio::spawn(async move {
            use tokio::net::TcpListener;

            let addr = format!("0.0.0.0:{}", PEER_LISTEN_PORT);
            let listener = match TcpListener::bind(&addr).await {
                Ok(l) => {
                    tracing::info!("[PeerListener] Listening for peer connections on {}", addr);
                    l
                }
                Err(e) => {
                    tracing::error!("[PeerListener] Failed to bind to {}: {}", addr, e);
                    return;
                }
            };

            loop {
                match listener.accept().await {
                    Ok((stream, peer_addr)) => {
                        tracing::info!("[PeerListener] Incoming connection from {}", peer_addr);

                        let search_results_clone = Arc::clone(&search_results);
                        let our_username = username.clone();

                        tokio::spawn(async move {
                            match Self::handle_incoming_peer(stream, &our_username).await {
                                Ok(Some((token, result))) => {
                                    tracing::info!(
                                        "[PeerListener] Received search result from '{}': {} files (token={})",
                                        result.username,
                                        result.files.len(),
                                        token
                                    );

                                    let mut results = search_results_clone.write().await;
                                    if let Some(list) = results.get_mut(&token) {
                                        list.push(result);
                                    } else {
                                        tracing::debug!("[PeerListener] No waiter for token {}", token);
                                    }
                                }
                                Ok(None) => {
                                    tracing::debug!("[PeerListener] Connection closed without search result");
                                }
                                Err(e) => {
                                    tracing::warn!("[PeerListener] Error handling peer connection: {}", e);
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::error!("[PeerListener] Accept error: {}", e);
                    }
                }
            }
        });
    }

    /// Handle an incoming peer connection
    async fn handle_incoming_peer(
        mut stream: tokio::net::TcpStream,
        our_username: &str,
    ) -> Result<Option<(u32, SearchResult)>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use bytes::{Buf, BytesMut};

        // Read peer init message (1 byte type)
        let init_type = stream.read_u8().await?;
        tracing::debug!("[PeerListener] Peer init type: {}", init_type);

        if init_type == 0 {
            // PierceFirewall - read token
            let token = stream.read_u32_le().await?;
            tracing::debug!("[PeerListener] PierceFirewall with token {}", token);

            // Now expect the peer to send FileSearchResponse
            return Self::read_file_search_response(&mut stream).await;
        } else if init_type == 1 {
            // PeerInit - read username, connection type, token
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).await?;
            let username_len = u32::from_le_bytes(len_buf) as usize;
            let mut username_buf = vec![0u8; username_len];
            stream.read_exact(&mut username_buf).await?;
            let peer_username = String::from_utf8_lossy(&username_buf).to_string();

            stream.read_exact(&mut len_buf).await?;
            let conn_type_len = u32::from_le_bytes(len_buf) as usize;
            let mut conn_type_buf = vec![0u8; conn_type_len];
            stream.read_exact(&mut conn_type_buf).await?;
            let conn_type = String::from_utf8_lossy(&conn_type_buf).to_string();

            let token = stream.read_u32_le().await?;

            tracing::info!(
                "[PeerListener] PeerInit from '{}', type='{}', token={}",
                peer_username, conn_type, token
            );

            // Respond with our own PeerInit
            // Format: [length:u32 LE][type:u8][username:string][conn_type:string][token:u32 LE]
            // Length = 1 + 4 + username.len() + 4 + conn_type.len() + 4
            let msg_len = 1 + 4 + our_username.len() + 4 + conn_type.len() + 4;
            stream.write_u32_le(msg_len as u32).await?;
            stream.write_u8(1).await?; // PeerInit type
            stream.write_u32_le(our_username.len() as u32).await?;
            stream.write_all(our_username.as_bytes()).await?;
            stream.write_u32_le(conn_type.len() as u32).await?;
            stream.write_all(conn_type.as_bytes()).await?;
            stream.write_u32_le(0).await?; // token
            stream.flush().await?;

            // If connection type is "F", expect FileSearchResponse
            if conn_type == "F" {
                return Self::read_file_search_response(&mut stream).await;
            }
        }

        Ok(None)
    }

    /// Connect to peer and receive search result (indirect connection via server)
    async fn connect_and_receive_search_result(
        ip: &str,
        port: u16,
        username: &str,
        token: u32,
    ) -> Result<Option<(u32, SearchResult)>> {
        use tokio::net::TcpStream;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use std::time::Duration;

        let addr = format!("{}:{}", ip, port);
        tracing::debug!("[PeerConnect] Connecting to {} at {} for search result", username, addr);

        // Connect with timeout
        let stream_result = tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect(&addr)
        ).await;

        let mut stream = match stream_result {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                tracing::debug!("[PeerConnect] Connection to {} failed: {}", username, e);
                return Ok(None);
            }
            Err(_) => {
                tracing::debug!("[PeerConnect] Connection to {} timed out", username);
                return Ok(None);
            }
        };

        tracing::debug!("[PeerConnect] Connected to {}, sending PierceFirewall", username);

        // Send PierceFirewall with token
        // Format: [length:u32 LE][type:u8][token:u32 LE]
        // Length = 5 (1 byte type + 4 bytes token)
        stream.write_u32_le(5).await?; // Message length
        stream.write_u8(0).await?; // PierceFirewall type
        stream.write_u32_le(token).await?;
        stream.flush().await?;

        tracing::debug!("[PeerConnect] Sent PierceFirewall to {}, waiting for response", username);

        // Read response with timeout
        match tokio::time::timeout(
            Duration::from_secs(10),
            Self::read_file_search_response(&mut stream)
        ).await {
            Ok(result) => result,
            Err(_) => {
                tracing::debug!("[PeerConnect] Response from {} timed out", username);
                Ok(None)
            }
        }
    }

    /// Read a FileSearchResponse from a peer stream
    async fn read_file_search_response(
        stream: &mut tokio::net::TcpStream,
    ) -> Result<Option<(u32, SearchResult)>> {
        use tokio::io::AsyncReadExt;
        use bytes::{Buf, BytesMut};
        use flate2::read::ZlibDecoder;
        use std::io::Read;

        // Read message length
        let msg_len = stream.read_u32_le().await? as usize;

        if msg_len == 0 || msg_len > 10_000_000 {
            tracing::warn!("[PeerConnect] Invalid message length: {}", msg_len);
            return Ok(None);
        }

        // Read message code
        let code = stream.read_u32_le().await?;
        tracing::debug!("[PeerConnect] Message code: {}, length: {}", code, msg_len);

        if code != 9 {
            // Not a FileSearchResponse
            tracing::debug!("[PeerConnect] Not a FileSearchResponse (code={})", code);
            return Ok(None);
        }

        // Read compressed data
        let compressed_len = msg_len - 4; // minus the code we already read
        let mut compressed = vec![0u8; compressed_len];
        stream.read_exact(&mut compressed).await?;

        // Decompress with zlib
        let mut decoder = ZlibDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed)?;

        tracing::debug!(
            "[PeerConnect] Decompressed {} bytes -> {} bytes",
            compressed_len,
            decompressed.len()
        );

        // Parse the decompressed data
        Self::parse_file_search_response(&decompressed)
    }

    /// Parse FileSearchResponse data
    fn parse_file_search_response(data: &[u8]) -> Result<Option<(u32, SearchResult)>> {
        use bytes::{Buf, BytesMut};

        let mut buf = BytesMut::from(data);

        if buf.remaining() < 4 {
            return Ok(None);
        }

        let username = decode_string(&mut buf)?;
        let token = buf.get_u32_le();
        let file_count = buf.get_u32_le();

        tracing::debug!(
            "[PeerConnect] FileSearchResponse: user='{}', token={}, files={}",
            username, token, file_count
        );

        let mut files = Vec::new();
        for _ in 0..file_count {
            if buf.remaining() < 1 {
                break;
            }

            let code = buf.get_u8();
            let filename = decode_string(&mut buf)?;
            let size = buf.get_u64_le();
            let extension = decode_string(&mut buf)?;
            let attr_count = buf.get_u32_le();

            let mut attributes = Vec::new();
            for _ in 0..attr_count {
                if buf.remaining() < 8 {
                    break;
                }
                let attribute_type = buf.get_u32_le();
                let value = buf.get_u32_le();
                attributes.push(FileAttribute { attribute_type, value });
            }

            files.push(SearchFile { code, filename, size, extension, attributes });
        }

        let has_slots = if buf.remaining() >= 1 { buf.get_u8() != 0 } else { false };
        let avg_speed = if buf.remaining() >= 4 { buf.get_u32_le() } else { 0 };
        let queue_length = if buf.remaining() >= 8 { buf.get_u64_le() } else { 0 };

        Ok(Some((token, SearchResult {
            username,
            files,
            has_slots,
            avg_speed,
            queue_length,
        })))
    }

    /// Generate a unique token
    fn generate_token() -> u32 {
        rand::thread_rng().gen()
    }

    /// Search for files on the network
    pub async fn search(&self, query: &str, timeout_secs: u64) -> Result<Vec<SearchResult>> {
        let token = Self::generate_token();

        tracing::info!("[SoulseekClient] === INITIATING SEARCH ===");
        tracing::info!("[SoulseekClient] Query: '{}'", query);
        tracing::info!("[SoulseekClient] Token: {}", token);
        tracing::info!("[SoulseekClient] Timeout: {} seconds", timeout_secs);

        // Register waiter for search completion
        let (tx, rx) = oneshot::channel();
        self.search_waiters.lock().await.insert(token, tx);

        // Clear any previous results for this token
        self.search_results.write().await.insert(token, Vec::new());

        // Send search request
        tracing::debug!("[SoulseekClient] About to send file_search...");
        self.server.file_search(token, query).await?;
        tracing::info!("[SoulseekClient] Search request sent to server, waiting for results...");

        // Wait for results with timeout
        tokio::select! {
            _ = rx => {
                tracing::info!("[SoulseekClient] Search completed via signal");
            }
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(timeout_secs)) => {
                tracing::info!("[SoulseekClient] Search timeout after {} seconds", timeout_secs);
            }
        }

        // Collect results
        let results = self.search_results.read().await
            .get(&token)
            .cloned()
            .unwrap_or_default();

        tracing::info!("[SoulseekClient] === SEARCH COMPLETE ===");
        tracing::info!("[SoulseekClient] Total results collected: {} users", results.len());

        let total_files: usize = results.iter().map(|r| r.files.len()).sum();
        tracing::info!("[SoulseekClient] Total files across all users: {}", total_files);

        // Cleanup
        self.search_waiters.lock().await.remove(&token);
        self.search_results.write().await.remove(&token);

        Ok(results)
    }

    /// Get the best file from search results based on bitrate
    pub fn get_best_file(results: &[SearchResult]) -> Option<(String, SearchFile)> {
        results.iter()
            .flat_map(|result| {
                result.files.iter().map(move |file| (result.username.clone(), file.clone()))
            })
            .max_by_key(|(_, file)| file.bitrate().unwrap_or(0))
    }

    /// Connect to a peer
    async fn connect_to_peer(&self, username: &str) -> Result<PeerConnection> {
        // Check if we already have a connection and remove it
        {
            let mut peers = self.peer_connections.lock().await;
            if let Some(conn) = peers.remove(username) {
                return Ok(conn);
            }
        }

        // Get peer address from server
        let (ip, port) = self.server.get_peer_address(username).await?;

        // Connect to peer
        let conn = PeerConnection::connect(&ip, port, username.to_string()).await?;

        Ok(conn)
    }

    /// Download a file from a peer
    pub async fn download_file(
        &self,
        username: &str,
        filename: &str,
        output_path: &std::path::Path,
    ) -> Result<u64> {
        // Update download status
        {
            let mut downloads = self.downloads.write().await;
            downloads.insert(filename.to_string(), DownloadProgress {
                filename: filename.to_string(),
                total_bytes: 0,
                downloaded_bytes: 0,
                status: DownloadStatus::Queued,
            });
        }

        // Connect to peer
        let mut peer_conn = self.connect_to_peer(username).await
            .context("Failed to connect to peer")?;

        let token = Self::generate_token();

        // Request transfer
        let filesize = match peer_conn.request_transfer(token, filename).await {
            Ok(size) => size,
            Err(e) => {
                // Update status to failed
                self.downloads.write().await
                    .get_mut(filename)
                    .map(|p| p.status = DownloadStatus::Failed(e.to_string()));
                return Err(e);
            }
        };

        // Update total bytes
        {
            let mut downloads = self.downloads.write().await;
            if let Some(progress) = downloads.get_mut(filename) {
                progress.total_bytes = filesize;
                progress.status = DownloadStatus::InProgress;
            }
        }

        // Get peer address again for file transfer connection
        let (ip, port) = self.server.get_peer_address(username).await?;

        // Open file transfer connection
        let mut file_conn = FileTransferConnection::connect(&ip, port).await?;
        file_conn.init(token).await?;

        // Download the file
        match file_conn.download_file(output_path).await {
            Ok(bytes) => {
                // Update status to completed
                self.downloads.write().await
                    .get_mut(filename)
                    .map(|p| {
                        p.downloaded_bytes = bytes;
                        p.status = DownloadStatus::Completed;
                    });
                Ok(bytes)
            }
            Err(e) => {
                // Update status to failed
                self.downloads.write().await
                    .get_mut(filename)
                    .map(|p| p.status = DownloadStatus::Failed(e.to_string()));
                Err(e)
            }
        }
    }

    /// Get download progress for a file
    pub async fn get_download_progress(&self, filename: &str) -> Option<DownloadProgress> {
        self.downloads.read().await.get(filename).cloned()
    }

    /// Get all download progress
    pub async fn get_all_downloads(&self) -> Vec<DownloadProgress> {
        self.downloads.read().await.values().cloned().collect()
    }

    /// Search and download best file
    pub async fn search_and_download(
        &self,
        query: &str,
        output_path: &std::path::Path,
    ) -> Result<(String, u64)> {
        // Search for the file
        let results = self.search(query, 30).await?;

        if results.is_empty() {
            bail!("No results found for query: {}", query);
        }

        // Get best file
        let (username, file) = Self::get_best_file(&results)
            .ok_or_else(|| anyhow::anyhow!("No suitable files found"))?;

        tracing::info!(
            "Selected file '{}' from user '{}' (bitrate: {:?})",
            file.filename,
            username,
            file.bitrate()
        );

        // Attempt to download
        match self.download_file(&username, &file.filename, output_path).await {
            Ok(bytes) => Ok((file.filename, bytes)),
            Err(e) => {
                tracing::warn!("First download attempt failed: {}. Trying next best file...", e);

                // Try next best file
                let remaining_results: Vec<_> = results.iter()
                    .filter(|r| r.username != username)
                    .cloned()
                    .collect();

                if let Some((username2, file2)) = Self::get_best_file(&remaining_results) {
                    tracing::info!(
                        "Retrying with file '{}' from user '{}'",
                        file2.filename,
                        username2
                    );

                    self.download_file(&username2, &file2.filename, output_path).await
                        .map(|bytes| (file2.filename, bytes))
                } else {
                    bail!("Download failed and no alternative files available: {}", e);
                }
            }
        }
    }
}
