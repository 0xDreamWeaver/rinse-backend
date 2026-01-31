//! E2E test for Soulseek search functionality
//!
//! Run with: cargo test --test e2e_search -- --nocapture

use std::time::Duration;
use std::sync::Arc;
use tokio::net::{TcpStream, TcpListener};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, Mutex};
use bytes::{Buf, BufMut, BytesMut};

const SERVER_HOST: &str = "server.slsknet.org";
const SERVER_PORT: u16 = 2242;
const LISTEN_PORT: u16 = 2234;
const OBFUSCATED_PORT: u16 = 2235;

// ============================================================================
// Obfuscation helpers
// ============================================================================

fn obfuscate(data: &[u8]) -> Vec<u8> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let initial_key: u32 = rng.gen();

    let mut result = Vec::with_capacity(4 + data.len());
    result.extend_from_slice(&initial_key.to_le_bytes());

    let mut key = initial_key;
    for chunk in data.chunks(4) {
        key = key.rotate_right(31);
        let key_bytes = key.to_le_bytes();
        for (i, &byte) in chunk.iter().enumerate() {
            result.push(byte ^ key_bytes[i]);
        }
    }
    result
}

fn deobfuscate(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < 4 {
        return None;
    }

    let initial_key = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
    let encrypted_data = &data[4..];
    let mut result = Vec::with_capacity(encrypted_data.len());

    let mut key = initial_key;
    for chunk in encrypted_data.chunks(4) {
        key = key.rotate_right(31);
        let key_bytes = key.to_le_bytes();
        for (i, &byte) in chunk.iter().enumerate() {
            result.push(byte ^ key_bytes[i]);
        }
    }
    Some(result)
}

// ============================================================================
// Query parsing and fuzzy matching
// ============================================================================

/// Parse a search query in "{artist} - {title}" format
fn parse_query(query: &str) -> (Option<String>, Option<String>) {
    if let Some(idx) = query.find(" - ") {
        let artist = query[..idx].trim().to_lowercase();
        let title = query[idx + 3..].trim().to_lowercase();
        (Some(artist), Some(title))
    } else {
        // No separator found, treat entire query as title
        (None, Some(query.trim().to_lowercase()))
    }
}

/// Normalize a string for fuzzy matching
fn normalize_for_matching(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extract the track name from a filename (strip path, number prefix, extension)
fn extract_track_name(filename: &str) -> String {
    // Get just the filename part (after last / or \)
    let basename = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename);

    // Remove extension
    let without_ext = basename
        .rsplit_once('.')
        .map(|(name, _)| name)
        .unwrap_or(basename);

    // Remove common track number prefixes like "01 - ", "01. ", "01_"
    let track_name = without_ext
        .trim_start_matches(|c: char| c.is_numeric())
        .trim_start_matches([' ', '-', '.', '_'])
        .trim();

    normalize_for_matching(track_name)
}

/// Score how well a filename matches the expected title (0.0 = no match, 1.0 = perfect)
fn score_title_match(filename: &str, expected_title: &str) -> f64 {
    let track_name = extract_track_name(filename);
    let expected = normalize_for_matching(expected_title);

    if track_name.is_empty() || expected.is_empty() {
        return 0.0;
    }

    // Exact match
    if track_name == expected {
        return 1.0;
    }

    // Track name contains expected title
    if track_name.contains(&expected) {
        return 0.9;
    }

    // Expected title contains track name (partial match)
    if expected.contains(&track_name) {
        return 0.7;
    }

    // Check word overlap
    let track_words: std::collections::HashSet<_> = track_name.split_whitespace().collect();
    let expected_words: std::collections::HashSet<_> = expected.split_whitespace().collect();

    let intersection = track_words.intersection(&expected_words).count();
    if intersection > 0 {
        let union = track_words.union(&expected_words).count();
        return (intersection as f64 / union as f64) * 0.6;
    }

    0.0
}

/// Score a file for selection (higher is better)
/// Considers: title match, format quality, bitrate
fn score_file(filename: &str, size: u64, bitrate: Option<u32>, expected_title: &str) -> f64 {
    let mut score = 0.0;

    // Title match is most important (0-100 points)
    let title_score = score_title_match(filename, expected_title);
    score += title_score * 100.0;

    // Format preference (0-20 points)
    let lower = filename.to_lowercase();
    if lower.ends_with(".flac") {
        score += 20.0;  // Lossless
    } else if lower.ends_with(".mp3") {
        score += 10.0;  // Common lossy
    } else if lower.ends_with(".wav") {
        score += 15.0;  // Uncompressed
    }

    // Bitrate bonus for lossy formats (0-10 points)
    if let Some(br) = bitrate {
        if lower.ends_with(".mp3") {
            // 320kbps = full bonus, scale down from there
            score += (br as f64 / 320.0).min(1.0) * 10.0;
        }
    }

    // Reasonable file size (1-50MB for a single track) gets a small bonus
    let size_mb = size as f64 / 1_048_576.0;
    if size_mb >= 1.0 && size_mb <= 50.0 {
        score += 5.0;
    }

    score
}

// ============================================================================
// Protocol helpers
// ============================================================================

fn load_credentials() -> (String, String) {
    dotenvy::dotenv().ok();
    let username = std::env::var("SLSK_USERNAME").expect("SLSK_USERNAME not set");
    let password = std::env::var("SLSK_PASSWORD").expect("SLSK_PASSWORD not set");
    (username, password)
}

fn encode_string(buf: &mut BytesMut, s: &str) {
    buf.put_u32_le(s.len() as u32);
    buf.put_slice(s.as_bytes());
}

fn decode_string(buf: &mut impl Buf) -> String {
    let len = buf.get_u32_le() as usize;
    let mut bytes = vec![0u8; len];
    buf.copy_to_slice(&mut bytes);
    String::from_utf8_lossy(&bytes).to_string()
}

async fn send_message(stream: &mut TcpStream, code: u32, data: &[u8]) -> std::io::Result<()> {
    let msg_len = 4 + data.len();
    stream.write_u32_le(msg_len as u32).await?;
    stream.write_u32_le(code).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_message(stream: &mut TcpStream) -> std::io::Result<(u32, Vec<u8>)> {
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
// Server protocol
// ============================================================================

async fn login(stream: &mut TcpStream, username: &str, password: &str) -> anyhow::Result<()> {
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
        let reason = decode_string(&mut buf);
        println!("[LOGIN] Success: {}, Message: {}", success, reason);
        if !success {
            anyhow::bail!("Login failed: {}", reason);
        }
    }

    Ok(())
}

async fn set_wait_port(stream: &mut TcpStream, port: u32) -> std::io::Result<()> {
    let mut data = BytesMut::new();
    data.put_u32_le(port);
    send_message(stream, 2, &data).await
}

async fn set_shared_counts(stream: &mut TcpStream, folders: u32, files: u32) -> std::io::Result<()> {
    let mut data = BytesMut::new();
    data.put_u32_le(folders);
    data.put_u32_le(files);
    send_message(stream, 35, &data).await
}

async fn file_search(stream: &mut TcpStream, token: u32, query: &str) -> std::io::Result<()> {
    let mut data = BytesMut::new();
    data.put_u32_le(token);
    encode_string(&mut data, query);
    send_message(stream, 26, &data).await
}

// ============================================================================
// Peer protocol helpers
// ============================================================================

/// Send PeerInit message
async fn send_peer_init(stream: &mut TcpStream, username: &str, conn_type: &str, token: u32) -> std::io::Result<()> {
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

/// Send PierceFirewall message
async fn send_pierce_firewall(stream: &mut TcpStream, token: u32) -> std::io::Result<()> {
    stream.write_u32_le(5).await?; // length = 5
    stream.write_u8(0).await?; // type = 0 (PierceFirewall)
    stream.write_u32_le(token).await?;
    stream.flush().await?;
    Ok(())
}

/// Read PeerInit from stream, returns (username, conn_type, token)
async fn read_peer_init(stream: &mut TcpStream) -> anyhow::Result<(String, String, u32)> {
    let msg_len = stream.read_u32_le().await?;
    let msg_type = stream.read_u8().await?;

    if msg_type != 1 {
        anyhow::bail!("Expected PeerInit (type 1), got type {}", msg_type);
    }

    let remaining = (msg_len - 1) as usize;
    let mut data = vec![0u8; remaining];
    stream.read_exact(&mut data).await?;

    let mut buf = BytesMut::from(&data[..]);
    let username = decode_string(&mut buf);
    let conn_type = decode_string(&mut buf);
    let token = buf.get_u32_le();

    Ok((username, conn_type, token))
}

// ============================================================================
// File Transfer Connection Handler
// ============================================================================

/// Represents an incoming file transfer connection ready for download
struct FileConnection {
    stream: TcpStream,
    transfer_token: u32,
}

/// Handle an incoming F connection (direct path - uploader connected to us)
async fn handle_incoming_f_connection(
    mut stream: TcpStream,
    our_username: &str,
) -> anyhow::Result<FileConnection> {
    // Read first 4 bytes to determine if it's PeerInit (length-prefixed) or PierceFirewall
    let first_u32 = stream.read_u32_le().await?;

    if first_u32 == 5 {
        // PierceFirewall: [length=5][type=0][token]
        let msg_type = stream.read_u8().await?;
        let token = stream.read_u32_le().await?;
        println!("[F-CONN] Received PierceFirewall, token={}", token);

        // Respond with PeerInit type F
        send_peer_init(&mut stream, our_username, "F", 0).await?;

        // Now wait for FileTransferInit (4 bytes, raw token, NO length prefix)
        let transfer_token = stream.read_u32_le().await?;
        println!("[F-CONN] Received FileTransferInit, transfer_token={}", transfer_token);

        Ok(FileConnection { stream, transfer_token })

    } else if first_u32 > 5 && first_u32 < 200 {
        // Likely PeerInit
        let msg_type = stream.read_u8().await?;

        if msg_type == 1 {
            // PeerInit: read remaining bytes
            let remaining = (first_u32 - 1) as usize;
            let mut data = vec![0u8; remaining];
            stream.read_exact(&mut data).await?;

            let mut buf = BytesMut::from(&data[..]);
            let peer_username = decode_string(&mut buf);
            let conn_type = decode_string(&mut buf);
            let _init_token = buf.get_u32_le();

            println!("[F-CONN] Received PeerInit from '{}', type='{}' ", peer_username, conn_type);

            if conn_type != "F" {
                anyhow::bail!("Expected F connection, got type '{}'", conn_type);
            }

            // Respond with our PeerInit
            send_peer_init(&mut stream, our_username, "F", 0).await?;
            println!("[F-CONN] Sent PeerInit response");

            // Now wait for FileTransferInit (4 bytes, raw token, NO length prefix)
            let transfer_token = stream.read_u32_le().await?;
            println!("[F-CONN] Received FileTransferInit, transfer_token={}", transfer_token);

            Ok(FileConnection { stream, transfer_token })
        } else {
            anyhow::bail!("Expected PeerInit (type 1), got type {}", msg_type);
        }
    } else {
        // Might be raw FileTransferInit (4-byte token) if handshake already done
        println!("[F-CONN] Received raw token: {}", first_u32);
        Ok(FileConnection { stream, transfer_token: first_u32 })
    }
}

/// Connect to uploader for indirect file transfer (Path B)
async fn connect_for_indirect_transfer(
    ip: &str,
    port: u32,
    indirect_token: u32,
    our_username: &str,
) -> anyhow::Result<FileConnection> {
    let addr = format!("{}:{}", ip, port);
    println!("[INDIRECT] Connecting to {} for file transfer...", addr);

    let mut stream = tokio::time::timeout(
        Duration::from_secs(10),
        TcpStream::connect(&addr)
    ).await??;

    println!("[INDIRECT] Connected, sending PierceFirewall with token {}", indirect_token);

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
            println!("[INDIRECT] Received PeerInit response");
        }

        // Now wait for FileTransferInit
        let transfer_token = stream.read_u32_le().await?;
        println!("[INDIRECT] Received FileTransferInit, transfer_token={}", transfer_token);

        Ok(FileConnection { stream, transfer_token })
    } else {
        // Assume it's the transfer token directly
        println!("[INDIRECT] Received FileTransferInit, transfer_token={}", first_u32);
        Ok(FileConnection { stream, transfer_token: first_u32 })
    }
}

// ============================================================================
// Download function with both paths
// ============================================================================

/// Download a file from a peer
///
/// This function implements both file transfer paths:
/// - Path A (Direct): Uploader connects to our listener
/// - Path B (Indirect): We receive ConnectToPeer type="F" and connect to uploader
async fn download_file(
    server_stream: &mut TcpStream,
    peer_ip: &str,
    peer_port: u32,
    peer_username: &str,
    our_username: &str,
    filename: &str,
    filesize: u64,
    transfer_token: u32,
    file_conn_rx: &mut mpsc::Receiver<FileConnection>,
) -> anyhow::Result<u64> {
    println!("\n[DOWNLOAD] ========================================");
    println!("[DOWNLOAD] Starting download from '{}'", peer_username);
    println!("[DOWNLOAD] File: {}", filename);
    println!("[DOWNLOAD] Size: {:.2} MB", filesize as f64 / 1_048_576.0);
    println!("[DOWNLOAD] Transfer token: {}", transfer_token);
    println!("[DOWNLOAD] ========================================\n");

    // Now we need to wait for EITHER:
    // 1. Direct connection from uploader on our listener (via file_conn_rx)
    // 2. ConnectToPeer type="F" from server (indirect)

    println!("[DOWNLOAD] Waiting for file connection (direct or indirect)...");
    println!("[DOWNLOAD] Will wait 15s for peer to connect, then try proactive connection");

    // First, try waiting for the peer to connect to us (the correct protocol)
    let file_conn = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            tokio::select! {
                // Check for direct connection via our listener
                conn = file_conn_rx.recv() => {
                    if let Some(conn) = conn {
                        println!("[DOWNLOAD] Got direct F connection with token {}", conn.transfer_token);
                        if conn.transfer_token == transfer_token {
                            return Ok::<FileConnection, anyhow::Error>(conn);
                        } else {
                            println!("[DOWNLOAD] Token mismatch: expected {}, got {}", transfer_token, conn.transfer_token);
                        }
                    }
                }

                // Check for ConnectToPeer type="F" from server (indirect path)
                msg_result = read_message(server_stream) => {
                    match msg_result {
                        Ok((18, data)) => {
                            let mut buf = BytesMut::from(&data[..]);
                            let username = decode_string(&mut buf);
                            let conn_type = decode_string(&mut buf);
                            let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                            let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                            let port = buf.get_u32_le();
                            let indirect_token = buf.get_u32_le();

                            if conn_type == "F" && username == peer_username {
                                println!("[DOWNLOAD] Indirect F connection request - connecting to uploader");
                                let conn = connect_for_indirect_transfer(&ip, port, indirect_token, our_username).await?;
                                return Ok(conn);
                            }
                        }
                        Ok((code, _)) => {
                            // Ignore other messages
                        }
                        Err(e) => {
                            println!("[DOWNLOAD] Server read error: {}", e);
                        }
                    }
                }
            }
        }
    }).await;

    // If timeout, try proactive F connection to peer (fallback - some peers support this)
    let file_conn: Result<FileConnection, anyhow::Error> = match file_conn {
        Ok(Ok(conn)) => Ok(conn),
        _ => {
            println!("[DOWNLOAD] No incoming connection, trying proactive F connection to peer...");

            // Try connecting to the peer for file transfer
            let addr = format!("{}:{}", peer_ip, peer_port);
            match tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addr)).await {
                Ok(Ok(mut stream)) => {
                    println!("[DOWNLOAD] Connected to peer, sending PeerInit type F...");

                    // Send PeerInit with type F and token 0
                    send_peer_init(&mut stream, our_username, "F", 0).await?;

                    // Wait for peer's PeerInit response
                    match tokio::time::timeout(Duration::from_secs(5), stream.read_u32_le()).await {
                        Ok(Ok(msg_len)) => {
                            if msg_len > 5 && msg_len < 200 {
                                let msg_type = stream.read_u8().await?;
                                if msg_type == 1 {
                                    let remaining = (msg_len - 1) as usize;
                                    let mut data = vec![0u8; remaining];
                                    stream.read_exact(&mut data).await?;
                                    println!("[DOWNLOAD] Got PeerInit response");
                                }
                            }
                        }
                        _ => println!("[DOWNLOAD] No PeerInit response, continuing anyway"),
                    }

                    // Now send FileTransferInit with our token
                    println!("[DOWNLOAD] Sending FileTransferInit with token {}", transfer_token);
                    stream.write_u32_le(transfer_token).await?;
                    stream.flush().await?;

                    // The peer should now send us the file data after we send offset
                    Ok(FileConnection { stream, transfer_token })
                }
                Ok(Err(e)) => {
                    println!("[DOWNLOAD] Proactive connection failed: {}", e);
                    anyhow::bail!("Failed to establish file connection: {}", e)
                }
                Err(_) => {
                    println!("[DOWNLOAD] Proactive connection timeout");
                    anyhow::bail!("Timeout waiting for file connection")
                }
            }
        }
    };

    let mut file_conn = file_conn?;

    // Send FileOffset (8 bytes, NO length prefix)
    println!("[DOWNLOAD] Sending FileOffset (0)...");
    file_conn.stream.write_u64_le(0).await?;
    file_conn.stream.flush().await?;

    // Receive file data
    println!("[DOWNLOAD] Receiving file data...");

    // Save to storage directory
    let storage_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("storage");
    tokio::fs::create_dir_all(&storage_dir).await?;
    let output_path = storage_dir.join(filename.split(['/', '\\']).last().unwrap_or("download"));
    let mut file = tokio::fs::File::create(&output_path).await?;

    let mut total_bytes = 0u64;
    let mut buffer = vec![0u8; 65536];
    let start_time = std::time::Instant::now();

    loop {
        match tokio::time::timeout(Duration::from_secs(30), file_conn.stream.read(&mut buffer)).await {
            Ok(Ok(0)) => {
                println!("[DOWNLOAD] Connection closed by peer");
                break;
            }
            Ok(Ok(n)) => {
                tokio::io::AsyncWriteExt::write_all(&mut file, &buffer[..n]).await?;
                total_bytes += n as u64;

                // Progress update every MB
                if total_bytes % (1024 * 1024) < 65536 {
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let speed = if elapsed > 0.0 { total_bytes as f64 / elapsed / 1024.0 } else { 0.0 };
                    println!("[DOWNLOAD] Progress: {:.2} / {:.2} MB ({:.1}%) - {:.1} KB/s",
                        total_bytes as f64 / 1_048_576.0,
                        filesize as f64 / 1_048_576.0,
                        (total_bytes as f64 / filesize as f64) * 100.0,
                        speed);
                }

                // Check if download complete
                if total_bytes >= filesize {
                    println!("[DOWNLOAD] Download complete!");
                    break;
                }
            }
            Ok(Err(e)) => {
                println!("[DOWNLOAD] Read error: {}", e);
                break;
            }
            Err(_) => {
                println!("[DOWNLOAD] Read timeout");
                break;
            }
        }
    }

    tokio::io::AsyncWriteExt::flush(&mut file).await?;
    drop(file);

    // Close the connection (signals completion to uploader)
    drop(file_conn.stream);

    println!("[DOWNLOAD] Saved {} bytes to {}", total_bytes, output_path.display());

    Ok(total_bytes)
}

/// Negotiate a download with a peer (send TransferRequest, receive TransferReply)
/// Returns (filesize, transfer_token) if successful
async fn negotiate_download(
    peer_stream: &mut TcpStream,
    filename: &str,
) -> anyhow::Result<(u64, u32)> {
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

    println!("[NEGOTIATE] Sent TransferRequest (token={}) for '{}'", transfer_token, filename);

    // Wait for TransferReply (code 41)
    let msg_len = tokio::time::timeout(Duration::from_secs(30), peer_stream.read_u32_le()).await??;
    let code = peer_stream.read_u32_le().await?;

    if code != 41 {
        // Read remaining data for debugging
        let data_len = (msg_len - 4) as usize;
        let mut data = vec![0u8; data_len];
        peer_stream.read_exact(&mut data).await?;

        if code == 44 {
            // PlaceInQueueResponse - file is queued
            let mut buf = BytesMut::from(&data[..]);
            let queued_file = decode_string(&mut buf);
            let place = buf.get_u32_le();
            anyhow::bail!("File queued at position {}", place);
        } else if code == 50 {
            // UploadDenied
            let mut buf = BytesMut::from(&data[..]);
            let denied_file = decode_string(&mut buf);
            let reason = decode_string(&mut buf);
            anyhow::bail!("Upload denied: {}", reason);
        } else {
            anyhow::bail!("Unexpected response code: {}", code);
        }
    }

    // Parse TransferReply
    let reply_token = peer_stream.read_u32_le().await?;
    let allowed = peer_stream.read_u8().await?;

    println!("[NEGOTIATE] TransferReply: token={}, allowed={}", reply_token, allowed);

    if allowed == 1 {
        // Transfer allowed - read filesize
        let filesize = peer_stream.read_u64_le().await?;
        println!("[NEGOTIATE] Transfer allowed! Size: {} bytes ({:.2} MB)", filesize, filesize as f64 / 1_048_576.0);
        Ok((filesize, transfer_token))
    } else {
        // Transfer denied - read reason
        let remaining = (msg_len - 4 - 4 - 1) as usize;
        let mut reason_data = vec![0u8; remaining];
        peer_stream.read_exact(&mut reason_data).await?;
        let mut buf = BytesMut::from(&reason_data[..]);
        let reason = decode_string(&mut buf);
        anyhow::bail!("Transfer denied: {}", reason);
    }
}

// ============================================================================
// Search result handling
// ============================================================================

/// Parsed search response with user stats
struct ParsedSearchResponse {
    files: Vec<(String, u64, Option<u32>)>,
    slot_free: bool,
    avg_speed: u32,  // bytes per second
    queue_length: u32,
}

fn parse_file_search_response(data: &[u8]) -> anyhow::Result<ParsedSearchResponse> {
    let mut buf = BytesMut::from(data);

    if buf.remaining() < 8 {
        anyhow::bail!("Not enough data");
    }

    let username = decode_string(&mut buf);
    let token = buf.get_u32_le();
    let file_count = buf.get_u32_le();

    println!("[PARSE] FileSearchResponse: user='{}', token={}, files={}", username, token, file_count);

    let mut files = Vec::new();
    for _i in 0..file_count.min(100) {
        if buf.remaining() < 1 {
            break;
        }

        let _code = buf.get_u8();
        let filename = decode_string(&mut buf);

        if buf.remaining() < 8 {
            break;
        }
        let size = buf.get_u64_le();

        let _extension = decode_string(&mut buf);

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
    println!("[PARSE] User stats: free={}, speed={:.1} KB/s, queue={}",
        slot_free, speed_kbps, queue_length);

    Ok(ParsedSearchResponse {
        files,
        slot_free,
        avg_speed,
        queue_length,
    })
}

/// Connect to peer for search results
async fn connect_to_peer_for_results(
    ip: &str,
    port: u32,
    _peer_username: &str,
    _our_username: &str,
    peer_token: u32,
) -> anyhow::Result<(TcpStream, ParsedSearchResponse)> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    let addr = format!("{}:{}", ip, port);

    let mut peer_stream = tokio::time::timeout(
        Duration::from_secs(5),
        TcpStream::connect(&addr)
    ).await??;

    // Send PierceFirewall
    send_pierce_firewall(&mut peer_stream, peer_token).await?;

    // Wait for response - could be PeerInit or FileSearchResponse directly
    let first_u32 = tokio::time::timeout(Duration::from_secs(5), peer_stream.read_u32_le()).await??;

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
    for _ in 0..10 {
        let msg_len = tokio::time::timeout(Duration::from_secs(3), peer_stream.read_u32_le()).await??;

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
            return Ok((peer_stream, response));
        }
    }

    anyhow::bail!("No FileSearchResponse received")
}

// ============================================================================
// Main test
// ============================================================================

#[tokio::test]
async fn test_search_raw() -> anyhow::Result<()> {
    let (username, password) = load_credentials();

    println!("\n=== SOULSEEK SEARCH & DOWNLOAD E2E TEST ===\n");

    // Initialize UPnP
    println!("[UPnP] Initializing port forwarding...");
    match rinse_backend::services::upnp::init_upnp().await {
        Ok(true) => println!("[UPnP] Port forwarding enabled"),
        Ok(false) => println!("[UPnP] UPnP not available, manual port forwarding may be required"),
        Err(e) => println!("[UPnP] Error: {} - continuing anyway", e),
    }

    // Connect to server
    println!("[TEST] Connecting to {}:{}", SERVER_HOST, SERVER_PORT);
    let mut stream = TcpStream::connect(format!("{}:{}", SERVER_HOST, SERVER_PORT)).await?;
    println!("[TEST] Connected!");

    // Login
    println!("[TEST] Logging in as '{}'...", username);
    login(&mut stream, &username, &password).await?;

    // Set wait port
    println!("[TEST] Setting wait port to {}...", LISTEN_PORT);
    set_wait_port(&mut stream, LISTEN_PORT as u32).await?;

    // Set shared counts
    set_shared_counts(&mut stream, 0, 0).await?;

    // Channel for file connections from listener
    let (file_tx, mut file_rx) = mpsc::channel::<FileConnection>(10);

    // Start listener for incoming connections
    let our_username_clone = username.clone();
    let listener_handle = tokio::spawn(async move {
        let listener = match TcpListener::bind(format!("0.0.0.0:{}", LISTEN_PORT)).await {
            Ok(l) => {
                println!("[LISTENER] Listening on port {}", LISTEN_PORT);
                l
            }
            Err(e) => {
                println!("[LISTENER] Failed to bind: {}", e);
                return;
            }
        };

        loop {
            tokio::select! {
                result = listener.accept() => {
                    match result {
                        Ok((stream, addr)) => {
                            println!("[LISTENER] Incoming connection from {}", addr);

                            let file_tx = file_tx.clone();
                            let username = our_username_clone.clone();

                            tokio::spawn(async move {
                                match handle_incoming_f_connection(stream, &username).await {
                                    Ok(conn) => {
                                        println!("[LISTENER] F connection ready, token={}", conn.transfer_token);
                                        let _ = file_tx.send(conn).await;
                                    }
                                    Err(e) => {
                                        println!("[LISTENER] Failed to handle connection: {}", e);
                                    }
                                }
                            });
                        }
                        Err(e) => println!("[LISTENER] Accept error: {}", e),
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(300)) => {
                    println!("[LISTENER] Timeout, stopping");
                    break;
                }
            }
        }
    });

    // Send search
    let search_token: u32 = rand::random();
    let query = "Skrillex - Bangarang";

    println!("\n[TEST] ========================================");
    println!("[TEST] SEARCHING: '{}'", query);
    println!("[TEST] ========================================\n");

    file_search(&mut stream, search_token, query).await?;

    // Collect results
    #[derive(Debug, Clone)]
    struct SearchResult {
        peer_username: String,
        peer_ip: String,
        peer_port: u32,
        peer_token: u32,
        files: Vec<(String, u64, Option<u32>)>,
        peer_stream: Option<Arc<Mutex<TcpStream>>>,
        slot_free: bool,       // User has free upload slots
        avg_speed: u32,        // User's average upload speed (bytes/sec)
        queue_length: u32,     // User's upload queue length
    }

    let mut all_results: Vec<SearchResult> = Vec::new();
    let mut connect_to_peer_count = 0;

    let start = std::time::Instant::now();
    let search_timeout = Duration::from_secs(30);

    println!("[TEST] Listening for search results...\n");

    while start.elapsed() < search_timeout {
        match tokio::time::timeout(Duration::from_millis(100), read_message(&mut stream)).await {
            Ok(Ok((18, data))) => {
                // ConnectToPeer - peer has results
                connect_to_peer_count += 1;
                let mut buf = BytesMut::from(&data[..]);
                let peer_username = decode_string(&mut buf);
                let conn_type = decode_string(&mut buf);
                let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                let port = buf.get_u32_le();
                let peer_token = buf.get_u32_le();

                if conn_type == "P" && connect_to_peer_count <= 30 {
                    println!("[PEER {:2}] {} at {}:{}", connect_to_peer_count, peer_username, ip, port);

                    // Try to connect and get results
                    match connect_to_peer_for_results(&ip, port, &peer_username, &username, peer_token).await {
                        Ok((peer_stream, response)) => {
                            let speed_kbps = response.avg_speed as f64 / 1024.0;
                            let free_str = if response.slot_free { "FREE" } else { "BUSY" };
                            println!("[SUCCESS] Got {} files from '{}' ({}, {:.0} KB/s, queue: {})",
                                response.files.len(), peer_username, free_str, speed_kbps, response.queue_length);

                            for (i, (filename, size, bitrate)) in response.files.iter().take(3).enumerate() {
                                let br = bitrate.map(|b| format!("{}kbps", b)).unwrap_or_else(|| "?".to_string());
                                println!("  {}. {} ({:.1}MB, {})", i + 1, filename, *size as f64 / 1_048_576.0, br);
                            }
                            if response.files.len() > 3 {
                                println!("  ... and {} more", response.files.len() - 3);
                            }

                            all_results.push(SearchResult {
                                peer_username: peer_username.clone(),
                                peer_ip: ip,
                                peer_port: port,
                                peer_token,
                                files: response.files,
                                peer_stream: Some(Arc::new(Mutex::new(peer_stream))),
                                slot_free: response.slot_free,
                                avg_speed: response.avg_speed,
                                queue_length: response.queue_length,
                            });
                        }
                        Err(_e) => {
                            // Silent failure for most cases
                        }
                    }
                }
            }
            Ok(Ok((code, _))) => {
                // Other messages, ignore
            }
            Ok(Err(e)) => {
                println!("[ERROR] Server error: {}", e);
                break;
            }
            Err(_) => {
                // Timeout, continue
            }
        }
    }

    println!("\n[TEST] ========================================");
    println!("[TEST] SEARCH RESULTS SUMMARY");
    println!("[TEST] ========================================");
    println!("[TEST] Peers with results: {}", connect_to_peer_count);
    println!("[TEST] Successful connections: {}", all_results.len());
    println!("[TEST] Total files: {}", all_results.iter().map(|r| r.files.len()).sum::<usize>());
    println!("[TEST] ========================================\n");

    // Try to download a file
    if !all_results.is_empty() {
        println!("[TEST] ========================================");
        println!("[TEST] ATTEMPTING FILE DOWNLOAD");
        println!("[TEST] ========================================\n");

        // Parse query to extract expected title
        let (expected_artist, expected_title) = parse_query(query);
        let title_to_match = expected_title.unwrap_or_default();

        println!("[SELECTION] Looking for title: '{}'", title_to_match);
        if let Some(ref artist) = expected_artist {
            println!("[SELECTION] Artist: '{}'", artist);
        }

        // Count available peers (with free slots)
        let free_peers: Vec<_> = all_results.iter().filter(|r| r.slot_free).collect();
        let busy_peers = all_results.len() - free_peers.len();
        println!("[SELECTION] Peers with free slots: {} (skipping {} busy peers)", free_peers.len(), busy_peers);

        // Find best matching file using scoring system
        // Only consider peers with free upload slots
        let mut best_file: Option<(&SearchResult, &String, u64, f64)> = None;

        for result in &all_results {
            // Skip users without free slots
            if !result.slot_free {
                continue;
            }

            for (filename, size, bitrate) in &result.files {
                let lower = filename.to_lowercase();
                let is_audio = lower.ends_with(".mp3") || lower.ends_with(".flac")
                    || lower.ends_with(".wav") || lower.ends_with(".ogg");

                if !is_audio {
                    continue;
                }

                let mut score = score_file(filename, *size, *bitrate, &title_to_match);
                let title_score = score_title_match(filename, &title_to_match);

                // Add speed bonus (0-30 points based on upload speed)
                // 1 MB/s (1024 KB/s) = full bonus, scale linearly
                let speed_kbps = result.avg_speed as f64 / 1024.0;
                let speed_bonus = (speed_kbps / 1024.0).min(1.0) * 30.0;
                score += speed_bonus;

                // Small penalty for longer queues (0-5 points)
                let queue_penalty = (result.queue_length as f64 / 10.0).min(5.0);
                score -= queue_penalty;

                // Only consider files with some title match (base score > 50 means title_score > 0.5)
                if score > 50.0 {
                    println!("[SELECTION] Candidate: {} from '{}' (score: {:.1}, title: {:.2}, speed: {:.0} KB/s)",
                        filename.rsplit(['/', '\\']).next().unwrap_or(filename),
                        result.peer_username,
                        score,
                        title_score,
                        speed_kbps);

                    if best_file.is_none() || score > best_file.unwrap().3 {
                        best_file = Some((result, filename, *size, score));
                    }
                }
            }
        }

        // If no good title match from free peers, fall back to any audio file (with warning)
        if best_file.is_none() {
            println!("[SELECTION] WARNING: No good title match found from free peers, falling back to any audio file");
            for result in &all_results {
                // Still prefer free peers in fallback
                if !result.slot_free && free_peers.len() > 0 {
                    continue;
                }

                for (filename, size, _) in &result.files {
                    let lower = filename.to_lowercase();
                    let is_audio = lower.ends_with(".mp3") || lower.ends_with(".flac");
                    let size_mb = *size as f64 / 1_048_576.0;

                    if is_audio && size_mb > 1.0 && size_mb < 50.0 {
                        if best_file.is_none() {
                            best_file = Some((result, filename, *size, 0.0));
                        }
                    }
                }
            }
        }

        if let Some((result, filename, size, score)) = best_file {
            let speed_kbps = result.avg_speed as f64 / 1024.0;
            println!("[DOWNLOAD] Selected: {} from '{}' (score: {:.1}, speed: {:.0} KB/s)",
                filename.rsplit(['/', '\\']).next().unwrap_or(filename),
                result.peer_username,
                score,
                speed_kbps);
            println!("[DOWNLOAD] Full path: {}", filename);
            println!("[DOWNLOAD] Size: {:.2} MB", size as f64 / 1_048_576.0);

            // Get the peer stream we saved
            if let Some(peer_stream_arc) = &result.peer_stream {
                let mut peer_stream = peer_stream_arc.lock().await;

                // Negotiate download
                match negotiate_download(&mut peer_stream, filename).await {
                    Ok((filesize, transfer_token)) => {
                        // Now download with both paths available
                        drop(peer_stream); // Release lock before download

                        match download_file(
                            &mut stream,
                            &result.peer_ip,
                            result.peer_port,
                            &result.peer_username,
                            &username,
                            filename,
                            filesize,
                            transfer_token,
                            &mut file_rx,
                        ).await {
                            Ok(bytes) => {
                                println!("\n[TEST] ========================================");
                                println!("[TEST] DOWNLOAD SUCCESSFUL!");
                                println!("[TEST] Downloaded {} bytes", bytes);
                                println!("[TEST] ========================================\n");
                            }
                            Err(e) => {
                                println!("\n[TEST] ========================================");
                                println!("[TEST] DOWNLOAD FAILED: {}", e);
                                println!("[TEST] ========================================\n");
                            }
                        }
                    }
                    Err(e) => {
                        println!("[DOWNLOAD] Negotiation failed: {}", e);
                    }
                }
            }
        } else {
            println!("[TEST] No suitable audio files found");
        }
    }

    // Cleanup
    listener_handle.abort();
    let _ = rinse_backend::services::upnp::cleanup_upnp().await;

    Ok(())
}

