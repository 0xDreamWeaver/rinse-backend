# Session Context - January 2026

This file captures the detailed context from recent development sessions for context preservation.

## Session Summary

### Completed Work

#### 1. Format Filter Feature
Added format toggle buttons (Any, MP3, FLAC, M4A, WAV) to search:

**Frontend changes:**
- `Search.tsx`: Added format toggle UI with `FORMAT_OPTIONS` array
- `lib/api.ts`: Updated `searchItem()` and `searchList()` to pass format parameter
- `types/index.ts`: Types already supported format

**Backend changes:**
- `models/mod.rs`: Added `format` field to `SearchRequest` and `SearchListRequest`
- `protocol/file_selection.rs`: Added `matches_format()` function
- `protocol/file_selection.rs`: Updated `find_best_file()` to accept and apply format filter
- `api/lists.rs`: Updated endpoint to pass format to download service

#### 2. Non-blocking Search Fix
**Problem:** Search was returning 0 results. ConnectToPeer attempts were blocking (5s timeout each), consuming the entire 10-second search window. Peers connecting to us arrived after search ended.

**Solution in `protocol/client.rs`:**
```rust
// Channel for collecting results from background ConnectToPeer tasks
let (result_tx, mut result_rx) = mpsc::channel::<(String, String, u32, TcpStream, ParsedSearchResponse)>(50);

// Spawn ConnectToPeer in background instead of blocking
tokio::spawn(async move {
    match connect_to_peer_for_results(&ip_clone, port, &peer_username_clone, &our_username, peer_token).await {
        Ok((peer_stream, response)) => {
            let _ = tx.send((peer_username_clone, ip_clone, port, peer_stream, response)).await;
        }
        Err(e) => {
            tracing::info!("[Search] Failed to get results from '{}': {}", peer_username_clone, e);
        }
    }
});
```

Added non-blocking `try_recv()` to collect results during loop and grace period after main loop for late arrivals.

#### 3. Duplicate Detection UX
**Problem:** When searching for already-downloaded tracks, backend returned existing item but didn't send WebSocket events, causing frontend to show error.

**Solution:**

Added `DuplicateFound` event to `api/ws.rs`:
```rust
/// Item already exists in library (duplicate found)
DuplicateFound {
    item_id: i64,
    filename: String,
    query: String,
},
```

Updated `services/download.rs` to broadcast event:
```rust
if let Some(existing) = self.fuzzy_matcher.find_duplicate(query, &completed_items) {
    tracing::info!("[DownloadService] Found existing completed item matching query '{}': {}", query, existing.filename);
    self.broadcast(WsEvent::DuplicateFound {
        item_id: existing.id,
        filename: existing.filename.clone(),
        query: query.to_string(),
    });
    return Ok(existing.clone());
}
```

Added frontend types in `types/index.ts`:
```typescript
export interface WsDuplicateFound {
  type: 'duplicate_found';
  item_id: number;
  filename: string;
  query: string;
}
```

Added 'duplicate' to `ActiveDownload.stage` type.

Updated `store/index.ts` to handle the event and display cyan "Already in library" message.

#### 4. Popup State Clearing
**Problem:** Download popup showed stale state from previous searches.

**Solution:** In `store/index.ts`, clear finished states when new search starts:
```typescript
case 'search_started': {
  // Clear any previous finished downloads so the new search is visible
  for (const [key, download] of downloads.entries()) {
    if (['completed', 'failed', 'duplicate'].includes(download.stage)) {
      downloads.delete(key);
    }
  }
  // ... rest of handler
}
```

## Key Architecture Patterns

### WebSocket Event Flow
```
peer.rs (download_file with progress callback)
    -> client.rs (orchestrates download, creates callback)
    -> download.rs (DownloadService wraps callback with broadcast)
    -> ws.rs (broadcast channel)
    -> Frontend WebSocket
    -> store/index.ts (handleWsEvent)
    -> Search.tsx (DownloadProgressPopup)
```

### Search Architecture
1. Server sends `ConnectToPeer` messages for peers with results
2. Each ConnectToPeer spawned in background via `tokio::spawn()`
3. Results collected via `mpsc::channel` with non-blocking `try_recv()`
4. Search loop continues processing while peers respond
5. Grace period after main loop for late arrivals
6. Incoming P connections stored in `peer_streams` HashMap for reuse

### Duplicate Detection
1. Before searching network, `DownloadService` checks existing items
2. Uses fuzzy matching via `fuzzy.rs`
3. If match found, broadcasts `DuplicateFound` and returns existing item
4. No network search performed for duplicates

## File Locations

### Critical Backend Files
- `src/protocol/client.rs` - SoulseekClient, search orchestration
- `src/protocol/peer.rs` - P/F connections, `download_file()`
- `src/protocol/file_selection.rs` - `find_best_file()`, `matches_format()`
- `src/services/download.rs` - DownloadService, duplicate detection
- `src/api/ws.rs` - WebSocket events, broadcast channel
- `src/api/items.rs` - Item endpoints
- `src/api/lists.rs` - List endpoints

### Critical Frontend Files
- `src/store/index.ts` - Zustand store, `handleWsEvent()`
- `src/pages/Search.tsx` - Search UI, `DownloadProgressPopup`
- `src/types/index.ts` - TypeScript types for WsEvent, ActiveDownload
- `src/hooks/useWebSocket.ts` - WebSocket connection
- `src/lib/api.ts` - API client functions

## Common Issues

### "Port already in use"
```bash
lsof -ti:3000 | xargs kill -9  # Backend API
lsof -ti:2234 | xargs kill -9  # Soulseek listener
```

### Search returns 0 results
Check if peers arrived AFTER timeout in logs. Look for:
- `[Search] ConnectToPeer` timing
- `[Listener] Incoming P connection` timestamps
- Verify non-blocking search is working (multiple spawns should happen quickly)

### Download stuck at "Searching..."
- Check WebSocket connection in browser dev tools
- Verify backend is broadcasting events (check logs)
- Check if duplicate detection is returning early without event

## Running the Backend

The backend should be started with:
```bash
cd backend
cargo run --bin rinse-backend
```

Or with debug logging:
```bash
RUST_LOG=debug cargo run --bin rinse-backend
```

## Next Steps / Known Issues

See CLAUDE.md for the full TODO list. High priority items:
- Handle queued downloads resume (when peer F-connects later)
- Better error messages for common failures
