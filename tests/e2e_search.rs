//! E2E test for Soulseek search and download functionality
//!
//! Run with: cargo test --test e2e_search -- --nocapture
//!
//! This test uses the backend's protocol implementation to:
//! 1. Connect to the database and check for existing downloads
//! 2. Connect to the Soulseek server
//! 3. Search for a file
//! 4. Connect to peers and receive search results
//! 5. Negotiate and download a file
//! 6. Track the download in the database

use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpStream, TcpListener};
use tokio::sync::{mpsc, Mutex};
use bytes::{Buf, BufMut, BytesMut};

// Import from backend
use rinse_backend::protocol::{
    // File selection
    parse_query, score_title_match, score_file, is_audio_file,
    // Peer protocol
    FileConnection, IncomingConnection, handle_incoming_connection,
    connect_to_peer_for_results, receive_search_results_from_peer,
    queue_upload, wait_for_transfer_request,  // Modern method
    negotiate_download, download_file as peer_download_file,
    NegotiationResult, ParsedSearchResponse,
    // Message helpers
    peer_encode_string as encode_string, send_message, read_message,
    // Core types
    decode_string,
};
use rinse_backend::services::{upnp, FuzzyMatcher};
use rinse_backend::db::Database;

/// Incoming search result from listener
struct IncomingSearchResult {
    peer_username: String,
    response: ParsedSearchResponse,
    stream: TcpStream,
}

const SERVER_HOST: &str = "server.slsknet.org";
const SERVER_PORT: u16 = 2242;
const LISTEN_PORT: u16 = 2234;

// ============================================================================
// Server protocol (keeping these local as they're test-specific wrappers)
// ============================================================================

fn load_credentials() -> (String, String) {
    dotenvy::dotenv().ok();
    let username = std::env::var("SLSK_USERNAME").expect("SLSK_USERNAME not set");
    let password = std::env::var("SLSK_PASSWORD").expect("SLSK_PASSWORD not set");
    (username, password)
}

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
        let reason = decode_string(&mut buf)?;
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

/// Request peer address from server (GetPeerAddress - code 3)
async fn get_peer_address(stream: &mut TcpStream, peer_username: &str) -> anyhow::Result<(String, u32)> {
    // Send GetPeerAddress request
    let mut data = BytesMut::new();
    encode_string(&mut data, peer_username);
    send_message(stream, 3, &data).await?;

    // Wait for response (with timeout)
    let start = std::time::Instant::now();
    let timeout = Duration::from_secs(10);

    while start.elapsed() < timeout {
        match tokio::time::timeout(Duration::from_millis(500), read_message(stream)).await {
            Ok(Ok((3, response_data))) => {
                // Parse GetPeerAddress response
                let mut buf = BytesMut::from(&response_data[..]);
                let _response_username = decode_string(&mut buf)?;
                // IP is 4 bytes in big-endian order (network byte order)
                let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                let port = buf.get_u32_le();

                if port > 0 {
                    println!("[SERVER] Got peer address for '{}': {}:{}", peer_username, ip, port);
                    return Ok((ip, port));
                } else {
                    anyhow::bail!("Peer '{}' is offline or not connectable", peer_username);
                }
            }
            Ok(Ok(_)) => {
                // Other message, continue waiting
            }
            Ok(Err(e)) => {
                anyhow::bail!("Error reading server response: {}", e);
            }
            Err(_) => {
                // Timeout on this iteration, continue
            }
        }
    }

    anyhow::bail!("Timeout waiting for peer address response")
}

// ============================================================================
// Main test
// ============================================================================

#[tokio::test]
async fn test_search_raw() -> anyhow::Result<()> {
    let (username, password) = load_credentials();

    println!("\n=== SOULSEEK SEARCH & DOWNLOAD E2E TEST ===\n");

    // Initialize database
    println!("[DB] Initializing database connection...");
    let db_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("rinse.db");
    let db_url = format!("sqlite:{}?mode=rwc", db_path.display());
    let db = Database::new(&db_url).await?;
    println!("[DB] Connected to database: {}", db_path.display());

    // Define our search query - use a unique song each time
    let query = "Chemical Brothers - Block Rockin Beats";

    // Check if we already have this file downloaded
    println!("\n[DB] ========================================");
    println!("[DB] CHECKING FOR EXISTING DOWNLOADS");
    println!("[DB] ========================================");
    println!("[DB] Query: '{}'", query);

    // First check exact match by query
    if let Some(existing) = db.find_completed_item_by_query(query).await? {
        println!("[DB] FOUND EXACT MATCH!");
        println!("[DB] Filename: {}", existing.filename);
        println!("[DB] Path: {}", existing.file_path);
        println!("[DB] Size: {:.2} MB", existing.file_size as f64 / 1_048_576.0);
        println!("[DB] Downloaded from: {}", existing.source_username);
        println!("[DB] ========================================\n");
        println!("[TEST] File already exists, skipping download.");

        // Verify file exists on disk
        if std::path::Path::new(&existing.file_path).exists() {
            println!("[TEST] Verified: File exists at {}", existing.file_path);
            return Ok(());
        } else {
            println!("[TEST] WARNING: Database record exists but file not found on disk");
            println!("[TEST] Proceeding with new download...");
        }
    }

    // Check for fuzzy matches using completed items
    let completed_items = db.get_completed_items().await?;
    if !completed_items.is_empty() {
        let fuzzy = FuzzyMatcher::new(70); // 70% similarity threshold
        if let Some(similar) = fuzzy.find_duplicate(query, &completed_items) {
            println!("[DB] FOUND SIMILAR MATCH (fuzzy)!");
            println!("[DB] Original query: {}", similar.original_query);
            println!("[DB] Filename: {}", similar.filename);
            println!("[DB] Path: {}", similar.file_path);

            // For now, we'll still download to get the exact track
            // In production, you might want to ask the user
            println!("[DB] Proceeding with download for exact match...");
        }
    }

    println!("[DB] No existing download found, proceeding with search...\n");

    // Initialize UPnP
    println!("[UPnP] Initializing port forwarding...");
    match upnp::init_upnp().await {
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

    // Channels for connections from listener
    let (file_tx, mut file_rx) = mpsc::channel::<FileConnection>(10);
    let (search_tx, mut search_rx) = mpsc::channel::<IncomingSearchResult>(50);

    // Start listener for incoming connections (handles both P and F types)
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
                            let file_tx = file_tx.clone();
                            let search_tx = search_tx.clone();
                            let username = our_username_clone.clone();

                            tokio::spawn(async move {
                                let start = std::time::Instant::now();
                                match handle_incoming_connection(stream, &username).await {
                                    Ok(IncomingConnection::File(conn)) => {
                                        println!("[LISTENER] F connection from {}, token={} (handshake took {:?})",
                                            addr, conn.transfer_token, start.elapsed());
                                        let send_start = std::time::Instant::now();
                                        match file_tx.send(conn).await {
                                            Ok(_) => println!("[LISTENER] F connection sent to channel (took {:?})", send_start.elapsed()),
                                            Err(e) => println!("[LISTENER] Failed to send F connection: {}", e),
                                        }
                                    }
                                    Ok(IncomingConnection::Peer(mut stream, peer_username, _token)) => {
                                        println!("[LISTENER] P connection from {} ({})", peer_username, addr);
                                        // Receive search results from this peer
                                        match receive_search_results_from_peer(&mut stream).await {
                                            Ok(response) => {
                                                let speed_kbps = response.avg_speed as f64 / 1024.0;
                                                let free_str = if response.slot_free { "FREE" } else { "BUSY" };
                                                println!("[LISTENER] Got {} files from '{}' ({}, {:.0} KB/s)",
                                                    response.files.len(), peer_username, free_str, speed_kbps);
                                                let _ = search_tx.send(IncomingSearchResult {
                                                    peer_username,
                                                    response,
                                                    stream,
                                                }).await;
                                            }
                                            Err(e) => {
                                                println!("[LISTENER] Failed to get results from '{}': {}", peer_username, e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        // Silent - many connections fail
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
        slot_free: bool,
        avg_speed: u32,
        queue_length: u32,
    }

    let mut all_results: Vec<SearchResult> = Vec::new();
    let mut connect_to_peer_count = 0;
    let mut incoming_results_count = 0;

    let start = std::time::Instant::now();
    let search_timeout = Duration::from_secs(30);

    println!("[TEST] Listening for search results...\n");

    while start.elapsed() < search_timeout {
        tokio::select! {
            // Check for incoming results from listener (peers connected to us)
            Some(incoming) = search_rx.recv() => {
                incoming_results_count += 1;
                let response = incoming.response;

                for (i, (filename, size, bitrate)) in response.files.iter().take(3).enumerate() {
                    let br = bitrate.map(|b| format!("{}kbps", b)).unwrap_or_else(|| "?".to_string());
                    println!("  {}. {} ({:.1}MB, {})", i + 1, filename, *size as f64 / 1_048_576.0, br);
                }
                if response.files.len() > 3 {
                    println!("  ... and {} more", response.files.len() - 3);
                }

                all_results.push(SearchResult {
                    peer_username: incoming.peer_username.clone(),
                    peer_ip: String::new(), // Not available for incoming
                    peer_port: 0,
                    peer_token: 0,
                    files: response.files,
                    peer_stream: Some(Arc::new(Mutex::new(incoming.stream))),
                    slot_free: response.slot_free,
                    avg_speed: response.avg_speed,
                    queue_length: response.queue_length,
                });
            }

            // Check for server messages (ConnectToPeer requests)
            result = tokio::time::timeout(Duration::from_millis(100), read_message(&mut stream)) => {
                match result {
                    Ok(Ok((18, data))) => {
                        // ConnectToPeer - peer has results
                        connect_to_peer_count += 1;
                        let mut buf = BytesMut::from(&data[..]);
                        let peer_username = decode_string(&mut buf)?;
                        let conn_type = decode_string(&mut buf)?;
                        let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                        let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);
                        let port = buf.get_u32_le();
                        let peer_token = buf.get_u32_le();

                        if conn_type == "P" && connect_to_peer_count <= 30 {
                            println!("[PEER {:2}] {} at {}:{}", connect_to_peer_count, peer_username, ip, port);

                            // Try to connect and get results using backend function
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
                    Ok(Ok((_code, _))) => {
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
        }
    }

    println!("\n[TEST] ========================================");
    println!("[TEST] SEARCH RESULTS SUMMARY");
    println!("[TEST] ========================================");
    println!("[TEST] ConnectToPeer messages received: {}", connect_to_peer_count);
    println!("[TEST] Incoming P connections (peers connected to us): {}", incoming_results_count);
    println!("[TEST] Total successful results: {}", all_results.len());
    println!("[TEST] Total files: {}", all_results.iter().map(|r| r.files.len()).sum::<usize>());
    println!("[TEST] ========================================\n");

    // Try to download a file
    if !all_results.is_empty() {
        println!("[TEST] ========================================");
        println!("[TEST] ATTEMPTING FILE DOWNLOAD");
        println!("[TEST] ========================================\n");

        // Parse query to extract expected title using backend function
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

        // Find best matching file using backend scoring functions
        let mut best_file: Option<(&SearchResult, &String, u64, Option<u32>, f64)> = None;

        for result in &all_results {
            // Skip users without free slots
            if !result.slot_free {
                continue;
            }

            for (filename, size, bitrate) in &result.files {
                if !is_audio_file(filename) {
                    continue;
                }

                let mut score = score_file(filename, *size, *bitrate, &title_to_match);
                let title_score = score_title_match(filename, &title_to_match);

                // Add speed bonus (0-30 points based on upload speed)
                let speed_kbps = result.avg_speed as f64 / 1024.0;
                let speed_bonus = (speed_kbps / 1024.0).min(1.0) * 30.0;
                score += speed_bonus;

                // Small penalty for longer queues (0-5 points)
                let queue_penalty = (result.queue_length as f64 / 10.0).min(5.0);
                score -= queue_penalty;

                // IMPORTANT: Big bonus for peers with known addresses (from ConnectToPeer)
                // These are much more likely to succeed for file transfers because:
                // 1. We can do proactive connections if needed
                // 2. The connection path is already proven (they replied to our connect)
                let has_address = !result.peer_ip.is_empty() && result.peer_port > 0;
                if has_address {
                    score += 50.0; // Significant bonus
                }

                // Only consider files with some title match
                if score > 50.0 {
                    let addr_indicator = if has_address { "✓ADDR" } else { "no-addr" };
                    println!("[SELECTION] Candidate: {} from '{}' (score: {:.1}, title: {:.2}, speed: {:.0} KB/s, {})",
                        filename.rsplit(['/', '\\']).next().unwrap_or(filename),
                        result.peer_username,
                        score,
                        title_score,
                        speed_kbps,
                        addr_indicator);

                    if best_file.is_none() || score > best_file.unwrap().4 {
                        best_file = Some((result, filename, *size, *bitrate, score));
                    }
                }
            }
        }

        // If no good title match from free peers, fall back to any audio file
        if best_file.is_none() {
            println!("[SELECTION] WARNING: No good title match found from free peers, falling back to any audio file");
            for result in &all_results {
                if !result.slot_free && !free_peers.is_empty() {
                    continue;
                }

                for (filename, size, bitrate) in &result.files {
                    if !is_audio_file(filename) {
                        continue;
                    }
                    let size_mb = *size as f64 / 1_048_576.0;
                    if size_mb >= 1.0 && size_mb <= 50.0 {
                        if best_file.is_none() {
                            best_file = Some((result, filename, *size, *bitrate, 0.0));
                        }
                    }
                }
            }
        }

        if let Some((result, filename, size, bitrate, score)) = best_file {
            let speed_kbps = result.avg_speed as f64 / 1024.0;
            let output_filename = filename.split(['/', '\\']).last().unwrap_or("download");
            let extension = output_filename.rsplit('.').next().unwrap_or("mp3");

            println!("[DOWNLOAD] Selected: {} from '{}' (score: {:.1}, speed: {:.0} KB/s)",
                output_filename,
                result.peer_username,
                score,
                speed_kbps);
            println!("[DOWNLOAD] Full path: {}", filename);
            println!("[DOWNLOAD] Size: {:.2} MB", size as f64 / 1_048_576.0);

            // Save to storage directory
            let storage_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("storage");
            tokio::fs::create_dir_all(&storage_dir).await?;
            let output_path = storage_dir.join(output_filename);

            // Create database record BEFORE starting download
            println!("[DB] Creating download record...");
            let item = db.create_item(
                output_filename,
                query,
                output_path.to_str().unwrap_or(""),
                size as i64,
                bitrate.map(|b| b as i32),
                None, // duration - we could extract this later
                extension,
                &result.peer_username,
            ).await?;
            println!("[DB] Created item record with ID: {}", item.id);

            // Update status to downloading
            db.update_item_status(item.id, "downloading", 0.0, None).await?;

            // Get the peer stream we saved
            if let Some(peer_stream_arc) = &result.peer_stream {
                let mut peer_stream = peer_stream_arc.lock().await;

                // MODERN METHOD: Use QueueUpload instead of legacy TransferRequest(direction=0)
                // This is what Nicotine+ 3.0.3+ and official clients use
                println!("[DOWNLOAD] Using modern QueueUpload method...");

                // Step 1: Send QueueUpload - tells peer we want to download this file
                if let Err(e) = queue_upload(&mut peer_stream, filename).await {
                    println!("[DOWNLOAD] Failed to send QueueUpload: {}", e);
                    db.update_item_status(item.id, "failed", 0.0, Some(&e.to_string())).await?;
                } else {
                    println!("[DOWNLOAD] QueueUpload sent, waiting for peer's TransferRequest...");

                    // Step 2: Wait for peer to send TransferRequest (direction=1) when ready
                    // This may take a while if we're queued
                    match wait_for_transfer_request(&mut peer_stream, filename, 60).await {
                        Ok(NegotiationResult::Allowed { filesize, transfer_token }) => {
                            println!("[DOWNLOAD] Transfer ALLOWED: size={} bytes, token={}", filesize, transfer_token);

                            // Release peer stream lock before download
                            drop(peer_stream);

                            // Get peer address - either from saved data or request from server
                            let (peer_ip, peer_port) = if result.peer_ip.is_empty() || result.peer_port == 0 {
                                println!("[DOWNLOAD] No peer address cached, requesting from server...");
                                match get_peer_address(&mut stream, &result.peer_username).await {
                                    Ok((ip, port)) => (ip, port),
                                    Err(e) => {
                                        println!("[DOWNLOAD] Could not get peer address: {}", e);
                                        // Continue with empty address - will rely on incoming F connection
                                        (String::new(), 0)
                                    }
                                }
                            } else {
                                (result.peer_ip.clone(), result.peer_port)
                            };

                            match peer_download_file(
                                &mut stream,
                                &peer_ip,
                                peer_port,
                                &result.peer_username,
                                &username,
                                filename,
                                filesize,
                                transfer_token,
                                &mut file_rx,
                                &output_path,
                            ).await {
                                Ok(bytes) => {
                                    // Update database with completion
                                    db.update_item_status(item.id, "completed", 1.0, None).await?;

                                    println!("\n[TEST] ========================================");
                                    println!("[TEST] DOWNLOAD SUCCESSFUL!");
                                    println!("[TEST] Downloaded {} bytes", bytes);
                                    println!("[DB] Updated item {} status to 'completed'", item.id);
                                    println!("[TEST] ========================================\n");
                                }
                                Err(e) => {
                                    // Update database with failure
                                    db.update_item_status(item.id, "failed", 0.0, Some(&e.to_string())).await?;

                                    println!("\n[TEST] ========================================");
                                    println!("[TEST] DOWNLOAD FAILED: {}", e);
                                    println!("[DB] Updated item {} status to 'failed'", item.id);
                                    println!("[TEST] ========================================\n");
                                }
                            }
                        }
                        Ok(NegotiationResult::Queued { transfer_token: _, position, reason }) => {
                            // Peer has queued our request - we'll receive TransferRequest later
                            println!("[DOWNLOAD] Transfer QUEUED: position={:?}, reason={}",
                                position, reason);
                            db.update_item_status(item.id, "queued", 0.0, None).await?;
                            println!("[DB] Updated item {} status to 'queued'", item.id);
                            // For now, we don't wait for queued downloads in this test
                            // In production, the listener would handle the F connection when peer is ready
                        }
                        Ok(NegotiationResult::Denied { reason }) => {
                            println!("[DOWNLOAD] Transfer DENIED: {}", reason);
                            db.update_item_status(item.id, "failed", 0.0, Some(&reason)).await?;
                            println!("[DB] Updated item {} status to 'failed'", item.id);
                        }
                        Err(e) => {
                            // Update database with failure
                            db.update_item_status(item.id, "failed", 0.0, Some(&e.to_string())).await?;
                            println!("[DOWNLOAD] Negotiation failed: {}", e);
                            println!("[DB] Updated item {} status to 'failed'", item.id);
                        }
                    }
                }
            }
        } else {
            println!("[TEST] No suitable audio files found");
        }
    }

    // Cleanup
    listener_handle.abort();
    let _ = upnp::cleanup_upnp().await;

    Ok(())
}
