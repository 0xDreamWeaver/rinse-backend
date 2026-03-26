//! Upload queue manager for handling peer upload requests.
//!
//! Manages an in-memory queue of upload requests from peers, enforces
//! concurrency limits (max upload slots), and provides bandwidth throttling.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicBool, Ordering};
use std::time::{Duration, Instant};
use bytes::Buf;
use tokio::sync::{broadcast, RwLock, Mutex};
use tokio::task::JoinHandle;
use anyhow::Result;

use crate::api::WsEvent;
use crate::protocol::SoulseekClient;
use crate::services::sharing::SharingService;

/// Upload configuration
#[derive(Debug, Clone)]
pub struct UploadConfig {
    /// Maximum concurrent upload slots (default 3)
    pub max_upload_slots: usize,
    /// Maximum upload speed in KB/s (0 = unlimited)
    pub max_upload_speed_kbps: u64,
    /// Whether sharing/uploads are enabled
    pub sharing_enabled: bool,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            max_upload_slots: 3,
            max_upload_speed_kbps: 0,
            sharing_enabled: true,
        }
    }
}

/// Status of an upload entry
#[derive(Debug, Clone)]
pub enum UploadStatus {
    Queued,
    Transferring {
        token: u32,
        bytes_sent: u64,
        started_at: Instant,
    },
    Completed {
        bytes_sent: u64,
        duration: Duration,
    },
    Failed {
        error: String,
    },
}

/// A single upload queue entry
#[derive(Debug, Clone)]
pub struct UploadQueueEntry {
    pub id: u64,
    pub username: String,
    pub virtual_path: String,
    pub real_path: PathBuf,
    pub file_size: u64,
    pub queued_at: Instant,
    pub status: UploadStatus,
}

/// Upload statistics for API responses
#[derive(Debug, Clone, serde::Serialize)]
pub struct UploadStats {
    pub active_uploads: u32,
    pub queued_uploads: usize,
    pub total_bytes_uploaded: u64,
    pub avg_speed_bytes_per_sec: u64,
    pub max_upload_slots: usize,
    pub max_upload_speed_kbps: u64,
    pub sharing_enabled: bool,
}

/// Token bucket rate limiter for bandwidth throttling
pub struct TokenBucket {
    /// Rate in bytes per second (0 = unlimited)
    rate_bytes_per_sec: AtomicU64,
    /// Current available tokens (bytes)
    tokens: Mutex<f64>,
    /// Last refill timestamp
    last_refill: Mutex<Instant>,
}

impl TokenBucket {
    pub fn new(rate_kbps: u64) -> Self {
        Self {
            rate_bytes_per_sec: AtomicU64::new(rate_kbps * 1024),
            tokens: Mutex::new((rate_kbps * 1024) as f64),
            last_refill: Mutex::new(Instant::now()),
        }
    }

    /// Set the rate limit (0 = unlimited)
    pub fn set_rate(&self, rate_kbps: u64) {
        self.rate_bytes_per_sec.store(rate_kbps * 1024, Ordering::Relaxed);
    }

    /// Consume bytes from the bucket, sleeping if necessary
    pub async fn consume(&self, bytes: u64) {
        let rate = self.rate_bytes_per_sec.load(Ordering::Relaxed);
        if rate == 0 {
            return; // Unlimited
        }

        loop {
            // Refill tokens based on elapsed time
            {
                let mut last_refill = self.last_refill.lock().await;
                let mut tokens = self.tokens.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(*last_refill).as_secs_f64();
                *tokens += elapsed * rate as f64;
                // Cap at 1 second worth of tokens
                if *tokens > rate as f64 {
                    *tokens = rate as f64;
                }
                *last_refill = now;

                if *tokens >= bytes as f64 {
                    *tokens -= bytes as f64;
                    return;
                }
            }

            // Not enough tokens, sleep for a bit
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// Upload queue manager service
pub struct UploadService {
    config: Arc<RwLock<UploadConfig>>,
    queue: Arc<Mutex<VecDeque<UploadQueueEntry>>>,
    active_count: Arc<AtomicU32>,
    next_id: Arc<AtomicU64>,
    sharing_service: Arc<SharingService>,
    /// Soulseek client for establishing P/F connections (set after construction)
    slsk_client: Arc<RwLock<Option<Arc<SoulseekClient>>>>,
    ws_broadcast: broadcast::Sender<WsEvent>,
    throttle: Arc<TokenBucket>,
    total_bytes_uploaded: Arc<AtomicU64>,
    checking_queue: Arc<AtomicBool>,
    /// Running total for average speed calculation
    speed_tracker: Arc<Mutex<SpeedTracker>>,
}

/// Tracks upload speed over a rolling window
struct SpeedTracker {
    /// (timestamp, bytes) pairs for the last 30 seconds
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedTracker {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
        }
    }

    fn record(&mut self, bytes: u64) {
        let now = Instant::now();
        self.samples.push_back((now, bytes));
        // Remove samples older than 30 seconds
        while let Some((ts, _)) = self.samples.front() {
            if now.duration_since(*ts) > Duration::from_secs(30) {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    fn avg_speed(&self) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let total_bytes: u64 = self.samples.iter().map(|(_, b)| b).sum();
        let first = self.samples.front().unwrap().0;
        let last = Instant::now();
        let elapsed = last.duration_since(first).as_secs_f64();
        if elapsed > 0.0 {
            (total_bytes as f64 / elapsed) as u64
        } else {
            0
        }
    }
}

impl UploadService {
    /// Create a new UploadService
    pub fn new(
        config: UploadConfig,
        sharing_service: Arc<SharingService>,
        ws_broadcast: broadcast::Sender<WsEvent>,
    ) -> Self {
        let throttle = Arc::new(TokenBucket::new(config.max_upload_speed_kbps));

        Self {
            config: Arc::new(RwLock::new(config)),
            queue: Arc::new(Mutex::new(VecDeque::new())),
            active_count: Arc::new(AtomicU32::new(0)),
            next_id: Arc::new(AtomicU64::new(1)),
            sharing_service,
            slsk_client: Arc::new(RwLock::new(None)),
            ws_broadcast,
            throttle,
            total_bytes_uploaded: Arc::new(AtomicU64::new(0)),
            checking_queue: Arc::new(AtomicBool::new(false)),
            speed_tracker: Arc::new(Mutex::new(SpeedTracker::new())),
        }
    }

    /// Set the Soulseek client reference (called after both are constructed)
    pub async fn set_slsk_client(&self, client: Arc<SoulseekClient>) {
        let mut c = self.slsk_client.write().await;
        *c = Some(client);
    }

    /// Handle a QueueUpload request from a peer.
    /// Returns Ok(position) if queued, or Err with reason if rejected.
    pub async fn handle_queue_upload(&self, username: &str, virtual_path: &str) -> Result<u32, String> {
        let config = self.config.read().await;
        if !config.sharing_enabled {
            return Err("Sharing disabled".to_string());
        }
        drop(config);

        // Validate file exists in share index
        let file = self.sharing_service.get_file(virtual_path).await;
        let file = match file {
            Some(f) => f,
            None => return Err(format!("File not shared: {}", virtual_path)),
        };

        // Verify file still exists on disk
        if !file.real_path.exists() {
            return Err("File not shared".to_string());
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let entry = UploadQueueEntry {
            id,
            username: username.to_string(),
            virtual_path: virtual_path.to_string(),
            real_path: file.real_path,
            file_size: file.size,
            queued_at: Instant::now(),
            status: UploadStatus::Queued,
        };

        let mut queue = self.queue.lock().await;
        let position = queue.len() as u32 + 1;
        queue.push_back(entry);

        tracing::info!(
            "[Upload] Queued upload for '{}': '{}' (position={})",
            username, virtual_path, position
        );

        Ok(position)
    }

    /// Get queue position for a user/file combination
    pub async fn get_queue_position(&self, username: &str, virtual_path: &str) -> Option<u32> {
        let queue = self.queue.lock().await;
        queue.iter().enumerate()
            .find(|(_, e)| e.username == username && e.virtual_path == virtual_path)
            .map(|(i, _)| i as u32 + 1)
    }

    /// Start the periodic queue checker task
    pub fn start_queue_checker(self: &Arc<Self>) -> JoinHandle<()> {
        let service = Arc::clone(self);

        tokio::spawn(async move {
            tracing::info!("[UploadQueue] Queue checker started");
            let mut interval = tokio::time::interval(Duration::from_secs(10));

            loop {
                interval.tick().await;
                service.check_upload_queue().await;
            }
        })
    }

    /// Check the queue and start uploads if slots are available
    async fn check_upload_queue(&self) {
        // Prevent overlapping checks
        if self.checking_queue.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return;
        }

        let config = self.config.read().await;
        let max_slots = config.max_upload_slots;
        let enabled = config.sharing_enabled;
        drop(config);

        if !enabled {
            self.checking_queue.store(false, Ordering::Release);
            return;
        }

        let active = self.active_count.load(Ordering::Relaxed) as usize;
        if active >= max_slots {
            self.checking_queue.store(false, Ordering::Release);
            return;
        }

        let slots_available = max_slots - active;
        let mut to_start = Vec::new();

        {
            let mut queue = self.queue.lock().await;
            for _ in 0..slots_available {
                if let Some(entry) = queue.pop_front() {
                    to_start.push(entry);
                } else {
                    break;
                }
            }
        }

        // Get the client reference (needed for P/F connections)
        let client = self.slsk_client.read().await.clone();
        let client = match client {
            Some(c) => c,
            None => {
                self.checking_queue.store(false, Ordering::Release);
                return;
            }
        };

        for entry in to_start {
            self.active_count.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                "[Upload] Starting upload to '{}': '{}' ({:.2} MB)",
                entry.username, entry.virtual_path, entry.file_size as f64 / 1_048_576.0
            );

            let _ = self.ws_broadcast.send(WsEvent::UploadStarted {
                username: entry.username.clone(),
                filename: entry.virtual_path.clone(),
                file_size: entry.file_size,
            });

            // Spawn upload execution as background task
            let active_count = Arc::clone(&self.active_count);
            let ws_broadcast = self.ws_broadcast.clone();
            let throttle = Arc::clone(&self.throttle);
            let total_bytes = Arc::clone(&self.total_bytes_uploaded);
            let speed_tracker = Arc::clone(&self.speed_tracker);
            let client = Arc::clone(&client);

            tokio::spawn(async move {
                let result = execute_upload(
                    &client,
                    &entry,
                    Some(throttle),
                    ws_broadcast.clone(),
                ).await;

                match result {
                    Ok(bytes_sent) => {
                        tracing::info!(
                            "[Upload] Completed: '{}' to '{}' ({:.2} MB)",
                            entry.virtual_path, entry.username, bytes_sent as f64 / 1_048_576.0
                        );
                        total_bytes.fetch_add(bytes_sent, Ordering::Relaxed);
                        speed_tracker.lock().await.record(bytes_sent);

                        let _ = ws_broadcast.send(WsEvent::UploadCompleted {
                            username: entry.username.clone(),
                            filename: entry.virtual_path.clone(),
                            total_bytes: bytes_sent,
                        });
                    }
                    Err(e) => {
                        tracing::error!(
                            "[Upload] Failed: '{}' to '{}': {}",
                            entry.virtual_path, entry.username, e
                        );
                        let _ = ws_broadcast.send(WsEvent::UploadFailed {
                            username: entry.username.clone(),
                            filename: entry.virtual_path.clone(),
                            error: e.to_string(),
                        });
                    }
                }

                active_count.fetch_sub(1, Ordering::Relaxed);
            });
        }

        self.checking_queue.store(false, Ordering::Release);
    }

    /// Get current upload statistics
    pub async fn stats(&self) -> UploadStats {
        let config = self.config.read().await;
        let queue = self.queue.lock().await;
        let speed = self.speed_tracker.lock().await.avg_speed();

        UploadStats {
            active_uploads: self.active_count.load(Ordering::Relaxed),
            queued_uploads: queue.len(),
            total_bytes_uploaded: self.total_bytes_uploaded.load(Ordering::Relaxed),
            avg_speed_bytes_per_sec: speed,
            max_upload_slots: config.max_upload_slots,
            max_upload_speed_kbps: config.max_upload_speed_kbps,
            sharing_enabled: config.sharing_enabled,
        }
    }

    /// Update configuration at runtime
    pub async fn update_config(&self, new_config: UploadConfig) {
        self.throttle.set_rate(new_config.max_upload_speed_kbps);
        let mut config = self.config.write().await;
        *config = new_config;
        tracing::info!("[Upload] Config updated: slots={}, speed_limit={}KB/s, enabled={}",
            config.max_upload_slots, config.max_upload_speed_kbps, config.sharing_enabled);
    }

    /// Get current config
    pub async fn get_config(&self) -> UploadConfig {
        self.config.read().await.clone()
    }

    /// Get average upload speed in bytes/sec (for reporting to server)
    pub async fn average_speed_bytes_per_sec(&self) -> u64 {
        self.speed_tracker.lock().await.avg_speed()
    }

    /// Get the token bucket for upload throttling
    pub fn throttle(&self) -> &Arc<TokenBucket> {
        &self.throttle
    }

    /// Record uploaded bytes for speed tracking
    pub async fn record_bytes_uploaded(&self, bytes: u64) {
        self.total_bytes_uploaded.fetch_add(bytes, Ordering::Relaxed);
        self.speed_tracker.lock().await.record(bytes);
    }

    /// Decrement active upload count (called after upload completes/fails)
    pub fn release_slot(&self) {
        self.active_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Check if upload slots are available
    pub async fn has_free_slots(&self) -> bool {
        let config = self.config.read().await;
        let active = self.active_count.load(Ordering::Relaxed) as usize;
        active < config.max_upload_slots
    }

    /// Get list of active and queued uploads for API
    pub async fn get_queue_entries(&self) -> Vec<UploadQueueEntry> {
        let queue = self.queue.lock().await;
        queue.iter().cloned().collect()
    }
}

// ============================================================================
// Upload execution
// ============================================================================

use bytes::{BufMut, BytesMut};
use crate::protocol::peer::{
    FileConnection, encode_string, send_message, read_message, send_peer_init,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Execute a file upload to a peer.
///
/// Upload flow:
/// 1. Get or establish P connection to peer
/// 2. Send TransferRequest(direction=1, token, filename, filesize)
/// 3. Wait for TransferReply(allowed=true) from peer
/// 4. Establish F connection (reuse file_conn_waiters infrastructure)
/// 5. Call send_file_data
async fn execute_upload(
    client: &SoulseekClient,
    entry: &UploadQueueEntry,
    throttle: Option<Arc<TokenBucket>>,
    ws_broadcast: broadcast::Sender<WsEvent>,
) -> Result<u64> {
    let transfer_token: u32 = rand::random();

    // Verify file still exists
    if !entry.real_path.exists() {
        anyhow::bail!("File no longer exists: {}", entry.real_path.display());
    }

    // Step 1: Get peer address and establish P connection
    tracing::info!(
        "[Upload] Establishing P connection to '{}' for '{}' (token={})",
        entry.username, entry.virtual_path, transfer_token
    );

    let (peer_ip, peer_port) = match client.router().get_peer_address(&entry.username).await {
        Ok(rx) => {
            match tokio::time::timeout(Duration::from_secs(10), rx).await {
                Ok(Ok(addr)) => (addr.ip, addr.port),
                _ => anyhow::bail!("Could not resolve peer address for '{}'", entry.username),
            }
        }
        Err(e) => anyhow::bail!("Failed to request peer address: {}", e),
    };

    if peer_port == 0 || peer_ip.is_empty() || peer_ip == "0.0.0.0" {
        anyhow::bail!("Peer '{}' has no direct address (port={})", entry.username, peer_port);
    }

    // Loopback detection: if peer IP matches our external IP, use 127.0.0.1
    let connect_ip = if let Some(ext_ip) = client.external_ip() {
        if peer_ip == ext_ip {
            tracing::info!("[Upload] Peer '{}' IP {} matches our external IP, using 127.0.0.1", entry.username, peer_ip);
            "127.0.0.1".to_string()
        } else {
            peer_ip.clone()
        }
    } else {
        peer_ip.clone()
    };

    let addr = format!("{}:{}", connect_ip, peer_port);
    let mut peer_stream = match tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(&addr),
    ).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => anyhow::bail!("Failed to connect to '{}' at {}: {}", entry.username, addr, e),
        Err(_) => anyhow::bail!("Timeout connecting to '{}' at {}", entry.username, addr),
    };

    peer_stream.set_nodelay(true)?;
    send_peer_init(&mut peer_stream, client.username(), "P", 0).await?;

    // Read peer's PeerInit response (they should respond to our PeerInit)
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let first = peer_stream.read_u32_le().await.ok()?;
        if first > 5 && first < 200 {
            let _msg_type = peer_stream.read_u8().await.ok()?;
            let remaining = (first - 1) as usize;
            let mut data = vec![0u8; remaining];
            peer_stream.read_exact(&mut data).await.ok()?;
        }
        Some(())
    }).await;

    // Step 2: Send TransferRequest(direction=1) to peer
    // This tells the peer we want to upload to them
    let mut req_data = BytesMut::new();
    req_data.put_u32_le(1); // direction = 1 (upload)
    req_data.put_u32_le(transfer_token);
    encode_string(&mut req_data, &entry.virtual_path);
    req_data.put_u64_le(entry.file_size);

    send_message(&mut peer_stream, 40, &req_data).await?;
    tracing::info!(
        "[Upload] Sent TransferRequest(dir=1) to '{}' for '{}' (token={}, size={})",
        entry.username, entry.virtual_path, transfer_token, entry.file_size
    );

    // Step 3: Wait for TransferReply from peer
    let reply_timeout = Duration::from_secs(60);
    let start = Instant::now();

    loop {
        if start.elapsed() > reply_timeout {
            anyhow::bail!("Timeout waiting for TransferReply from '{}'", entry.username);
        }

        let remaining = reply_timeout.saturating_sub(start.elapsed());
        let (code, data) = match tokio::time::timeout(remaining, read_message(&mut peer_stream)).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(e)) => anyhow::bail!("Read error waiting for TransferReply: {}", e),
            Err(_) => anyhow::bail!("Timeout waiting for TransferReply from '{}'", entry.username),
        };

        if code == 41 {
            // TransferReply
            let mut buf = BytesMut::from(&data[..]);
            if buf.remaining() < 5 {
                anyhow::bail!("TransferReply too short");
            }
            let reply_token = buf.get_u32_le();
            let allowed = buf.get_u8();

            if reply_token != transfer_token {
                tracing::debug!("[Upload] TransferReply token mismatch: expected {}, got {}", transfer_token, reply_token);
                continue;
            }

            if allowed != 1 {
                // Read denial reason
                let reason = if buf.remaining() >= 4 {
                    crate::protocol::messages::decode_string(&mut buf).unwrap_or_else(|_| "Unknown".to_string())
                } else {
                    "Transfer denied".to_string()
                };
                anyhow::bail!("Peer '{}' denied upload: {}", entry.username, reason);
            }

            tracing::info!("[Upload] TransferReply: allowed by '{}'", entry.username);
            break;
        } else {
            tracing::debug!("[Upload] Ignoring peer message code {} while waiting for TransferReply", code);
        }
    }

    // Release P connection
    drop(peer_stream);

    // Step 4: Establish F connection for upload
    // Try to connect directly to peer for file transfer
    tracing::info!("[Upload] Establishing F connection to '{}' (token={})", entry.username, transfer_token);

    let file_conn = {
        let addr = format!("{}:{}", connect_ip, peer_port);
        let mut stream = match tokio::time::timeout(
            Duration::from_secs(10),
            TcpStream::connect(&addr),
        ).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => anyhow::bail!("F connection to '{}' failed: {}", entry.username, e),
            Err(_) => anyhow::bail!("F connection timeout to '{}'", entry.username),
        };

        stream.set_nodelay(true)?;

        // Send PeerInit type "F"
        send_peer_init(&mut stream, client.username(), "F", 0).await?;

        // Read peer response (optional)
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            let first = stream.read_u32_le().await.ok()?;
            if first > 5 && first < 200 {
                let _msg_type = stream.read_u8().await.ok()?;
                let remaining = (first - 1) as usize;
                let mut data = vec![0u8; remaining];
                stream.read_exact(&mut data).await.ok()?;
            }
            Some(())
        }).await;

        // Send FileTransferInit with our transfer token
        stream.write_u32_le(transfer_token).await?;
        stream.flush().await?;

        FileConnection { stream, transfer_token }
    };

    // Step 5: Send file data
    let username = entry.username.clone();
    let virtual_path = entry.virtual_path.clone();
    let file_size = entry.file_size;

    // Create progress callback for WebSocket
    let ws = ws_broadcast.clone();
    let un = username.clone();
    let vp = virtual_path.clone();
    let progress_callback: Option<crate::protocol::ArcProgressCallback> = Some(Arc::new(move |bytes_sent, total, speed| {
        let _ = ws.send(WsEvent::UploadProgress {
            username: un.clone(),
            filename: vp.clone(),
            bytes_sent,
            total_bytes: total,
            speed_kbps: speed,
        });
    }));

    let bytes_sent = crate::protocol::peer::send_file_data(
        file_conn,
        entry.real_path.clone(),
        file_size,
        throttle,
        progress_callback,
        &username,
    ).await?;

    Ok(bytes_sent)
}
