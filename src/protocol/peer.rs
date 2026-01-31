//! Peer protocol helpers for Soulseek P2P connections
//!
//! This module provides low-level peer protocol operations including:
//! - PeerInit and PierceFirewall message handling
//! - File transfer connection management
//! - Both direct and indirect file transfer paths

use anyhow::{Result, bail};
use bytes::{Buf, BufMut, BytesMut};
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
    token: u32,
) -> std::io::Result<()> {
    let msg_len = 1 + 4 + username.len() + 4 + conn_type.len() + 4;
    stream.write_u32_le(msg_len as u32).await?;
    stream.write_u8(1).await?; // PeerInit type
    stream.write_u32_le(username.len() as u32).await?;
    stream.write_all(username.as_bytes()).await?;
    stream.write_u32_le(conn_type.len() as u32).await?;
    stream.write_all(conn_type.as_bytes()).await?;
    stream.write_u32_le(token).await?;
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

    if first_u32 == 5 {
        // PierceFirewall: [length=5][type=0][token]
        let _msg_type = stream.read_u8().await?;
        let token = stream.read_u32_le().await?;
        tracing::debug!("[CONN] Received PierceFirewall, token={}", token);

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

            tracing::debug!("[CONN] Received PeerInit from '{}', type='{}', token={}", peer_username, conn_type, init_token);

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
        tracing::debug!("[CONN] Received raw token: {}", first_u32);
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

    // Step 1: TCP connection
    let mut peer_stream = match tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&addr)
    ).await {
        Ok(Ok(stream)) => {
            tracing::debug!("[ConnectToPeer] Connected to '{}' at {}", peer_username, addr);
            stream
        }
        Ok(Err(e)) => {
            bail!("TCP connect to '{}' failed: {}", peer_username, e);
        }
        Err(_) => {
            bail!("TCP connect to '{}' timed out (peer unreachable)", peer_username);
        }
    };

    // Step 2: Send PierceFirewall
    if let Err(e) = send_pierce_firewall(&mut peer_stream, peer_token).await {
        bail!("Failed to send PierceFirewall to '{}': {}", peer_username, e);
    }
    tracing::debug!("[ConnectToPeer] Sent PierceFirewall to '{}' (token={})", peer_username, peer_token);

    // Step 3: Wait for response - could be PeerInit or FileSearchResponse directly
    let first_u32 = match tokio::time::timeout(Duration::from_secs(5), peer_stream.read_u32_le()).await {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            bail!("'{}' handshake read error: {}", peer_username, e);
        }
        Err(_) => {
            bail!("'{}' handshake timeout (connected but no response)", peer_username);
        }
    };

    if first_u32 == 5 {
        // PierceFirewall response
        let _msg_type = peer_stream.read_u8().await?;
        let _token = peer_stream.read_u32_le().await?;
    } else if first_u32 > 5 && first_u32 < 200 {
        // PeerInit response
        let _msg_type = peer_stream.read_u8().await?;
        let remaining = (first_u32 - 1) as usize;
        let mut data = vec![0u8; remaining];
        peer_stream.read_exact(&mut data).await?;
    } else if first_u32 > 100 && first_u32 < 10_000_000 {
        // FileSearchResponse directly
        let code = peer_stream.read_u32_le().await?;
        if code == 9 {
            let data_len = (first_u32 - 4) as usize;
            let mut compressed = vec![0u8; data_len];
            peer_stream.read_exact(&mut compressed).await?;

            let decompressed = match ZlibDecoder::new(&compressed[..]).bytes().collect::<Result<Vec<_>, _>>() {
                Ok(d) => d,
                Err(_) => compressed,
            };

            let response = parse_file_search_response(&decompressed)?;
            return Ok((peer_stream, response));
        }
    }

    // Wait for FileSearchResponse
    tracing::debug!("[ConnectToPeer] Waiting for FileSearchResponse from '{}'...", peer_username);
    for attempt in 0..10 {
        let msg_len = match tokio::time::timeout(Duration::from_secs(3), peer_stream.read_u32_le()).await {
            Ok(Ok(len)) => len,
            Ok(Err(e)) => {
                bail!("'{}' read error waiting for results: {}", peer_username, e);
            }
            Err(_) => {
                if attempt == 9 {
                    bail!("'{}' results timeout (connected, handshook, but no search results after 30s)", peer_username);
                }
                continue;
            }
        };

        if msg_len == 0 || msg_len > 10_000_000 {
            continue;
        }

        let code = peer_stream.read_u32_le().await?;
        let data_len = (msg_len - 4) as usize;
        let mut data = vec![0u8; data_len];
        peer_stream.read_exact(&mut data).await?;

        if code == 9 {
            let decompressed = match ZlibDecoder::new(&data[..]).bytes().collect::<Result<Vec<_>, _>>() {
                Ok(d) => d,
                Err(_) => data,
            };

            let response = parse_file_search_response(&decompressed)?;
            tracing::debug!("[ConnectToPeer] Got {} files from '{}'", response.files.len(), peer_username);
            return Ok((peer_stream, response));
        } else {
            tracing::debug!("[ConnectToPeer] '{}' sent code {} (not search results), continuing...", peer_username, code);
        }
    }

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

            tracing::info!("[TRANSFER] Sent TransferReply (allowed) for token {}", transfer_token);

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
) -> Result<u64> {
    tracing::info!("[DOWNLOAD] Starting file receive for {:?}", output_path);
    tracing::info!("[DOWNLOAD] Expected size: {:.2} MB", filesize as f64 / 1_048_576.0);

    // Send FileOffset (8 bytes, NO length prefix)
    tracing::info!("[DOWNLOAD] Sending FileOffset (0)...");
    file_conn.stream.write_u64_le(0).await?;
    file_conn.stream.flush().await?;
    tracing::info!("[DOWNLOAD] FileOffset sent!");

    // Create parent directories if needed
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut file = tokio::fs::File::create(&output_path).await?;

    let mut total_bytes = 0u64;
    let mut buffer = vec![0u8; 65536];
    let start_time = std::time::Instant::now();

    loop {
        match tokio::time::timeout(Duration::from_secs(30), file_conn.stream.read(&mut buffer)).await {
            Ok(Ok(0)) => {
                tracing::warn!("[DOWNLOAD] Connection closed by peer - total received: {} bytes", total_bytes);
                break;
            }
            Ok(Ok(n)) => {
                tokio::io::AsyncWriteExt::write_all(&mut file, &buffer[..n]).await?;
                total_bytes += n as u64;

                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 { total_bytes as f64 / elapsed / 1024.0 } else { 0.0 };

                // Progress update every 256KB
                if total_bytes % (256 * 1024) < 65536 {
                    if let Some(ref callback) = progress_callback {
                        callback(total_bytes, filesize, speed);
                    }
                }

                // Log every MB
                if total_bytes % (1024 * 1024) < 65536 {
                    tracing::info!("[DOWNLOAD] Progress: {:.2} / {:.2} MB ({:.1}%) - {:.1} KB/s",
                        total_bytes as f64 / 1_048_576.0,
                        filesize as f64 / 1_048_576.0,
                        (total_bytes as f64 / filesize as f64) * 100.0,
                        speed);
                }

                // Check if download complete
                if total_bytes >= filesize {
                    tracing::info!("[DOWNLOAD] Download complete!");
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

    tracing::info!("[DOWNLOAD] Saved {} bytes to {:?}", total_bytes, output_path);

    Ok(total_bytes)
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
    tracing::info!("[DOWNLOAD] Waiting for F connection (token={})", transfer_token);

    let mut indirect_info: Option<(String, u32, u32)> = None;

    let file_conn = tokio::time::timeout(Duration::from_secs(timeout_secs), async {
        loop {
            tokio::select! {
                biased;

                // Check for direct connection via our listener (PREFERRED)
                conn = file_conn_rx.recv() => {
                    if let Some(conn) = conn {
                        tracing::info!("[DOWNLOAD] Got F connection from channel with token {}", conn.transfer_token);
                        if conn.transfer_token == transfer_token {
                            tracing::info!("[DOWNLOAD] Token matches!");
                            return Ok::<FileConnection, anyhow::Error>(conn);
                        } else {
                            tracing::debug!("[DOWNLOAD] Token mismatch: expected {}, got {}", transfer_token, conn.transfer_token);
                        }
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
                            let token = buf.get_u32_le();

                            if conn_type == "F" && username == peer_username {
                                tracing::info!("[DOWNLOAD] Got ConnectToPeer F from server");
                                indirect_info = Some((ip, port, token));
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

    match file_conn {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(e)) => bail!("F connection error: {}", e),
        Err(_) => {
            // Timeout - try indirect if we have info
            if let Some((ip, port, indirect_token)) = indirect_info {
                tracing::info!("[DOWNLOAD] Trying indirect connection to {}:{}", ip, port);
                connect_for_indirect_transfer(&ip, port, indirect_token, our_username).await
            } else {
                bail!("Timeout waiting for file connection")
            }
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
    tracing::info!("[DOWNLOAD] Starting download from '{}'", peer_username);
    tracing::info!("[DOWNLOAD] File: {}", filename);
    tracing::info!("[DOWNLOAD] Size: {:.2} MB", filesize as f64 / 1_048_576.0);
    tracing::info!("[DOWNLOAD] Transfer token: {}", transfer_token);

    // Now we need to wait for EITHER:
    // 1. Direct connection from uploader on our listener (via file_conn_rx)
    // 2. ConnectToPeer type="F" from server (indirect)

    tracing::debug!("[DOWNLOAD] Waiting for file connection (direct or indirect)...");

    // First, try waiting for the peer to connect to us directly (preferred path)
    // We use biased selection to PREFER direct connections over indirect
    // This prevents race conditions where server sends ConnectToPeer F while peer also connects
    let mut indirect_info: Option<(String, u32, u32)> = None; // (ip, port, indirect_token)

    let file_conn = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            tokio::select! {
                // BIASED: Prefer direct connections over indirect
                biased;

                // Check for direct connection via our listener (PREFERRED)
                conn = file_conn_rx.recv() => {
                    if let Some(conn) = conn {
                        tracing::info!("[DOWNLOAD] Got F connection from channel with token {}", conn.transfer_token);
                        if conn.transfer_token == transfer_token {
                            tracing::info!("[DOWNLOAD] Token matches! Returning connection immediately.");
                            return Ok::<FileConnection, anyhow::Error>(conn);
                        } else {
                            tracing::warn!("[DOWNLOAD] Token mismatch: expected {}, got {} - discarding", transfer_token, conn.transfer_token);
                        }
                    }
                }

                // Check for ConnectToPeer type="F" from server (save for fallback)
                msg_result = read_message(server_stream) => {
                    match msg_result {
                        Ok((18, data)) => {
                            let mut buf = BytesMut::from(&data[..]);
                            let username = decode_string(&mut buf)?;
                            let conn_type = decode_string(&mut buf)?;
                            let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                            let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                            let port = buf.get_u32_le();
                            let token = buf.get_u32_le();

                            if conn_type == "F" && username == peer_username {
                                tracing::info!("[DOWNLOAD] Got ConnectToPeer F from server (saving as fallback)");
                                indirect_info = Some((ip, port, token));
                                // Don't immediately connect - wait a bit for direct connection
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
        Ok(Ok(conn)) => {
            tracing::info!("[DOWNLOAD] Got file connection (token={})", conn.transfer_token);
            Ok(conn)
        }
        Ok(Err(e)) => {
            tracing::warn!("[DOWNLOAD] File connection error: {}", e);
            bail!("File connection error: {}", e)
        }
        Err(_) => {
            // Timeout waiting for direct connection
            tracing::warn!("[DOWNLOAD] Timeout waiting for direct F connection.");

            // If we received indirect connection info from server, try that now
            if let Some((ip, port, indirect_token)) = indirect_info {
                tracing::info!("[DOWNLOAD] Trying saved indirect connection to {}:{}", ip, port);
                match connect_for_indirect_transfer(&ip, port, indirect_token, our_username).await {
                    Ok(conn) => Ok(conn),
                    Err(e) => {
                        tracing::warn!("[DOWNLOAD] Indirect connection failed: {}", e);
                        bail!("Both direct and indirect connections failed")
                    }
                }
            } else {
                // No indirect info, wait a bit longer for either path
                tracing::info!("[DOWNLOAD] Waiting additional 10s for connection...");

                match tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        tokio::select! {
                            biased;

                            conn = file_conn_rx.recv() => {
                                if let Some(conn) = conn {
                                    if conn.transfer_token == transfer_token {
                                        return Ok::<FileConnection, anyhow::Error>(conn);
                                    }
                                }
                            }
                            msg_result = read_message(server_stream) => {
                                if let Ok((18, data)) = msg_result {
                                    let mut buf = BytesMut::from(&data[..]);
                                    let username = decode_string(&mut buf)?;
                                    let conn_type = decode_string(&mut buf)?;
                                    let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                                    let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                                    let port = buf.get_u32_le();
                                    let indirect_token = buf.get_u32_le();

                                    if conn_type == "F" && username == peer_username {
                                        tracing::info!("[DOWNLOAD] Got late ConnectToPeer F from server!");
                                        let conn = connect_for_indirect_transfer(&ip, port, indirect_token, our_username).await?;
                                        return Ok(conn);
                                    }
                                }
                            }
                        }
                    }
                }).await {
                    Ok(Ok(conn)) => Ok(conn),
                    _ => bail!("Timeout waiting for file connection - peer couldn't connect and server didn't broker. Try a different peer.")
                }
            }
        }
    };

    let mut file_conn = file_conn?;
    tracing::info!("[DOWNLOAD] Have file connection, sending FileOffset...");

    // Send FileOffset (8 bytes, NO length prefix)
    tracing::info!("[DOWNLOAD] Sending FileOffset (0)...");
    file_conn.stream.write_u64_le(0).await?;
    file_conn.stream.flush().await?;
    tracing::info!("[DOWNLOAD] FileOffset sent and flushed!");

    // Receive file data
    tracing::info!("[DOWNLOAD] Waiting for file data...");

    // Create parent directories if needed
    if let Some(parent) = output_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut file = tokio::fs::File::create(output_path).await?;

    let mut total_bytes = 0u64;
    let mut buffer = vec![0u8; 65536];
    let start_time = std::time::Instant::now();

    tracing::info!("[DOWNLOAD] Starting data receive loop...");
    loop {
        match tokio::time::timeout(Duration::from_secs(30), file_conn.stream.read(&mut buffer)).await {
            Ok(Ok(0)) => {
                tracing::warn!("[DOWNLOAD] Connection closed by peer (got 0 bytes) - total received: {} bytes", total_bytes);
                break;
            }
            Ok(Ok(n)) => {
                tokio::io::AsyncWriteExt::write_all(&mut file, &buffer[..n]).await?;
                total_bytes += n as u64;

                let elapsed = start_time.elapsed().as_secs_f64();
                let speed = if elapsed > 0.0 { total_bytes as f64 / elapsed / 1024.0 } else { 0.0 };

                // Progress update every MB (log) or every 256KB (callback)
                if total_bytes % (256 * 1024) < 65536 {
                    // Call progress callback if provided
                    if let Some(ref callback) = progress_callback {
                        callback(total_bytes, filesize, speed);
                    }
                }

                if total_bytes % (1024 * 1024) < 65536 {
                    tracing::info!("[DOWNLOAD] Progress: {:.2} / {:.2} MB ({:.1}%) - {:.1} KB/s",
                        total_bytes as f64 / 1_048_576.0,
                        filesize as f64 / 1_048_576.0,
                        (total_bytes as f64 / filesize as f64) * 100.0,
                        speed);
                }

                // Check if download complete
                if total_bytes >= filesize {
                    tracing::info!("[DOWNLOAD] Download complete!");
                    // Final progress callback
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

    tracing::info!("[DOWNLOAD] Saved {} bytes to {:?}", total_bytes, output_path);

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
