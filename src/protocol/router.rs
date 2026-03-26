//! Server message router for multiplexed Soulseek server connection.
//!
//! Splits the single server TCP stream into read/write halves and dispatches
//! incoming messages to appropriate handlers. This enables concurrent searches
//! and downloads without requiring `&mut self` on the client.
//!
//! Architecture:
//! ```text
//! ServerRouter
//! ├── writer: Arc<Mutex<OwnedWriteHalf>>      ← shared by all callers
//! └── reader task (spawned)                     ← routes messages:
//!     ├── ConnectToPeer "P" → connect_to_peer_tx (central handler in client)
//!     ├── ConnectToPeer "F" → spawns indirect F connection → file_conn_tx
//!     ├── GetPeerAddress (code 3) → peer_address_subs[username]
//!     └── other → logged and ignored
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use bytes::{Buf, BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use super::messages::decode_string;
use super::peer::{FileConnection, encode_string};

/// A parsed ConnectToPeer message from the server
#[derive(Debug, Clone)]
pub struct ConnectToPeerMsg {
    pub username: String,
    pub conn_type: String,
    pub ip: String,
    pub port: u32,
    pub token: u32,
}

/// Parsed GetPeerAddress response
#[derive(Debug, Clone)]
pub struct PeerAddressResponse {
    pub username: String,
    pub ip: String,
    pub port: u32,
}

/// A private chat message received from or sent to the Soulseek network
#[derive(Debug, Clone, serde::Serialize)]
pub struct ChatMessage {
    /// Message ID assigned by the server (for incoming messages)
    pub message_id: Option<u32>,
    /// Unix timestamp from the server
    pub timestamp: u32,
    /// The other user (sender for incoming, recipient for outgoing)
    pub username: String,
    /// The message text
    pub message: String,
    /// Whether this message is incoming (from peer) or outgoing (from us)
    pub incoming: bool,
    /// Whether this was a new message (not a replay of an unacknowledged one)
    pub is_new: bool,
}

/// Server message router that multiplexes the single Soulseek server connection.
///
/// The router spawns a background task that reads from the server stream and
/// dispatches messages to appropriate handlers. Writing is done through a shared Mutex.
#[derive(Clone)]
pub struct ServerRouter {
    /// Write half of the server connection, shared by all callers
    writer: Arc<Mutex<OwnedWriteHalf>>,
    /// Per-username peer address response channels
    peer_address_subs: Arc<Mutex<HashMap<String, Vec<oneshot::Sender<PeerAddressResponse>>>>>,
    /// Our username for protocol handshakes
    username: String,
    /// Our external IP as reported by the server (for loopback detection)
    external_ip: Option<String>,
    /// Broadcast channel for incoming chat messages
    chat_tx: broadcast::Sender<ChatMessage>,
    /// In-memory chat history (recent messages)
    chat_history: Arc<Mutex<Vec<ChatMessage>>>,
}

impl ServerRouter {
    /// Create a new ServerRouter from a split TCP stream.
    ///
    /// Spawns a background reader task that dispatches incoming messages.
    ///
    /// # Arguments
    /// - `reader` / `writer` - Split halves of the server TCP stream
    /// - `file_conn_tx` - Channel for F connections from indirect ConnectToPeer "F" messages
    /// - `connect_to_peer_tx` - Channel for ConnectToPeer "P" messages (handled by client)
    /// - `username` - Our username for protocol handshakes
    pub fn new(
        reader: OwnedReadHalf,
        writer: OwnedWriteHalf,
        file_conn_tx: mpsc::Sender<FileConnection>,
        connect_to_peer_tx: mpsc::Sender<ConnectToPeerMsg>,
        username: String,
        external_ip: Option<String>,
    ) -> Self {
        let writer = Arc::new(Mutex::new(writer));
        let peer_address_subs: Arc<Mutex<HashMap<String, Vec<oneshot::Sender<PeerAddressResponse>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (chat_tx, _) = broadcast::channel(256);
        let chat_history: Arc<Mutex<Vec<ChatMessage>>> = Arc::new(Mutex::new(Vec::new()));

        let router = Self {
            writer: Arc::clone(&writer),
            peer_address_subs: Arc::clone(&peer_address_subs),
            username: username.clone(),
            external_ip: external_ip.clone(),
            chat_tx: chat_tx.clone(),
            chat_history: Arc::clone(&chat_history),
        };

        // Spawn the reader task
        Self::spawn_reader(
            reader,
            Arc::clone(&writer),
            peer_address_subs,
            file_conn_tx,
            connect_to_peer_tx,
            username,
            external_ip,
            chat_tx,
            chat_history,
        );

        router
    }

    /// Spawn the background reader task that reads from the server and dispatches messages.
    fn spawn_reader(
        mut reader: OwnedReadHalf,
        writer: Arc<Mutex<OwnedWriteHalf>>,
        peer_address_subs: Arc<Mutex<HashMap<String, Vec<oneshot::Sender<PeerAddressResponse>>>>>,
        file_conn_tx: mpsc::Sender<FileConnection>,
        connect_to_peer_tx: mpsc::Sender<ConnectToPeerMsg>,
        our_username: String,
        our_external_ip: Option<String>,
        chat_tx: broadcast::Sender<ChatMessage>,
        chat_history: Arc<Mutex<Vec<ChatMessage>>>,
    ) {
        tokio::spawn(async move {
            tracing::info!("[ServerRouter] Reader task started");

            loop {
                // Read message length
                let msg_len = match reader.read_u32_le().await {
                    Ok(len) => len as usize,
                    Err(e) => {
                        tracing::error!("[ServerRouter] Server connection lost (read length): {}", e);
                        break;
                    }
                };

                if msg_len < 4 {
                    tracing::warn!("[ServerRouter] Invalid message length: {}", msg_len);
                    continue;
                }

                // Read message code
                let code = match reader.read_u32_le().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("[ServerRouter] Server connection lost (read code): {}", e);
                        break;
                    }
                };

                // Read message data
                let data_len = msg_len - 4;
                let mut data = vec![0u8; data_len];
                if data_len > 0 {
                    if let Err(e) = reader.read_exact(&mut data).await {
                        tracing::error!("[ServerRouter] Server connection lost (read data): {}", e);
                        break;
                    }
                }

                match code {
                    18 => {
                        // ConnectToPeer
                        match Self::parse_connect_to_peer(&data) {
                            Ok(msg) => {
                                if msg.conn_type == "P" {
                                    tracing::info!(
                                        "[ServerRouter] ConnectToPeer P from '{}' at {}:{} (token={})",
                                        msg.username, msg.ip, msg.port, msg.token
                                    );
                                    // Send to central handler in client
                                    if let Err(_) = connect_to_peer_tx.send(msg).await {
                                        tracing::warn!("[ServerRouter] ConnectToPeer P channel closed");
                                    }
                                } else if msg.conn_type == "F" {
                                    // Indirect F connection - connect to peer in background
                                    let file_tx = file_conn_tx.clone();
                                    let our_user = our_username.clone();
                                    let ext_ip = our_external_ip.clone();
                                    tokio::spawn(async move {
                                        // Loopback detection for F connections
                                        let connect_ip = if let Some(ref ext) = ext_ip {
                                            if msg.ip == *ext {
                                                tracing::info!(
                                                    "[ServerRouter] ConnectToPeer F from '{}' IP {} matches our external IP, using 127.0.0.1",
                                                    msg.username, msg.ip
                                                );
                                                "127.0.0.1".to_string()
                                            } else {
                                                msg.ip.clone()
                                            }
                                        } else {
                                            msg.ip.clone()
                                        };

                                        tracing::debug!(
                                            "[ServerRouter] ConnectToPeer F from '{}' at {}:{} (token={})",
                                            msg.username, connect_ip, msg.port, msg.token
                                        );
                                        match super::peer::connect_for_indirect_transfer(
                                            &connect_ip, msg.port, msg.token, &our_user,
                                        ).await {
                                            Ok(conn) => {
                                                let _ = file_tx.send(conn).await;
                                            }
                                            Err(e) => {
                                                tracing::debug!(
                                                    "[ServerRouter] Indirect F connection failed: {}",
                                                    e
                                                );
                                            }
                                        }
                                    });
                                }
                            }
                            Err(e) => {
                                tracing::warn!("[ServerRouter] Failed to parse ConnectToPeer: {}", e);
                            }
                        }
                    }
                    3 => {
                        // GetPeerAddress response
                        match Self::parse_peer_address(&data) {
                            Ok(response) => {
                                let key = response.username.to_lowercase();
                                let mut subs = peer_address_subs.lock().await;
                                if let Some(waiters) = subs.remove(&key) {
                                    for tx in waiters {
                                        let _ = tx.send(response.clone());
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!("[ServerRouter] Failed to parse PeerAddress: {}", e);
                            }
                        }
                    }
                    22 => {
                        // MessageUser - private chat message
                        match Self::parse_chat_message(&data) {
                            Ok(msg) => {
                                tracing::info!(
                                    "[Chat] Message from '{}': {}",
                                    msg.username, msg.message
                                );

                                // Send acknowledgment (code 23) so the server stops resending
                                if let Some(msg_id) = msg.message_id {
                                    let mut ack = BytesMut::new();
                                    ack.put_u32_le(msg_id);
                                    let msg_len = 4 + ack.len();
                                    let mut w = writer.lock().await;
                                    let _ = w.write_u32_le(msg_len as u32).await;
                                    let _ = w.write_u32_le(23).await;
                                    let _ = w.write_all(&ack).await;
                                    let _ = w.flush().await;
                                }

                                // Store in history (keep last 500 messages)
                                {
                                    let mut history = chat_history.lock().await;
                                    history.push(msg.clone());
                                    if history.len() > 500 {
                                        let excess = history.len() - 500;
                                        history.drain(..excess);
                                    }
                                }

                                // Broadcast to subscribers
                                let _ = chat_tx.send(msg);
                            }
                            Err(e) => {
                                tracing::warn!("[ServerRouter] Failed to parse chat message: {}", e);
                            }
                        }
                    }

                    _ => {
                        // Other server messages - ignore
                        tracing::trace!("[ServerRouter] Ignoring server message code {}", code);
                    }
                }
            }

            tracing::error!("[ServerRouter] Reader task ended - server connection lost");
        });
    }

    /// Parse a ConnectToPeer message (code 18)
    fn parse_connect_to_peer(data: &[u8]) -> Result<ConnectToPeerMsg> {
        let mut buf = BytesMut::from(data);
        let username = decode_string(&mut buf)?;
        let conn_type = decode_string(&mut buf)?;
        let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
        let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
        let port = buf.get_u32_le();
        let token = buf.get_u32_le();

        Ok(ConnectToPeerMsg {
            username,
            conn_type,
            ip,
            port,
            token,
        })
    }

    /// Parse a GetPeerAddress response (code 3)
    fn parse_peer_address(data: &[u8]) -> Result<PeerAddressResponse> {
        let mut buf = BytesMut::from(data);
        let username = decode_string(&mut buf)?;

        if buf.remaining() < 8 {
            anyhow::bail!("Invalid PeerAddress response - not enough bytes");
        }
        let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
        let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
        let port = buf.get_u32_le();

        Ok(PeerAddressResponse {
            username,
            ip,
            port,
        })
    }

    /// Send a raw message to the server.
    ///
    /// Acquires the writer mutex briefly, allowing concurrent callers
    /// to interleave messages without blocking each other for long.
    pub async fn send_message(&self, code: u32, data: &[u8]) -> Result<()> {
        let mut writer = self.writer.lock().await;
        let msg_len = 4 + data.len();
        writer.write_u32_le(msg_len as u32).await?;
        writer.write_u32_le(code).await?;
        writer.write_all(data).await?;
        writer.flush().await?;
        Ok(())
    }

    /// Request a peer's address from the server and wait for the response.
    ///
    /// Sends GetPeerAddress (code 3) and returns a oneshot receiver
    /// that will resolve when the server responds.
    pub async fn get_peer_address(&self, username: &str) -> Result<oneshot::Receiver<PeerAddressResponse>> {
        let (tx, rx) = oneshot::channel();

        // Register the waiter
        let key = username.to_lowercase();
        {
            let mut subs = self.peer_address_subs.lock().await;
            subs.entry(key).or_insert_with(Vec::new).push(tx);
        }

        // Send GetPeerAddress request (code 3)
        let mut data = BytesMut::new();
        encode_string(&mut data, username);
        self.send_message(3, &data).await?;

        Ok(rx)
    }

    /// Send CantConnectToPeer message to the server
    pub async fn send_cant_connect_to_peer(&self, token: u32, username: &str) -> Result<()> {
        let mut data = BytesMut::new();
        data.put_u32_le(token);
        encode_string(&mut data, username);
        self.send_message(1001, &data).await?;
        tracing::debug!("[ServerRouter] Sent CantConnectToPeer for '{}' (token={})", username, token);
        Ok(())
    }

    /// Get our username
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Parse an incoming private message (server code 22)
    fn parse_chat_message(data: &[u8]) -> Result<ChatMessage> {
        let mut buf = BytesMut::from(data);

        if buf.remaining() < 8 {
            anyhow::bail!("Chat message too short");
        }

        let message_id = buf.get_u32_le();
        let timestamp = buf.get_u32_le();
        let username = decode_string(&mut buf)?;
        let message = decode_string(&mut buf)?;
        let is_new = if buf.remaining() >= 1 { buf.get_u8() != 0 } else { true };

        Ok(ChatMessage {
            message_id: Some(message_id),
            timestamp,
            username,
            message,
            incoming: true,
            is_new,
        })
    }

    /// Send a private message to another user via the server (code 22)
    pub async fn send_private_message(&self, username: &str, message: &str) -> Result<()> {
        let mut data = BytesMut::new();
        encode_string(&mut data, username);
        encode_string(&mut data, message);
        self.send_message(22, &data).await?;

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as u32;

        let chat_msg = ChatMessage {
            message_id: None,
            timestamp: now,
            username: username.to_string(),
            message: message.to_string(),
            incoming: false,
            is_new: true,
        };

        tracing::info!("[Chat] Sent to '{}': {}", username, message);

        // Store in history
        {
            let mut history = self.chat_history.lock().await;
            history.push(chat_msg.clone());
            if history.len() > 500 {
                let excess = history.len() - 500;
                history.drain(..excess);
            }
        }

        // Broadcast to WebSocket subscribers
        let _ = self.chat_tx.send(chat_msg);

        Ok(())
    }

    /// Subscribe to incoming chat messages
    pub fn subscribe_chat(&self) -> broadcast::Receiver<ChatMessage> {
        self.chat_tx.subscribe()
    }

    /// Get recent chat history
    pub async fn get_chat_history(&self) -> Vec<ChatMessage> {
        let history = self.chat_history.lock().await;
        history.clone()
    }
}
