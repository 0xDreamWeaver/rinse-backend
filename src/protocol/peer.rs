//! Peer protocol helpers for Soulseek P2P connections
//!
//! This module provides low-level peer protocol operations including:
//! - PeerInit and PierceFirewall message handling
//! - File transfer connection management
//! - Both direct and indirect file transfer paths

use anyhow::{Result, bail, Context};
use bytes::{Buf, BufMut, BytesMut};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::messages::decode_string;

// ============================================================================
// Low-level protocol helpers
// ============================================================================

/// Encode a string (length-prefixed) into a BytesMut buffer
pub fn encode_string(buf: &mut BytesMut, s: &str) {
    buf.put_u32_le(s.len() as u32);
    buf.put_slice(s.as_bytes());
}

/// Send a raw message to a stream with length prefix and code
pub async fn send_message(stream: &mut TcpStream, code: u32, data: &[u8]) -> std::io::Result<()> {
    let msg_len = 4 + data.len();
    stream.write_u32_le(msg_len as u32).await?;
    stream.write_u32_le(code).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}

/// Read a raw message from a stream, returns (code, data)
pub async fn read_message(stream: &mut TcpStream) -> std::io::Result<(u32, Vec<u8>)> {
    let msg_len = stream.read_u32_le().await? as usize;
    if msg_len < 4 {
        return Ok((0, vec![]));
    }

    let code = stream.read_u32_le().await?;
    let data_len = msg_len - 4;
    let mut data = vec![0u8; data_len];
    if data_len > 0 {
        stream.read_exact(&mut data).await?;
    }

    Ok((code, data))
}

// ============================================================================
// Peer init message helpers
// ============================================================================

/// Send PeerInit message to a peer
///
/// Format: [length:u32][type=1:u8][username:string][conn_type:string][token:u32]
pub async fn send_peer_init(
    stream: &mut TcpStream,
    username: &str,
    conn_type: &str,
    _token: u32,  // Ignored - protocol spec says token is always 0
) -> std::io::Result<()> {
    let msg_len = 1 + 4 + username.len() + 4 + conn_type.len() + 4;
    stream.write_u32_le(msg_len as u32).await?;
    stream.write_u8(1).await?; // PeerInit type
    stream.write_u32_le(username.len() as u32).await?;
    stream.write_all(username.as_bytes()).await?;
    stream.write_u32_le(conn_type.len() as u32).await?;
    stream.write_all(conn_type.as_bytes()).await?;
    // Token is always 0 per protocol spec (Nicotine+ does the same)
    // "The token is always zero and ignored today"
    stream.write_u32_le(0).await?;
    stream.flush().await?;
    Ok(())
}

/// Send PierceFirewall message to a peer
///
/// Format: [length=5:u32][type=0:u8][token:u32]
pub async fn send_pierce_firewall(stream: &mut TcpStream, token: u32) -> std::io::Result<()> {
    stream.write_u32_le(5).await?; // length = 5
    stream.write_u8(0).await?; // type = 0 (PierceFirewall)
    stream.write_u32_le(token).await?;
    stream.flush().await?;
    Ok(())
}

/// Send CantConnectToPeer to server when we fail to connect to a peer
///
/// This tells the server that the indirect connection attempt failed,
/// allowing it to clean up and potentially try alternative routing.
///
/// Format: [length:u32][code=1001:u32][token:u32][username:string]
pub async fn send_cant_connect_to_peer(
    server_stream: &mut TcpStream,
    token: u32,
    username: &str,
) -> std::io::Result<()> {
    let mut data = BytesMut::new();
    data.put_u32_le(token);
    encode_string(&mut data, username);

    let msg_len = 4 + data.len();  // code (4 bytes) + data
    server_stream.write_u32_le(msg_len as u32).await?;
    server_stream.write_u32_le(1001).await?;  // CantConnectToPeer code
    server_stream.write_all(&data).await?;
    server_stream.flush().await?;

    tracing::debug!("[CantConnectToPeer] Sent for '{}' (token={})", username, token);
    Ok(())
}

/// Read PeerInit from stream
///
/// Returns (username, conn_type, token)
pub async fn read_peer_init(stream: &mut TcpStream) -> Result<(String, String, u32)> {
    let msg_len = stream.read_u32_le().await?;
    let msg_type = stream.read_u8().await?;

    if msg_type != 1 {
        bail!("Expected PeerInit (type 1), got type {}", msg_type);
    }

    let remaining = (msg_len - 1) as usize;
    let mut data = vec![0u8; remaining];
    stream.read_exact(&mut data).await?;

    let mut buf = BytesMut::from(&data[..]);
    let username = decode_string(&mut buf)?;
    let conn_type = decode_string(&mut buf)?;
    let token = buf.get_u32_le();

    Ok((username, conn_type, token))
}

// ============================================================================
// File transfer connection
// ============================================================================

/// Represents an incoming file transfer connection ready for download
pub struct FileConnection {
    pub stream: TcpStream,
    pub transfer_token: u32,
}

/// Result of handling an incoming peer connection - can be either F (file) or P (peer/search)
pub enum IncomingConnection {
    /// F connection for file transfer
    File(FileConnection),
    /// P connection for search results (returns stream, username, token)
    Peer(TcpStream, String, u32),
}

/// Result of download negotiation with a peer
#[derive(Debug, Clone)]
pub enum NegotiationResult {
    /// Transfer allowed - ready for F connection
    Allowed {
        filesize: u64,
        transfer_token: u32,
    },
    /// Transfer queued - peer will initiate F connection when ready
    Queued {
        transfer_token: u32,
        position: Option<u32>,
        reason: String,
    },
    /// Transfer denied
    Denied {
        reason: String,
    },
}

/// Handle an incoming peer connection (either F for file or P for search results)
///
/// This handles the handshake for both types of connections:
/// - F connections: File transfers where uploader connects to us
/// - P connections: Peers connecting to deliver search results
pub async fn handle_incoming_connection(
    mut stream: TcpStream,
    our_username: &str,
) -> Result<IncomingConnection> {
    // Read first 4 bytes to determine if it's PeerInit (length-prefixed) or PierceFirewall
    let first_u32 = stream.read_u32_le().await?;
    tracing::info!("[CONN] Incoming handshake: first_u32={} (0x{:08x})", first_u32, first_u32);

    if first_u32 == 5 {
        // PierceFirewall: [length=5][type=0][token]
        let _msg_type = stream.read_u8().await?;
        let token = stream.read_u32_le().await?;
        tracing::info!("[CONN] Received PierceFirewall, token={}", token);

        // Respond with PeerInit type F (assume file transfer for PierceFirewall)
        send_peer_init(&mut stream, our_username, "F", 0).await?;

        // Now wait for FileTransferInit (4 bytes, raw token, NO length prefix)
        let transfer_token = stream.read_u32_le().await?;
        tracing::debug!("[CONN] Received FileTransferInit, transfer_token={}", transfer_token);

        Ok(IncomingConnection::File(FileConnection { stream, transfer_token }))

    } else if first_u32 > 5 && first_u32 < 200 {
        // Likely PeerInit
        let msg_type = stream.read_u8().await?;

        if msg_type == 1 {
            // PeerInit: read remaining bytes
            let remaining = (first_u32 - 1) as usize;
            let mut data = vec![0u8; remaining];
            stream.read_exact(&mut data).await?;

            let mut buf = BytesMut::from(&data[..]);
            let peer_username = decode_string(&mut buf)?;
            let conn_type = decode_string(&mut buf)?;
            let init_token = buf.get_u32_le();

            tracing::info!("[CONN] Received PeerInit from '{}', type='{}', token={}", peer_username, conn_type, init_token);

            if conn_type == "F" {
                // File transfer connection - uploader is connecting to us
                // IMPORTANT: Do NOT send PeerInit response! The uploader expects us to just
                // wait for their FileTransferInit, then respond with FileOffset.
                // See: https://nicotine-plus.org/doc/SLSKPROTOCOL.html
                tracing::debug!("[CONN] F connection from '{}' - waiting for FileTransferInit", peer_username);

                // Wait for FileTransferInit (4 bytes, raw token, NO length prefix)
                let transfer_token = stream.read_u32_le().await?;
                tracing::debug!("[CONN] Received FileTransferInit, transfer_token={}", transfer_token);

                Ok(IncomingConnection::File(FileConnection { stream, transfer_token }))
            } else if conn_type == "P" {
                // Peer connection for search results
                send_peer_init(&mut stream, our_username, "P", init_token).await?;
                tracing::debug!("[CONN] Sent PeerInit response for P connection");

                Ok(IncomingConnection::Peer(stream, peer_username, init_token))
            } else {
                bail!("Unknown connection type '{}'", conn_type);
            }
        } else {
            bail!("Expected PeerInit (type 1), got type {}", msg_type);
        }
    } else {
        // Might be raw FileTransferInit (4-byte token) if handshake already done
        tracing::info!("[CONN] Received raw token (assumed FileTransferInit): {}", first_u32);
        Ok(IncomingConnection::File(FileConnection { stream, transfer_token: first_u32 }))
    }
}

/// Handle an incoming F connection (direct path - uploader connected to us)
///
/// This handles the handshake for file transfer connections where the
/// uploader connects to our listening port.
pub async fn handle_incoming_f_connection(
    stream: TcpStream,
    our_username: &str,
) -> Result<FileConnection> {
    match handle_incoming_connection(stream, our_username).await? {
        IncomingConnection::File(conn) => Ok(conn),
        IncomingConnection::Peer(_, username, _) => {
            bail!("Expected F connection, got P connection from '{}'", username);
        }
    }
}

/// Connect to uploader for indirect file transfer (Path B)
///
/// This is used when we receive ConnectToPeer type="F" from the server,
/// indicating we should connect to the uploader to receive a file.
pub async fn connect_for_indirect_transfer(
    ip: &str,
    port: u32,
    indirect_token: u32,
    our_username: &str,
) -> Result<FileConnection> {
    let addr = format!("{}:{}", ip, port);
    tracing::debug!("[INDIRECT] Connecting to {} for file transfer...", addr);

    let mut stream = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(&addr)
    ).await??;

    // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
    stream.set_nodelay(true)?;

    tracing::debug!("[INDIRECT] Connected, sending PierceFirewall with token {}", indirect_token);

    // Send PierceFirewall with the indirect token
    send_pierce_firewall(&mut stream, indirect_token).await?;

    // Wait for peer's PeerInit response (or they might send FileTransferInit directly)
    let first_u32 = tokio::time::timeout(
        Duration::from_secs(10),
        stream.read_u32_le()
    ).await??;

    if first_u32 > 5 && first_u32 < 200 {
        // Likely PeerInit response
        let msg_type = stream.read_u8().await?;
        if msg_type == 1 {
            let remaining = (first_u32 - 1) as usize;
            let mut data = vec![0u8; remaining];
            stream.read_exact(&mut data).await?;
            tracing::debug!("[INDIRECT] Received PeerInit response");
        }

        // Now wait for FileTransferInit
        let transfer_token = stream.read_u32_le().await?;
        tracing::debug!("[INDIRECT] Received FileTransferInit, transfer_token={}", transfer_token);

        Ok(FileConnection { stream, transfer_token })
    } else {
        // Assume it's the transfer token directly
        tracing::debug!("[INDIRECT] Received FileTransferInit, transfer_token={}", first_u32);
        Ok(FileConnection { stream, transfer_token: first_u32 })
    }
}

// ============================================================================
// Search result handling
// ============================================================================

use super::messages::{SearchFile, FileAttribute};
use flate2::read::ZlibDecoder;
use std::io::Read;

/// Parsed search response with user stats
#[derive(Debug, Clone)]
pub struct ParsedSearchResponse {
    pub username: String,
    pub token: u32,
    pub files: Vec<(String, u64, Option<u32>)>, // (filename, size, bitrate)
    pub slot_free: bool,
    pub avg_speed: u32,  // bytes per second
    pub queue_length: u32,
}

/// Parse a decompressed FileSearchResponse
pub fn parse_file_search_response(data: &[u8]) -> Result<ParsedSearchResponse> {
    let mut buf = BytesMut::from(data);

    if buf.remaining() < 8 {
        bail!("Not enough data");
    }

    let username = decode_string(&mut buf)?;
    let token = buf.get_u32_le();
    let file_count = buf.get_u32_le();

    tracing::debug!("[PARSE] FileSearchResponse: user='{}', token={}, files={}", username, token, file_count);

    let mut files = Vec::new();
    for _i in 0..file_count.min(100) {
        if buf.remaining() < 1 {
            break;
        }

        let _code = buf.get_u8();
        let filename = decode_string(&mut buf)?;

        if buf.remaining() < 8 {
            break;
        }
        let size = buf.get_u64_le();

        let _extension = decode_string(&mut buf)?;

        if buf.remaining() < 4 {
            break;
        }
        let attr_count = buf.get_u32_le();

        let mut bitrate = None;
        for _ in 0..attr_count {
            if buf.remaining() < 8 {
                break;
            }
            let attr_type = buf.get_u32_le();
            let attr_value = buf.get_u32_le();
            if attr_type == 0 {
                bitrate = Some(attr_value);
            }
        }

        files.push((filename, size, bitrate));
    }

    // Parse user stats that come after file list
    // Format: slotfree (u8/bool), avgspeed (u32), queue_length (u32)
    let slot_free = if buf.remaining() >= 1 {
        buf.get_u8() != 0
    } else {
        true  // Default to free if not provided
    };

    let avg_speed = if buf.remaining() >= 4 {
        buf.get_u32_le()
    } else {
        0
    };

    let queue_length = if buf.remaining() >= 4 {
        buf.get_u32_le()
    } else {
        0
    };

    let speed_kbps = avg_speed as f64 / 1024.0;
    tracing::debug!("[PARSE] User stats: free={}, speed={:.1} KB/s, queue={}",
        slot_free, speed_kbps, queue_length);

    Ok(ParsedSearchResponse {
        username,
        token,
        files,
        slot_free,
        avg_speed,
        queue_length,
    })
}

/// Receive search results from an incoming P connection
///
/// Used after accepting a P connection via handle_incoming_connection
pub async fn receive_search_results_from_peer(
    stream: &mut TcpStream,
) -> Result<ParsedSearchResponse> {
    // Wait for FileSearchResponse (code 9)
    for _ in 0..10 {
        let msg_len = match tokio::time::timeout(Duration::from_secs(5), stream.read_u32_le()).await {
            Ok(Ok(len)) => len,
            Ok(Err(e)) => {
                tracing::debug!("[P-CONN] Read error: {}", e);
                bail!("Read error: {}", e);
            }
            Err(_) => {
                tracing::debug!("[P-CONN] Timeout waiting for message");
                bail!("Timeout waiting for search results");
            }
        };

        if msg_len == 0 || msg_len > 10_000_000 {
            continue;
        }

        let code = stream.read_u32_le().await?;
        let data_len = (msg_len - 4) as usize;
        let mut data = vec![0u8; data_len];
        stream.read_exact(&mut data).await?;

        if code == 9 {
            // FileSearchResponse - decompress and parse
            let decompressed = match ZlibDecoder::new(&data[..]).bytes().collect::<Result<Vec<_>, _>>() {
                Ok(d) => d,
                Err(_) => data, // Not compressed or already decompressed
            };

            return parse_file_search_response(&decompressed);
        }
    }

    bail!("No FileSearchResponse received from peer")
}

/// Connect to a peer via PierceFirewall and return the raw stream.
///
/// This is the general-purpose outbound peer connection function used when
/// responding to a ConnectToPeer "P" message from the server. After connecting,
/// the caller should read the first peer message and dispatch appropriately
/// (search results, GetSharedFileList, QueueUpload, etc.).
pub async fn connect_to_peer_for_dispatch(
    ip: &str,
    port: u32,
    peer_username: &str,
    peer_token: u32,
) -> Result<TcpStream> {
    let addr = format!("{}:{}", ip, port);
    tracing::debug!("[ConnectToPeer] Connecting to '{}' at {} (token={})", peer_username, addr, peer_token);

    let mut peer_stream = match tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&addr)
    ).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => bail!("TCP connect to '{}' failed: {}", peer_username, e),
        Err(_) => bail!("TCP connect to '{}' timed out (peer unreachable)", peer_username),
    };

    peer_stream.set_nodelay(true)?;

    // Send PierceFirewall to identify ourselves
    if let Err(e) = send_pierce_firewall(&mut peer_stream, peer_token).await {
        bail!("Failed to send PierceFirewall to '{}': {}", peer_username, e);
    }

    tracing::debug!("[ConnectToPeer] Connected to '{}' at {}, PierceFirewall sent (token={})", peer_username, addr, peer_token);
    Ok(peer_stream)
}

/// Connect to peer for search results
///
/// Returns the TCP stream (for reuse) and the parsed search response
pub async fn connect_to_peer_for_results(
    ip: &str,
    port: u32,
    peer_username: &str,
    _our_username: &str,
    peer_token: u32,
) -> Result<(TcpStream, ParsedSearchResponse)> {
    let addr = format!("{}:{}", ip, port);
    tracing::debug!("[ConnectToPeer] START: '{}' at {} (token={})", peer_username, addr, peer_token);

    // Step 1: TCP connection
    let mut peer_stream = match tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&addr)
    ).await {
        Ok(Ok(stream)) => {
            tracing::debug!("[ConnectToPeer] TCP connected to '{}' at {}", peer_username, addr);
            stream
        }
        Ok(Err(e)) => {
            tracing::debug!("[ConnectToPeer] TCP connect FAILED to '{}': {}", peer_username, e);
            bail!("TCP connect to '{}' failed: {}", peer_username, e);
        }
        Err(_) => {
            tracing::debug!("[ConnectToPeer] TCP connect TIMEOUT to '{}'", peer_username);
            bail!("TCP connect to '{}' timed out (peer unreachable)", peer_username);
        }
    };

    // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
    peer_stream.set_nodelay(true)?;

    // Step 2: Send PierceFirewall
    tracing::debug!("[ConnectToPeer] Sending PierceFirewall to '{}' (token={})", peer_username, peer_token);
    if let Err(e) = send_pierce_firewall(&mut peer_stream, peer_token).await {
        tracing::debug!("[ConnectToPeer] PierceFirewall FAILED to '{}': {}", peer_username, e);
        bail!("Failed to send PierceFirewall to '{}': {}", peer_username, e);
    }
    tracing::debug!("[ConnectToPeer] PierceFirewall sent to '{}' (token={})", peer_username, peer_token);

    // Step 3: Wait for FileSearchResponse
    // Per Nicotine+ protocol: after PierceFirewall, peer sends FileSearchResponse directly
    // (The peer already knows who we are from the token match)
    tracing::debug!("[ConnectToPeer] Waiting for response from '{}'...", peer_username);
    let msg_len = match tokio::time::timeout(Duration::from_secs(5), peer_stream.read_u32_le()).await {
        Ok(Ok(v)) => {
            tracing::debug!("[ConnectToPeer] Got first u32 from '{}': {} (0x{:08x})", peer_username, v, v);
            v
        }
        Ok(Err(e)) => {
            tracing::debug!("[ConnectToPeer] Read error from '{}': {}", peer_username, e);
            bail!("'{}' handshake read error: {}", peer_username, e);
        }
        Err(_) => {
            tracing::debug!("[ConnectToPeer] Timeout waiting for response from '{}'", peer_username);
            bail!("'{}' handshake timeout (connected but no response)", peer_username);
        }
    };

    // Handle response based on message length
    // FileSearchResponse: length > 100, code = 9, zlib compressed
    // Note: Some peers may send unexpected responses (PeerInit, PierceFirewall)
    // which we consume and retry
    if msg_len > 100 && msg_len < 10_000_000 {
        // Expected: FileSearchResponse
        let code = peer_stream.read_u32_le().await?;
        tracing::debug!("[ConnectToPeer] '{}': msg_len={}, code={}", peer_username, msg_len, code);
        if code == 9 {
            tracing::debug!("[ConnectToPeer] '{}': Got FileSearchResponse (code=9), reading {} bytes", peer_username, msg_len - 4);
            let data_len = (msg_len - 4) as usize;
            let mut compressed = vec![0u8; data_len];
            peer_stream.read_exact(&mut compressed).await?;

            let decompressed = match ZlibDecoder::new(&compressed[..]).bytes().collect::<Result<Vec<_>, _>>() {
                Ok(d) => d,
                Err(_) => compressed,
            };

            let response = parse_file_search_response(&decompressed)?;
            tracing::debug!("[ConnectToPeer] '{}': Parsed {} files from search response", peer_username, response.files.len());
            return Ok((peer_stream, response));
        } else {
            // Unknown code, skip this message and continue to wait
            tracing::debug!("[ConnectToPeer] '{}': Unexpected code {} (not 9), skipping {} bytes", peer_username, code, msg_len - 4);
            let remaining = (msg_len - 4) as usize;
            let mut skip_data = vec![0u8; remaining];
            peer_stream.read_exact(&mut skip_data).await?;
        }
    } else if msg_len == 5 {
        // Unexpected: PierceFirewall response (shouldn't happen per Nicotine+ protocol)
        tracing::debug!("[ConnectToPeer] '{}': Unexpected PierceFirewall (len=5), consuming", peer_username);
        let msg_type = peer_stream.read_u8().await?;
        let token = peer_stream.read_u32_le().await?;
        tracing::debug!("[ConnectToPeer] '{}': PierceFirewall type={}, token={}", peer_username, msg_type, token);
    } else if msg_len > 5 && msg_len < 200 {
        // Unexpected: PeerInit response (shouldn't happen per Nicotine+ protocol)
        tracing::debug!("[ConnectToPeer] '{}': Unexpected PeerInit (len={}), consuming", peer_username, msg_len);
        let msg_type = peer_stream.read_u8().await?;
        let remaining = (msg_len - 1) as usize;
        let mut data = vec![0u8; remaining];
        peer_stream.read_exact(&mut data).await?;
        tracing::debug!("[ConnectToPeer] '{}': PeerInit type={}, consumed {} bytes", peer_username, msg_type, remaining);
    } else {
        tracing::debug!("[ConnectToPeer] '{}': Unexpected message length {} (not PierceFirewall, PeerInit, or FileSearchResponse)", peer_username, msg_len);
    }

    // Wait for FileSearchResponse (retry loop if first message wasn't it)
    tracing::debug!("[ConnectToPeer] '{}': Entering retry loop for FileSearchResponse...", peer_username);
    for attempt in 0..10 {
        tracing::debug!("[ConnectToPeer] '{}': Retry attempt {}/10", peer_username, attempt + 1);
        let msg_len = match tokio::time::timeout(Duration::from_secs(3), peer_stream.read_u32_le()).await {
            Ok(Ok(len)) => {
                tracing::debug!("[ConnectToPeer] '{}': Retry got msg_len={}", peer_username, len);
                len
            }
            Ok(Err(e)) => {
                tracing::debug!("[ConnectToPeer] '{}': Retry read error: {}", peer_username, e);
                bail!("'{}' read error waiting for results: {}", peer_username, e);
            }
            Err(_) => {
                tracing::debug!("[ConnectToPeer] '{}': Retry timeout (attempt {})", peer_username, attempt + 1);
                if attempt == 9 {
                    bail!("'{}' results timeout (connected, handshook, but no search results after 30s)", peer_username);
                }
                continue;
            }
        };

        if msg_len == 0 || msg_len > 10_000_000 {
            tracing::debug!("[ConnectToPeer] '{}': Skipping invalid msg_len={}", peer_username, msg_len);
            continue;
        }

        let code = peer_stream.read_u32_le().await?;
        let data_len = (msg_len - 4) as usize;
        let mut data = vec![0u8; data_len];
        peer_stream.read_exact(&mut data).await?;
        tracing::debug!("[ConnectToPeer] '{}': Retry got code={}, data_len={}", peer_username, code, data_len);

        if code == 9 {
            let decompressed = match ZlibDecoder::new(&data[..]).bytes().collect::<Result<Vec<_>, _>>() {
                Ok(d) => d,
                Err(_) => data,
            };

            let response = parse_file_search_response(&decompressed)?;
            tracing::debug!("[ConnectToPeer] '{}': SUCCESS - Got {} files from retry loop", peer_username, response.files.len());
            return Ok((peer_stream, response));
        } else {
            tracing::debug!("[ConnectToPeer] '{}': Retry got code {} (not 9), continuing...", peer_username, code);
        }
    }

    tracing::debug!("[ConnectToPeer] '{}': FAILED - No FileSearchResponse after 10 attempts", peer_username);
    bail!("'{}' sent no FileSearchResponse after 10 messages", peer_username)
}

// ============================================================================
// Transfer negotiation
// ============================================================================

/// Queue upload request - MODERN METHOD (recommended)
///
/// This is the modern approach used by Nicotine+ 3.0.3+ and the official clients.
/// The flow is:
/// 1. We send QueueUpload (code 43) - "please queue this file for upload to me"
/// 2. Peer queues us and eventually sends TransferRequest (direction=1) - "I'm ready"
/// 3. We reply with TransferReply (allowed=true)
/// 4. Peer initiates F connection
///
/// This is more reliable than the legacy TransferRequest(direction=0) method.
pub async fn queue_upload(
    peer_stream: &mut TcpStream,
    filename: &str,
) -> Result<()> {
    // Send QueueUpload (code 43)
    // Format: [length][code=43][filename]
    let mut request_data = BytesMut::new();
    encode_string(&mut request_data, filename);

    let msg_len = 4 + request_data.len();
    peer_stream.write_u32_le(msg_len as u32).await?;
    peer_stream.write_u32_le(43).await?;
    peer_stream.write_all(&request_data).await?;
    peer_stream.flush().await?;

    tracing::info!("[QUEUE] Sent QueueUpload for '{}'", filename);
    Ok(())
}

/// Wait for TransferRequest from peer (after sending QueueUpload)
///
/// The peer sends TransferRequest (direction=1) when they're ready to upload.
/// We respond with TransferReply (allowed=true).
///
/// Returns NegotiationResult with the transfer token and filesize.
pub async fn wait_for_transfer_request(
    peer_stream: &mut TcpStream,
    expected_filename: &str,
    timeout_secs: u64,
) -> Result<NegotiationResult> {
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(timeout_secs);

    while start.elapsed() < timeout {
        // Read next message with timeout
        let remaining = timeout.saturating_sub(start.elapsed());
        let msg_len = match tokio::time::timeout(remaining, peer_stream.read_u32_le()).await {
            Ok(Ok(len)) => len,
            Ok(Err(e)) => bail!("Read error: {}", e),
            Err(_) => bail!("Timeout waiting for TransferRequest from peer"),
        };

        let code = peer_stream.read_u32_le().await?;
        let data_len = (msg_len - 4) as usize;
        let mut data = vec![0u8; data_len];
        peer_stream.read_exact(&mut data).await?;

        if code == 40 {
            // TransferRequest from peer
            let mut buf = BytesMut::from(&data[..]);
            let direction = buf.get_u32_le();
            let transfer_token = buf.get_u32_le();
            let filename = decode_string(&mut buf)?;

            tracing::debug!("[TRANSFER] Received TransferRequest: dir={}, token={}, file='{}'",
                direction, transfer_token, filename);

            if direction != 1 {
                tracing::warn!("[TRANSFER] Unexpected direction {} (expected 1 for upload)", direction);
                continue;
            }

            // Read filesize (only present for direction=1)
            let filesize = buf.get_u64_le();

            tracing::info!("[TRANSFER] Peer ready to upload: {} ({:.2} MB)",
                filename, filesize as f64 / 1_048_576.0);

            // Send TransferReply (code 41) - allowed
            let mut reply_data = BytesMut::new();
            reply_data.put_u32_le(transfer_token);
            reply_data.put_u8(1); // allowed = true

            let reply_len = 4 + reply_data.len();
            peer_stream.write_u32_le(reply_len as u32).await?;
            peer_stream.write_u32_le(41).await?;
            peer_stream.write_all(&reply_data).await?;
            peer_stream.flush().await?;

            return Ok(NegotiationResult::Allowed { filesize, transfer_token });
        } else if code == 44 {
            // PlaceInQueueReply - tells us our position in queue
            let mut buf = BytesMut::from(&data[..]);
            let queued_file = decode_string(&mut buf)?;
            let position = buf.get_u32_le();
            tracing::info!("[TRANSFER] Queue position for '{}': {}", queued_file, position);
            // Continue waiting for TransferRequest
        } else if code == 50 {
            // UploadDenied
            let mut buf = BytesMut::from(&data[..]);
            let denied_file = decode_string(&mut buf)?;
            let reason = decode_string(&mut buf)?;
            tracing::warn!("[TRANSFER] Upload denied for '{}': {}", denied_file, reason);
            return Ok(NegotiationResult::Denied { reason });
        } else if code == 46 {
            // UploadFailed
            let mut buf = BytesMut::from(&data[..]);
            let failed_file = decode_string(&mut buf)?;
            tracing::warn!("[TRANSFER] Upload failed for '{}'", failed_file);
            return Ok(NegotiationResult::Denied { reason: "Upload failed".to_string() });
        } else {
            tracing::debug!("[TRANSFER] Ignoring message code {} while waiting for TransferRequest", code);
        }
    }

    bail!("Timeout waiting for TransferRequest from peer")
}

/// Negotiate a download with a peer (send TransferRequest, receive TransferReply)
///
/// NOTE: This is the LEGACY method using TransferRequest(direction=0).
/// Modern clients (Nicotine+ 3.0.3+, official client) prefer the QueueUpload method.
/// Consider using queue_upload() + wait_for_transfer_request() instead.
///
/// Returns NegotiationResult indicating allowed, queued, or denied
pub async fn negotiate_download(
    peer_stream: &mut TcpStream,
    filename: &str,
) -> Result<NegotiationResult> {
    // Generate transfer token
    let transfer_token: u32 = rand::random();

    // Send TransferRequest (code 40)
    // Format: [length][code=40][direction=0][token][filename]
    let mut request_data = BytesMut::new();
    request_data.put_u32_le(0); // direction = 0 (download request - legacy method)
    request_data.put_u32_le(transfer_token);
    encode_string(&mut request_data, filename);

    let msg_len = 4 + request_data.len();
    peer_stream.write_u32_le(msg_len as u32).await?;
    peer_stream.write_u32_le(40).await?;
    peer_stream.write_all(&request_data).await?;
    peer_stream.flush().await?;

    tracing::debug!("[NEGOTIATE] Sent TransferRequest (token={}) for '{}'", transfer_token, filename);

    // Wait for TransferReply (code 41) or other response
    let msg_len = tokio::time::timeout(Duration::from_secs(30), peer_stream.read_u32_le()).await??;
    let code = peer_stream.read_u32_le().await?;

    if code != 41 {
        // Read remaining data
        let data_len = (msg_len - 4) as usize;
        let mut data = vec![0u8; data_len];
        peer_stream.read_exact(&mut data).await?;

        if code == 44 {
            // PlaceInQueueResponse - file is queued
            let mut buf = BytesMut::from(&data[..]);
            let _queued_file = decode_string(&mut buf)?;
            let place = buf.get_u32_le();
            tracing::info!("[NEGOTIATE] Queued at position {}", place);
            return Ok(NegotiationResult::Queued {
                transfer_token,
                position: Some(place),
                reason: format!("Position {}", place),
            });
        } else if code == 50 {
            // UploadDenied
            let mut buf = BytesMut::from(&data[..]);
            let _denied_file = decode_string(&mut buf)?;
            let reason = decode_string(&mut buf)?;
            tracing::info!("[NEGOTIATE] Denied: {}", reason);
            return Ok(NegotiationResult::Denied { reason });
        } else {
            bail!("Unexpected response code: {}", code);
        }
    }

    // Parse TransferReply
    let reply_token = peer_stream.read_u32_le().await?;
    let allowed = peer_stream.read_u8().await?;

    tracing::debug!("[NEGOTIATE] TransferReply: token={}, allowed={}", reply_token, allowed);

    if allowed == 1 {
        // Transfer allowed - read filesize
        let filesize = peer_stream.read_u64_le().await?;
        tracing::info!("[NEGOTIATE] Transfer allowed! Size: {} bytes ({:.2} MB)", filesize, filesize as f64 / 1_048_576.0);
        Ok(NegotiationResult::Allowed { filesize, transfer_token })
    } else {
        // Transfer denied - read reason
        let remaining = (msg_len - 4 - 4 - 1) as usize;
        let mut reason_data = vec![0u8; remaining];
        peer_stream.read_exact(&mut reason_data).await?;
        let mut buf = BytesMut::from(&reason_data[..]);
        let reason = decode_string(&mut buf)?;

        // Check if this is a "Queued" response (common denial reason)
        if reason.to_lowercase().contains("queued") || reason.to_lowercase().contains("queue") {
            tracing::info!("[NEGOTIATE] Queued: {}", reason);
            Ok(NegotiationResult::Queued {
                transfer_token,
                position: None,
                reason,
            })
        } else {
            tracing::info!("[NEGOTIATE] Denied: {}", reason);
            Ok(NegotiationResult::Denied { reason })
        }
    }
}

// ============================================================================
// File download
// ============================================================================

/// Download a file from a peer
///
/// This function implements both file transfer paths:
/// - Path A (Direct): Uploader connects to our listener
/// - Path B (Indirect): We receive ConnectToPeer type="F" and connect to uploader
///
/// Parameters:
/// - server_stream: Connection to the Soulseek server for receiving ConnectToPeer messages
/// - peer_ip: IP address of the peer
/// - peer_port: Port of the peer
/// - peer_username: Username of the peer
/// - our_username: Our username
/// - filename: Full path of the file to download
/// - filesize: Expected size of the file
/// - transfer_token: Token from TransferReply
/// - file_conn_rx: Channel receiver for incoming F connections from our listener
/// - output_path: Path to save the downloaded file

/// Progress callback type for download progress reporting
pub type ProgressCallback = Box<dyn Fn(u64, u64, f64) + Send + Sync>;

/// Arc-wrapped progress callback for use in spawned tasks
pub type ArcProgressCallback = std::sync::Arc<dyn Fn(u64, u64, f64) + Send + Sync>;

/// Receive file data from an established F connection.
/// This function can be spawned as an independent task since it only needs the F connection stream.
///
/// # Arguments
/// - file_conn: The established F connection
/// - filesize: Expected file size
/// - output_path: Path to save the file
/// - progress_callback: Optional callback for progress updates
///
/// # Returns
/// The number of bytes received
pub async fn receive_file_data(
    mut file_conn: FileConnection,
    filesize: u64,
    output_path: std::path::PathBuf,
    progress_callback: Option<ArcProgressCallback>,
    peer_username: &str,
) -> Result<u64> {
    let size_mb = filesize as f64 / 1_048_576.0;
    let basename = output_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    tracing::info!("[DOWNLOAD] Starting receive: {:.2} MB -> \"{}\" from '{}'", size_mb, basename, peer_username);

    // Send FileOffset (8 bytes, NO length prefix)
    file_conn.stream.write_u64_le(0).await?;
    file_conn.stream.flush().await?;

    // Create parent directories if needed
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut file = tokio::fs::File::create(&output_path).await?;

    let mut total_bytes = 0u64;
    let mut buffer = vec![0u8; 65536];
    let start_time = std::time::Instant::now();
    let mut next_log_pct: u64 = 25;

    loop {
        match tokio::time::timeout(Duration::from_secs(30), file_conn.stream.read(&mut buffer)).await {
            Ok(Ok(0)) => {
                tracing::warn!("[DOWNLOAD] Connection closed by peer - received: {} bytes", total_bytes);
                break;
            }
            Ok(Ok(n)) => {
                tokio::io::AsyncWriteExt::write_all(&mut file, &buffer[..n]).await?;
                total_bytes += n as u64;

                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 { total_bytes as f64 / elapsed / 1024.0 } else { 0.0 };

                // Progress callback every 256KB (for frontend)
                if total_bytes % (256 * 1024) < 65536 {
                    if let Some(ref callback) = progress_callback {
                        callback(total_bytes, filesize, speed);
                    }
                }

                // Log at 25% intervals
                if filesize > 0 {
                    let pct = (total_bytes as f64 / filesize as f64 * 100.0) as u64;
                    if pct >= next_log_pct && next_log_pct <= 75 {
                        tracing::info!("[DOWNLOAD] Progress: {}% ({:.2} / {:.2} MB) - {:.1} KB/s [\"{}\" from '{}']",
                            next_log_pct, total_bytes as f64 / 1_048_576.0, size_mb, speed, basename, peer_username);
                        next_log_pct += 25;
                    }
                }

                // Check if download complete
                if total_bytes >= filesize {
                    let elapsed = start_time.elapsed();
                    let avg_speed = if elapsed.as_secs_f64() > 0.0 {
                        total_bytes as f64 / elapsed.as_secs_f64() / 1024.0
                    } else { 0.0 };
                    tracing::info!("[DOWNLOAD] Complete: {:.2} MB in {:.1}s ({:.1} KB/s avg) -> \"{}\" from '{}'",
                        total_bytes as f64 / 1_048_576.0, elapsed.as_secs_f64(), avg_speed, basename, peer_username);
                    if let Some(ref callback) = progress_callback {
                        callback(total_bytes, filesize, speed);
                    }
                    break;
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("[DOWNLOAD] Read error: {}", e);
                break;
            }
            Err(_) => {
                tracing::warn!("[DOWNLOAD] Read timeout");
                break;
            }
        }
    }

    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);
    drop(file_conn.stream);

    Ok(total_bytes)
}

/// Send file data over an established F connection (upload).
/// Mirror of `receive_file_data` - used when we are the uploader.
///
/// # Arguments
/// - file_conn: The established F connection
/// - file_path: Path to the file to send
/// - file_size: Total file size
/// - throttle: Optional bandwidth throttle
/// - progress_callback: Optional callback for progress updates
/// - peer_username: Username of the receiving peer (for logging)
///
/// # Returns
/// The number of bytes sent
pub async fn send_file_data(
    mut file_conn: FileConnection,
    file_path: std::path::PathBuf,
    file_size: u64,
    throttle: Option<Arc<crate::services::upload::TokenBucket>>,
    progress_callback: Option<ArcProgressCallback>,
    peer_username: &str,
) -> Result<u64> {
    let size_mb = file_size as f64 / 1_048_576.0;
    let basename = file_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");
    tracing::info!("[UPLOAD] Starting send: {:.2} MB \"{}\" to '{}'", size_mb, basename, peer_username);

    // Read FileOffset from peer (8 bytes, NO length prefix)
    let offset = match tokio::time::timeout(Duration::from_secs(30), file_conn.stream.read_u64_le()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => bail!("Failed to read FileOffset from peer: {}", e),
        Err(_) => bail!("Timeout waiting for FileOffset from peer"),
    };

    tracing::debug!("[UPLOAD] Peer requested offset: {} bytes", offset);

    // Open file and seek to offset
    let mut file = tokio::fs::File::open(&file_path).await
        .context("Failed to open file for upload")?;

    if offset > 0 {
        use tokio::io::AsyncSeekExt;
        file.seek(std::io::SeekFrom::Start(offset)).await
            .context("Failed to seek to offset")?;
    }

    let mut total_bytes = offset;
    let mut buffer = vec![0u8; 65536]; // 64KB chunks
    let start_time = std::time::Instant::now();
    let mut next_log_pct: u64 = 25;

    loop {
        let n = match tokio::time::timeout(Duration::from_secs(30), tokio::io::AsyncReadExt::read(&mut file, &mut buffer)).await {
            Ok(Ok(0)) => break, // EOF
            Ok(Ok(n)) => n,
            Ok(Err(e)) => {
                tracing::warn!("[UPLOAD] File read error: {}", e);
                break;
            }
            Err(_) => {
                tracing::warn!("[UPLOAD] File read timeout");
                break;
            }
        };

        // Apply bandwidth throttle
        if let Some(ref throttle) = throttle {
            throttle.consume(n as u64).await;
        }

        // Write to peer
        match tokio::time::timeout(Duration::from_secs(30), file_conn.stream.write_all(&buffer[..n])).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("[UPLOAD] Write error to '{}': {}", peer_username, e);
                break;
            }
            Err(_) => {
                tracing::warn!("[UPLOAD] Write timeout to '{}'", peer_username);
                break;
            }
        }

        total_bytes += n as u64;

        let elapsed = start_time.elapsed().as_secs_f64();
        let speed = if elapsed > 0.0 { (total_bytes - offset) as f64 / elapsed / 1024.0 } else { 0.0 };

        // Progress callback every 256KB
        if total_bytes % (256 * 1024) < 65536 {
            if let Some(ref callback) = progress_callback {
                callback(total_bytes, file_size, speed);
            }
        }

        // Log at 25% intervals
        if file_size > 0 {
            let pct = (total_bytes as f64 / file_size as f64 * 100.0) as u64;
            if pct >= next_log_pct && next_log_pct <= 75 {
                tracing::info!("[UPLOAD] Progress: {}% ({:.2} / {:.2} MB) - {:.1} KB/s [\"{}\" to '{}']",
                    next_log_pct, total_bytes as f64 / 1_048_576.0, size_mb, speed, basename, peer_username);
                next_log_pct += 25;
            }
        }

        // Check if upload complete
        if total_bytes >= file_size {
            break;
        }
    }

    let _ = file_conn.stream.flush().await;

    let elapsed = start_time.elapsed();
    let bytes_sent = total_bytes - offset;
    let avg_speed = if elapsed.as_secs_f64() > 0.0 {
        bytes_sent as f64 / elapsed.as_secs_f64() / 1024.0
    } else { 0.0 };

    tracing::info!("[UPLOAD] Complete: {:.2} MB in {:.1}s ({:.1} KB/s avg) -> \"{}\" to '{}'",
        bytes_sent as f64 / 1_048_576.0, elapsed.as_secs_f64(), avg_speed, basename, peer_username);

    // Final progress callback
    if let Some(ref callback) = progress_callback {
        callback(total_bytes, file_size, avg_speed);
    }

    Ok(bytes_sent)
}

/// Wait for a file connection (F connection) from a peer.
/// This handles both direct connections (peer connects to us) and indirect (server-brokered).
///
/// Returns the FileConnection once established, which can then be passed to receive_file_data.
/// This function needs the server_stream for handling indirect connections.
pub async fn wait_for_file_connection(
    server_stream: &mut TcpStream,
    peer_username: &str,
    our_username: &str,
    transfer_token: u32,
    file_conn_rx: &mut tokio::sync::mpsc::Receiver<FileConnection>,
    timeout_secs: u64,
) -> Result<FileConnection> {
    tracing::info!("[F-CONN] Waiting for peer '{}' (token={}, timeout={}s)", peer_username, transfer_token, timeout_secs);

    let file_conn = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        let mut loop_count = 0;
        loop {
            loop_count += 1;
            if loop_count % 10 == 0 {
                tracing::debug!("[F-CONN] Still waiting... (loop iteration {})", loop_count);
            }
            tokio::select! {
                biased;

                // Check for direct connection via our listener (PREFERRED)
                conn = file_conn_rx.recv() => {
                    if let Some(conn) = conn {
                        if conn.transfer_token == transfer_token {
                            return Ok::<FileConnection, anyhow::Error>(conn);
                        } else {
                            tracing::debug!("[F-CONN] Token mismatch: expected {}, got {} - ignoring", transfer_token, conn.transfer_token);
                        }
                    } else {
                        tracing::debug!("[F-CONN] file_conn_rx.recv() returned None");
                    }
                }

                // Check for ConnectToPeer type="F" from server
                msg_result = read_message(server_stream) => {
                    match msg_result {
                        Ok((18, data)) => {
                            let mut buf = BytesMut::from(&data[..]);
                            let username = decode_string(&mut buf)?;
                            let conn_type = decode_string(&mut buf)?;
                            let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                            let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                            let port = buf.get_u32_le();
                            let indirect_token = buf.get_u32_le();

                            tracing::debug!("[F-CONN] Got ConnectToPeer: user='{}', type='{}', addr={}:{}, token={}",
                                username, conn_type, ip, port, indirect_token);

                            if conn_type == "F" && username == peer_username {
                                tracing::debug!("[F-CONN] ConnectToPeer F for '{}' - connecting to {}:{}", peer_username, ip, port);
                                // Try indirect connection IMMEDIATELY, not after timeout
                                match connect_for_indirect_transfer(&ip, port, indirect_token, our_username).await {
                                    Ok(conn) => {
                                        return Ok(conn);
                                    }
                                    Err(e) => {
                                        tracing::warn!("[F-CONN] Indirect connection FAILED: {} - continuing to wait", e);
                                        // Continue waiting for direct connection as fallback
                                    }
                                }
                            } else if conn_type == "F" {
                                tracing::debug!("[F-CONN] ConnectToPeer F for different user '{}' (expected '{}')", username, peer_username);
                            }
                        }
                        Ok((code, _)) => {
                            tracing::trace!("[F-CONN] Ignoring server message code {}", code);
                        }
                        Err(e) => {
                            tracing::warn!("[F-CONN] Server read error: {}", e);
                        }
                    }
                }
            }
        }
    }).await;

    match file_conn {
        Ok(Ok(conn)) => {
            tracing::info!("[F-CONN] Established (token={})", conn.transfer_token);
            Ok(conn)
        }
        Ok(Err(e)) => {
            tracing::error!("[F-CONN] Error: {}", e);
            bail!("F connection error: {}", e)
        }
        Err(_) => {
            tracing::error!("[F-CONN] Timeout ({}s elapsed)", timeout_secs);
            bail!("Timeout waiting for file connection")
        }
    }
}

pub async fn download_file(
    server_stream: &mut TcpStream,
    peer_ip: &str,
    peer_port: u32,
    peer_username: &str,
    our_username: &str,
    filename: &str,
    filesize: u64,
    transfer_token: u32,
    file_conn_rx: &mut tokio::sync::mpsc::Receiver<FileConnection>,
    output_path: &std::path::Path,
    progress_callback: Option<ProgressCallback>,
) -> Result<u64> {
    let size_mb = filesize as f64 / 1_048_576.0;
    tracing::info!("[DOWNLOAD] Starting: '{}' from '{}' ({:.2} MB, token={})", filename, peer_username, size_mb, transfer_token);

    // Wait for file connection - either direct (peer connects to us) or indirect (server-brokered)
    let file_conn = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                // BIASED: Prefer direct connections over indirect
                biased;

                // Check for direct connection via our listener (PREFERRED)
                conn = file_conn_rx.recv() => {
                    if let Some(conn) = conn {
                        if conn.transfer_token == transfer_token {
                            return Ok::<FileConnection, anyhow::Error>(conn);
                        } else {
                            tracing::debug!("[DOWNLOAD] Token mismatch: expected {}, got {} - ignoring", transfer_token, conn.transfer_token);
                        }
                    }
                }

                // Check for ConnectToPeer type="F" from server - connect IMMEDIATELY
                msg_result = read_message(server_stream) => {
                    match msg_result {
                        Ok((18, data)) => {
                            let mut buf = BytesMut::from(&data[..]);
                            let username = decode_string(&mut buf)?;
                            let conn_type = decode_string(&mut buf)?;
                            let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                            let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                            let port = buf.get_u32_le();
                            let indirect_token = buf.get_u32_le();

                            if conn_type == "F" && username == peer_username {
                                tracing::debug!("[DOWNLOAD] ConnectToPeer F for '{}' - connecting to {}:{}", peer_username, ip, port);
                                // Try indirect connection IMMEDIATELY
                                match connect_for_indirect_transfer(&ip, port, indirect_token, our_username).await {
                                    Ok(conn) => {
                                        return Ok(conn);
                                    }
                                    Err(e) => {
                                        tracing::warn!("[DOWNLOAD] Indirect connection failed: {} - continuing to wait", e);
                                    }
                                }
                            }
                        }
                        Ok((code, _)) => {
                            tracing::trace!("[DOWNLOAD] Ignoring server message code {}", code);
                        }
                        Err(e) => {
                            tracing::warn!("[DOWNLOAD] Server read error: {}", e);
                        }
                    }
                }
            }
        }
    }).await;

    // Handle file connection result
    let file_conn: Result<FileConnection, anyhow::Error> = match file_conn {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => {
            tracing::warn!("[DOWNLOAD] File connection error: {}", e);
            bail!("File connection error: {}", e)
        }
        Err(_) => {
            bail!("Timeout waiting for file connection - peer couldn't connect and server didn't broker. Try a different peer.")
        }
    };

    let mut file_conn = file_conn?;

    // Send FileOffset (8 bytes, NO length prefix)
    file_conn.stream.write_u64_le(0).await?;
    file_conn.stream.flush().await?;

    // Create parent directories if needed
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut file = tokio::fs::File::create(output_path).await?;
    let dl_basename = output_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let mut total_bytes = 0u64;
    let mut buffer = vec![0u8; 65536];
    let start_time = std::time::Instant::now();
    let mut next_log_pct: u64 = 25;

    loop {
        match tokio::time::timeout(Duration::from_secs(30), file_conn.stream.read(&mut buffer)).await {
            Ok(Ok(0)) => {
                tracing::warn!("[DOWNLOAD] Connection closed by peer - received: {} bytes", total_bytes);
                break;
            }
            Ok(Ok(n)) => {
                tokio::io::AsyncWriteExt::write_all(&mut file, &buffer[..n]).await?;
                total_bytes += n as u64;

                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 { total_bytes as f64 / elapsed / 1024.0 } else { 0.0 };

                // Progress callback every 256KB (for frontend)
                if total_bytes % (256 * 1024) < 65536 {
                    if let Some(ref callback) = progress_callback {
                        callback(total_bytes, filesize, speed);
                    }
                }

                // Log at 25% intervals
                if filesize > 0 {
                    let pct = (total_bytes as f64 / filesize as f64 * 100.0) as u64;
                    if pct >= next_log_pct && next_log_pct <= 75 {
                        tracing::info!("[DOWNLOAD] Progress: {}% ({:.2} / {:.2} MB) - {:.1} KB/s [\"{}\" from '{}']",
                            next_log_pct, total_bytes as f64 / 1_048_576.0, size_mb, speed, dl_basename, peer_username);
                        next_log_pct += 25;
                    }
                }

                // Check if download complete
                if total_bytes >= filesize {
                    let elapsed = start_time.elapsed();
                    let avg_speed = if elapsed.as_secs_f64() > 0.0 {
                        total_bytes as f64 / elapsed.as_secs_f64() / 1024.0
                    } else { 0.0 };
                    tracing::info!("[DOWNLOAD] Complete: {:.2} MB in {:.1}s ({:.1} KB/s avg) -> \"{}\" from '{}'",
                        total_bytes as f64 / 1_048_576.0, elapsed.as_secs_f64(), avg_speed, dl_basename, peer_username);
                    if let Some(ref callback) = progress_callback {
                        callback(total_bytes, filesize, speed);
                    }
                    break;
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("[DOWNLOAD] Read error: {}", e);
                break;
            }
            Err(_) => {
                tracing::warn!("[DOWNLOAD] Read timeout");
                break;
            }
        }
    }

    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);

    // Close the connection (signals completion to uploader)
    drop(file_conn.stream);

    Ok(total_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_string() {
        let mut buf = BytesMut::new();
        encode_string(&mut buf, "test");
        assert_eq!(buf.len(), 8); // 4 bytes length + 4 bytes "test"
        assert_eq!(&buf[..4], &4u32.to_le_bytes());
        assert_eq!(&buf[4..], b"test");
    }
}
