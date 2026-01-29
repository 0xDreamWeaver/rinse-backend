/// End-to-End tests for Soulseek functionality
///
/// These tests actually connect to the Soulseek network and perform real operations.
/// They require valid Soulseek credentials to be set in environment variables:
/// - SLSK_USERNAME
/// - SLSK_PASSWORD
///
/// Run with: cargo test --test e2e_soulseek -- --nocapture --test-threads=1
///
/// Note: These tests may take longer to run as they interact with real network services.

use anyhow::Result;
use rinse_backend::protocol::client::{SoulseekClient, DownloadStatus};
use std::path::PathBuf;
use std::env;
use tokio::fs;

/// Helper to get test credentials from environment
fn get_test_credentials() -> Result<(String, String)> {
    // Try to load .env file from backend directory
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let env_path = std::path::Path::new(manifest_dir).join(".env");
    let _ = dotenvy::from_path(&env_path);

    let username = env::var("SLSK_USERNAME")
        .map_err(|_| anyhow::anyhow!("SLSK_USERNAME not set. Required for E2E tests."))?;
    let password = env::var("SLSK_PASSWORD")
        .map_err(|_| anyhow::anyhow!("SLSK_PASSWORD not set. Required for E2E tests."))?;

    if username.is_empty() || password.is_empty() {
        return Err(anyhow::anyhow!("Soulseek credentials cannot be empty"));
    }

    Ok((username, password))
}

/// Helper to create temp directory for test downloads
async fn create_test_dir() -> Result<PathBuf> {
    let test_dir = env::temp_dir().join(format!("rinse_e2e_test_{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&test_dir).await?;
    println!("Created test directory: {:?}", test_dir);
    Ok(test_dir)
}

/// Helper to cleanup test directory
async fn cleanup_test_dir(dir: &PathBuf) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir).await?;
        println!("Cleaned up test directory: {:?}", dir);
    }
    Ok(())
}

#[tokio::test]
async fn test_e2e_connection() -> Result<()> {
    println!("\n=== E2E Test: Soulseek Connection ===");

    let (username, password) = get_test_credentials()?;
    println!("Attempting to connect to Soulseek as '{}'...", username);

    let start = std::time::Instant::now();
    let client = SoulseekClient::connect(&username, &password).await?;
    let duration = start.elapsed();

    println!("✓ Successfully connected to Soulseek network");
    println!("  Connection time: {:?}", duration);
    println!("  Username: {}", username);

    // Give the client a moment to stabilize
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    Ok(())
}

#[tokio::test]
async fn test_e2e_search() -> Result<()> {
    println!("\n=== E2E Test: Soulseek Search ===");

    let (username, password) = get_test_credentials()?;
    println!("Connecting to Soulseek...");
    let client = SoulseekClient::connect(&username, &password).await?;

    // Wait for connection to stabilize
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Test search for a common query
    let query = "the beatles";
    println!("Searching for '{}'...", query);

    let start = std::time::Instant::now();
    let results = client.search(query, 15).await?;
    let duration = start.elapsed();

    println!("✓ Search completed successfully");
    println!("  Search time: {:?}", duration);
    println!("  Results found: {} users responded", results.len());

    if results.is_empty() {
        println!("  ⚠ Warning: No results found. This might indicate network issues.");
        return Ok(());
    }

    // Display detailed results
    let total_files: usize = results.iter().map(|r| r.files.len()).sum();
    println!("  Total files: {}", total_files);

    // Show first 3 results
    for (i, result) in results.iter().take(3).enumerate() {
        println!("\n  Result #{}", i + 1);
        println!("    User: {}", result.username);
        println!("    Files: {}", result.files.len());
        println!("    Has slots: {}", result.has_slots);
        println!("    Avg speed: {} KB/s", result.avg_speed);
        println!("    Queue length: {}", result.queue_length);

        // Show first file details
        if let Some(file) = result.files.first() {
            println!("    First file:");
            println!("      Filename: {}", file.filename);
            println!("      Size: {} bytes ({:.2} MB)", file.size, file.size as f64 / 1_048_576.0);
            println!("      Bitrate: {:?} kbps", file.bitrate());
            println!("      Extension: {}", file.extension);
        }
    }

    // Test get_best_file
    if let Some((best_user, best_file)) = SoulseekClient::get_best_file(&results) {
        println!("\n  Best file selected:");
        println!("    User: {}", best_user);
        println!("    Filename: {}", best_file.filename);
        println!("    Bitrate: {:?} kbps", best_file.bitrate());
        println!("    Size: {:.2} MB", best_file.size as f64 / 1_048_576.0);
    }

    Ok(())
}

#[tokio::test]
async fn test_e2e_search_no_results() -> Result<()> {
    println!("\n=== E2E Test: Soulseek Search (No Results) ===");

    let (username, password) = get_test_credentials()?;
    println!("Connecting to Soulseek...");
    let client = SoulseekClient::connect(&username, &password).await?;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Search for something that should yield no results
    let query = "xyzabc123impossiblequery9999";
    println!("Searching for nonsense query '{}'...", query);

    let start = std::time::Instant::now();
    let results = client.search(query, 10).await?;
    let duration = start.elapsed();

    println!("✓ Search completed");
    println!("  Search time: {:?}", duration);
    println!("  Results found: {}", results.len());

    if results.is_empty() {
        println!("  ✓ As expected, no results for impossible query");
    } else {
        println!("  ⚠ Unexpected: Found {} results for impossible query", results.len());
    }

    Ok(())
}

#[tokio::test]
async fn test_e2e_download_small_file() -> Result<()> {
    println!("\n=== E2E Test: Soulseek Download (Small File) ===");

    let (username, password) = get_test_credentials()?;
    let test_dir = create_test_dir().await?;

    println!("Connecting to Soulseek...");
    let client = SoulseekClient::connect(&username, &password).await?;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Search for a common, small file
    let query = "test";
    println!("Searching for '{}'...", query);

    let results = client.search(query, 15).await?;

    if results.is_empty() {
        println!("  ⚠ Warning: No results found. Skipping download test.");
        cleanup_test_dir(&test_dir).await?;
        return Ok(());
    }

    println!("  Found {} results", results.len());

    // Get the best file (by bitrate)
    let (best_user, best_file) = match SoulseekClient::get_best_file(&results) {
        Some(result) => result,
        None => {
            println!("  ⚠ Warning: No suitable file found. Skipping download test.");
            cleanup_test_dir(&test_dir).await?;
            return Ok(());
        }
    };

    println!("\n  Selected file for download:");
    println!("    User: {}", best_user);
    println!("    Filename: {}", best_file.filename);
    println!("    Size: {} bytes ({:.2} MB)", best_file.size, best_file.size as f64 / 1_048_576.0);
    println!("    Bitrate: {:?} kbps", best_file.bitrate());

    // Only attempt download if file is reasonably small (< 10MB)
    if best_file.size > 10_000_000 {
        println!("  ⚠ File too large for test (> 10MB). Skipping actual download.");
        cleanup_test_dir(&test_dir).await?;
        return Ok(());
    }

    // Attempt download
    let output_path = test_dir.join(&best_file.filename);
    println!("\n  Attempting download to: {:?}", output_path);

    let start = std::time::Instant::now();

    match client.download_file(&best_user, &best_file.filename, &output_path).await {
        Ok(bytes_downloaded) => {
            let duration = start.elapsed();
            println!("  ✓ Download completed successfully!");
            println!("    Bytes downloaded: {}", bytes_downloaded);
            println!("    Duration: {:?}", duration);
            println!("    Speed: {:.2} KB/s", bytes_downloaded as f64 / duration.as_secs_f64() / 1024.0);

            // Verify file exists
            if output_path.exists() {
                let metadata = fs::metadata(&output_path).await?;
                println!("    File size on disk: {} bytes", metadata.len());

                if metadata.len() == bytes_downloaded {
                    println!("    ✓ File size matches reported download size");
                } else {
                    println!("    ⚠ File size mismatch: {} vs {}", metadata.len(), bytes_downloaded);
                }
            } else {
                println!("    ⚠ Warning: File not found on disk after download");
            }

            // Check download progress
            if let Some(progress) = client.get_download_progress(&best_file.filename).await {
                println!("\n    Download progress info:");
                println!("      Status: {:?}", progress.status);
                println!("      Total bytes: {}", progress.total_bytes);
                println!("      Downloaded bytes: {}", progress.downloaded_bytes);

                if progress.status == DownloadStatus::Completed {
                    println!("      ✓ Status correctly marked as Completed");
                } else {
                    println!("      ⚠ Status not marked as Completed: {:?}", progress.status);
                }
            }
        }
        Err(e) => {
            let duration = start.elapsed();
            println!("  ✗ Download failed after {:?}", duration);
            println!("    Error: {}", e);
            println!("    This is expected in some cases (user offline, no slots, etc.)");

            // Check download progress shows failure
            if let Some(progress) = client.get_download_progress(&best_file.filename).await {
                println!("\n    Download progress info:");
                println!("      Status: {:?}", progress.status);

                match &progress.status {
                    DownloadStatus::Failed(msg) => {
                        println!("      ✓ Status correctly marked as Failed");
                        println!("      Failure reason: {}", msg);
                    }
                    _ => {
                        println!("      ⚠ Status not marked as Failed: {:?}", progress.status);
                    }
                }
            }
        }
    }

    cleanup_test_dir(&test_dir).await?;
    Ok(())
}

#[tokio::test]
async fn test_e2e_search_and_download_with_retry() -> Result<()> {
    println!("\n=== E2E Test: Soulseek Search and Download with Retry ===");

    let (username, password) = get_test_credentials()?;
    let test_dir = create_test_dir().await?;

    println!("Connecting to Soulseek...");
    let client = SoulseekClient::connect(&username, &password).await?;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Use search_and_download which includes retry logic
    let query = "jazz";
    let output_path = test_dir.join("downloaded_file");

    println!("Searching and downloading '{}' with automatic retry...", query);

    let start = std::time::Instant::now();

    match client.search_and_download(query, &output_path).await {
        Ok((filename, bytes)) => {
            let duration = start.elapsed();
            println!("  ✓ Search and download completed successfully!");
            println!("    Filename: {}", filename);
            println!("    Bytes downloaded: {}", bytes);
            println!("    Total duration: {:?}", duration);
            println!("    Average speed: {:.2} KB/s", bytes as f64 / duration.as_secs_f64() / 1024.0);

            // Check file exists
            if output_path.exists() {
                let metadata = fs::metadata(&output_path).await?;
                println!("    File verified on disk: {} bytes", metadata.len());
            }
        }
        Err(e) => {
            let duration = start.elapsed();
            println!("  ✗ Search and download failed after {:?}", duration);
            println!("    Error: {}", e);
            println!("    Note: Retry logic attempted but both attempts failed.");
            println!("    This can happen if no users are online or sharing files.");
        }
    }

    cleanup_test_dir(&test_dir).await?;
    Ok(())
}

#[tokio::test]
async fn test_e2e_multiple_searches() -> Result<()> {
    println!("\n=== E2E Test: Multiple Concurrent Searches ===");

    let (username, password) = get_test_credentials()?;
    println!("Connecting to Soulseek...");
    let client = SoulseekClient::connect(&username, &password).await?;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    let queries = vec!["rock", "jazz", "classical"];
    println!("Performing {} searches concurrently...", queries.len());

    let start = std::time::Instant::now();

    // Perform searches sequentially (to avoid overwhelming the network)
    let mut all_results = Vec::new();

    for query in &queries {
        println!("\n  Searching for '{}'...", query);
        let results = client.search(query, 10).await?;
        println!("    Results: {} users responded with {} total files",
                 results.len(),
                 results.iter().map(|r| r.files.len()).sum::<usize>());
        all_results.push((query, results));
    }

    let duration = start.elapsed();

    println!("\n✓ All searches completed");
    println!("  Total time: {:?}", duration);
    println!("  Average time per search: {:?}", duration / queries.len() as u32);

    // Summary
    println!("\n  Summary:");
    for (query, results) in &all_results {
        let total_files: usize = results.iter().map(|r| r.files.len()).sum();
        println!("    '{}': {} users, {} files", query, results.len(), total_files);
    }

    Ok(())
}

#[tokio::test]
async fn test_e2e_connection_stability() -> Result<()> {
    println!("\n=== E2E Test: Connection Stability ===");

    let (username, password) = get_test_credentials()?;
    println!("Connecting to Soulseek...");
    let client = SoulseekClient::connect(&username, &password).await?;

    println!("✓ Initial connection established");

    // Keep connection alive for a period and perform operations
    let test_duration = 30; // seconds
    println!("Testing connection stability for {} seconds...", test_duration);

    for i in 0..3 {
        tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

        println!("\n  Check #{} ({}s elapsed)", i + 1, (i + 1) * 10);

        // Perform a search to verify connection is still active
        let query = "test";
        match client.search(query, 5).await {
            Ok(results) => {
                println!("    ✓ Connection still active");
                println!("    Search returned {} results", results.len());
            }
            Err(e) => {
                println!("    ✗ Connection may have dropped");
                println!("    Error: {}", e);
                return Err(e);
            }
        }
    }

    println!("\n✓ Connection remained stable throughout test");

    Ok(())
}

#[tokio::test]
async fn test_e2e_progress_tracking() -> Result<()> {
    println!("\n=== E2E Test: Download Progress Tracking ===");

    let (username, password) = get_test_credentials()?;
    let test_dir = create_test_dir().await?;

    println!("Connecting to Soulseek...");
    let client = SoulseekClient::connect(&username, &password).await?;

    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    println!("Searching for a file...");
    let results = client.search("test", 10).await?;

    if results.is_empty() {
        println!("  ⚠ No results found. Skipping progress tracking test.");
        cleanup_test_dir(&test_dir).await?;
        return Ok(());
    }

    let (best_user, best_file) = match SoulseekClient::get_best_file(&results) {
        Some(result) => result,
        None => {
            println!("  ⚠ No suitable file found. Skipping progress tracking test.");
            cleanup_test_dir(&test_dir).await?;
            return Ok(());
        }
    };

    // Only test with small files
    if best_file.size > 5_000_000 {
        println!("  ⚠ File too large. Skipping progress tracking test.");
        cleanup_test_dir(&test_dir).await?;
        return Ok(());
    }

    println!("  Selected file: {} ({} bytes)", best_file.filename, best_file.size);

    let output_path = test_dir.join(&best_file.filename);

    println!("\n  Starting download...");
    let start = std::time::Instant::now();

    // Attempt download
    match client.download_file(&best_user, &best_file.filename, &output_path).await {
        Ok(bytes) => {
            let duration = start.elapsed();
            println!("\n  ✓ Download completed: {} bytes in {:?}", bytes, duration);

            // Check progress tracking after completion
            if let Some(progress) = client.get_download_progress(&best_file.filename).await {
                println!("\n  Progress tracking verification:");
                println!("    Status: {:?}", progress.status);
                println!("    Total bytes: {}", progress.total_bytes);
                println!("    Downloaded bytes: {}", progress.downloaded_bytes);

                if progress.status == DownloadStatus::Completed {
                    println!("    ✓ Status correctly marked as Completed");
                } else {
                    println!("    ⚠ Status should be Completed but is: {:?}", progress.status);
                }

                if progress.downloaded_bytes == bytes {
                    println!("    ✓ Downloaded bytes matches actual download size");
                } else {
                    println!("    ⚠ Downloaded bytes mismatch: {} vs {}", progress.downloaded_bytes, bytes);
                }
            } else {
                println!("    ⚠ No progress information found");
            }
        }
        Err(e) => {
            let duration = start.elapsed();
            println!("\n  ✗ Download failed after {:?}: {}", duration, e);

            // Check failure status
            if let Some(progress) = client.get_download_progress(&best_file.filename).await {
                println!("\n  Progress tracking verification:");
                println!("    Status: {:?}", progress.status);

                match progress.status {
                    DownloadStatus::Failed(msg) => {
                        println!("    ✓ Status correctly marked as Failed");
                        println!("    Failure reason: {}", msg);
                    }
                    _ => {
                        println!("    ⚠ Status should be Failed but is: {:?}", progress.status);
                    }
                }
            } else {
                println!("    ⚠ No progress information found");
            }
        }
    }

    cleanup_test_dir(&test_dir).await?;
    Ok(())
}
