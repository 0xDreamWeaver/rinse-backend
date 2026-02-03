# Rinse Backend - Project Documentation

---

## Session Update - February 3, 2026 (Latest)

### Protocol Implementation Fixes - Phase 1 & 2 Complete

Implemented four critical protocol fixes identified from Nicotine+ comparison. **Benchmarked before and after with measurable improvements.**

**Benchmark Results (Search: "Holdin' On Skrillex & Nero Remix"):**

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| Peer success rate | 1.4-3.9% | 6.9% | **2-5x improvement** |
| Handshake EOF errors | ~31 | ~10 | **3x reduction** |
| Download success | Failed (30s timeout) | Succeeded first try | - |

**Completed Fixes:**

1. **✅ TCP_NODELAY on all connections** (`peer.rs`, `client.rs`, `connection.rs`)
   - Added `stream.set_nodelay(true)?` to all 10 connection points
   - Disables Nagle's algorithm - handshake messages (5-50 bytes) now sent immediately
   - This was the highest-impact fix

2. **✅ PeerInit token always 0** (`peer.rs`)
   - Changed `send_peer_init()` to always send token=0
   - Protocol spec: "The token is always zero and ignored today"
   - We were incorrectly sending the actual token value

3. **✅ CantConnectToPeer feedback** (`peer.rs`, `client.rs`)
   - Added `send_cant_connect_to_peer()` helper function (message code 1001)
   - Added failure channel in search loop to collect failed connection attempts
   - After search completion, drains failures and notifies server
   - Good network citizenship - server can attempt alternative routing

4. **✅ Post-PierceFireWall response handling** (`peer.rs`)
   - Clarified expected flow with comments and debug logging
   - After PierceFireWall, we should expect FileSearchResponse directly (not PeerInit)
   - Kept backward-compatible handling for robustness

**Key Code Additions:**

```rust
// peer.rs - CantConnectToPeer helper (lines ~90-115)
pub async fn send_cant_connect_to_peer(
    server_stream: &mut TcpStream,
    token: u32,
    username: &str,
) -> std::io::Result<()>

// client.rs - Failure channel in search loop (line ~557)
let (failure_tx, mut failure_rx) = mpsc::channel::<(u32, String)>(100);

// client.rs - After search, send CantConnectToPeer messages (lines ~703-720)
while let Ok(Some((token, username))) = tokio::time::timeout(...).await {
    super::peer::send_cant_connect_to_peer(&mut self.server_stream, token, &username).await?;
}
```

---

### Next Phase: Diagnostic Analysis Required

The following issues remain and need investigation before further protocol work:

**Issue 1: Zero Incoming Connections (HIGH PRIORITY)**
```
[SoulseekClient] Incoming connections: 0
```
Despite UPnP claiming success for ports 2234/2235, no peers connect to our listener.

**Possible causes to investigate:**
- UPnP mapping not actually working (router may not honor it)
- Double NAT / CGNAT at ISP level
- macOS firewall blocking incoming
- Listener accept loop bug

**Diagnostic steps:**
1. Test port accessibility from external network (`nc -vz <external-ip> 2234`)
2. Check local listener works (`nc -vz 127.0.0.1 2234`)
3. Verify UPnP mappings on router admin page
4. Check macOS firewall settings

**Issue 2: Orphaned Popups for List Searches (MEDIUM PRIORITY)**
List queue searches (`queueList`) don't pass per-item `client_id`, causing popup tracking to fail. Items use query fallback matching which can mismatch.

**Files needing update:**
- `backend/src/api/queue.rs` - Accept client_ids array
- `backend/src/services/queue.rs` - Pass to enqueue
- `frontend/src/lib/api.ts` - Generate per-item client_ids
- `frontend/src/pages/Search.tsx` - Update list mutation

---

### Diagnostic Logging Session (Feb 3, 2026 - Afternoon)

After implementing protocol fixes, user reported downloads failing in some cases. Added comprehensive diagnostic logging to trace the issue:

**Files Modified:**
- `src/protocol/peer.rs` - Added `[ConnectToPeer]` and `[F-CONN]` logging
- `src/protocol/client.rs` - Added `[Download]` step logging and `[Search]` connection storage logging

**Status:** Logging added, code compiles, awaiting test run to identify failure point.

**See "CRITICAL REGRESSION" section below for full diagnostic guide.**

---

### Previous: Protocol Analysis Session

Conducted deep analysis comparing our Soulseek protocol implementation with Nicotine+ reference client. Identified major differences causing low peer success rates (21% in sample search).

**Key Documents Created:**
- `docs/PROTOCOL_IMPL.md` - Comprehensive implementation guide with TODO list
- `docs/nicotine-comparison.md` - Detailed technical comparison

**Original Findings (before fixes):**

1. ~~**TCP_NODELAY not set**~~ → ✅ FIXED
2. ~~**No CantConnectToPeer feedback**~~ → ✅ FIXED
3. ~~**PeerInit token should be 0**~~ → ✅ FIXED
4. ~~**Post-PierceFireWall handling**~~ → ✅ FIXED
5. **Zero incoming connections** → 🔍 NEEDS DIAGNOSIS
6. **Peer blacklisting after failed downloads** → 📋 Future investigation

**Added Handler Timing Logging:**
- `src/protocol/client.rs` - Added `ACTIVE_HANDLERS` counter and timing for handshake/recv phases
- Logs show handshake duration, receive duration, and total task time

---

## Session Update - February 1, 2026

### Completed This Session

#### 1. Client ID for Search Popup Tracking (MOSTLY DONE)
Implemented client-generated IDs to link frontend popups with backend WebSocket events:

**Backend Changes:**
- Created migration `007_add_client_id.sql` - adds `client_id` column to `search_queue` table
- Updated `models/mod.rs` - added `client_id: Option<String>` to relevant structs
- Updated `db/mod.rs` - `enqueue_search` now accepts and stores `client_id`
- Updated `api/queue.rs` - endpoint passes `client_id` through
- Updated `api/ws.rs` - all `WsEvent` variants now have `client_id` with `skip_serializing_if`
- Updated `services/queue.rs` - includes `client_id` in all WebSocket broadcasts

**Frontend Changes:**
- Updated `types/index.ts` - added `client_id` to all WebSocket event types
- Updated `lib/api.ts` - `queueSearch` generates `client_id` if not provided
- Updated `store/index.ts` - `addPendingSearch(query, clientId)`, matching by `client_id` first
- Updated `pages/Search.tsx` - generates clientId and passes to both store and API
- Fixed `search_started` handler to not overwrite advanced stages (`duplicate`, `completed`, etc.)

**Remaining Issue:** List searches (`queueList`) don't pass per-item `client_id` - they fall back to query matching. Need to update list API to support this.

#### 2. Move Play Button to Track Name Column (DONE)
- Updated `Items.tsx` and `ListDetail.tsx` to inline play button with filename
- Uses flex layout with gap-3 for spacing

#### 3. Remove Legacy Download Code (DONE)
- Removed ~630 lines of `_legacy_*` functions from `services/download.rs`
- `DownloadService` is now a clean CRUD/connection manager
- Removed unused `ws_broadcast` parameter

#### 4. WebSocket Initial Load Optimization (DONE)
- Changed `api/ws.rs` to only send `item_updated` for active items (pending/downloading/queued)
- Previously sent ALL items including completed ones (50+ unnecessary events)

#### 5. Peer Connection LRU Cache (DONE)
- Added `lru` crate to dependencies
- Changed `peer_streams` from `HashMap` to `LruCache` with max 100 connections
- Removed storage of connections from peers with no results
- This prevents long-term FD accumulation

### Discovered Issue: File Descriptor Exhaustion (NEEDS WORK)

**Problem:** "Too many open files" errors occur immediately during search, even with LRU cache fix.

**Root Cause:** Burst of concurrent connections during search exceeds FD limit (default 256 on macOS):
- When you search, 200+ peers may connect simultaneously
- Each `accept()` creates a file descriptor
- Spawned handler tasks hold FDs until complete
- Rate of incoming connections exceeds rate of task completion

**Analysis Document:** See `docs/fd-exhaustion-analysis.md` for full details.

**Potential Solutions:**
1. Increase ulimit: `ulimit -n 4096` (immediate workaround)
2. Semaphore to limit concurrent handlers
3. Connection timeout for slow peers
4. Bounded worker pool instead of unbounded spawning

### Key Files Modified This Session

**Backend:**
- `src/api/ws.rs` - Only send active items on connect, added client_id to events
- `src/api/queue.rs` - Accept and pass client_id
- `src/services/queue.rs` - Include client_id in WebSocket broadcasts
- `src/services/download.rs` - Removed legacy code, simplified to CRUD only
- `src/protocol/client.rs` - LRU cache for peer_streams, removed no-result connection storage
- `src/db/mod.rs` - Store client_id in enqueue_search
- `src/models/mod.rs` - Added client_id to request/response structs
- `migrations/007_add_client_id.sql` - New migration
- `Cargo.toml` - Added lru crate

**Frontend:**
- `src/store/index.ts` - Client ID generation, matching logic, stage protection
- `src/lib/api.ts` - Generate and pass client_id
- `src/pages/Search.tsx` - Use generateClientId
- `src/pages/Items.tsx` - Inline play button, fixed activeDownloads lookup
- `src/pages/ListDetail.tsx` - Inline play button
- `src/pages/ItemDetail.tsx` - Added missing status colors

---

## NEXT TASKS TO WORK ON

### 1. Protocol Diagnostics (NEXT PHASE)

**See `docs/PROTOCOL_IMPL.md` for full implementation guide and status.**

Protocol Phase 1 & 2 fixes are complete with measurable improvements (peer success rate: 1.4% → 6.9%). Next phase requires diagnostic investigation.

#### ✅ Completed (Feb 3, 2026)
- [x] **Add TCP_NODELAY** to all connections - Success rate improved 2-5x
- [x] **Change PeerInit token to 0** - Now matches protocol spec
- [x] **Implement CantConnectToPeer** - Server feedback on failures
- [x] **Simplify post-PierceFireWall** - Better response handling with logging

#### 🚨 CRITICAL REGRESSION - Diagnose First
- [ ] **Downloads failing after PierceFireWall changes** - Since implementing fix #4 (Post-PierceFireWall handling), track downloads are no longer functioning in some cases

**Background:**
On Feb 3, 2026, we implemented protocol fixes to improve peer connection rates. Fix #4 modified `connect_to_peer_for_results()` in `peer.rs` to clarify post-PierceFireWall response handling. After this change, some downloads stopped working.

**Diagnostic Logging Added (Feb 3, 2026):**
Comprehensive logging was added to trace the complete flow:

| Log Prefix | Location | What It Traces |
|------------|----------|----------------|
| `[ConnectToPeer]` | `peer.rs` | Search result retrieval from peers |
| `[Download]` | `client.rs` | Download initiation steps 1-6 |
| `[F-CONN]` | `peer.rs` | F connection waiting/establishment |
| `[Search]` | `client.rs` | Peer connection caching |

**How to Diagnose:**
1. Run backend with debug logging: `RUST_LOG=debug cargo run`
2. Perform a search and attempt download
3. Look for which step fails in the logs

**Expected Successful Flow:**
```
[Search] Storing P connection for 'username' (has N files)
[SoulseekClient] Cached peer connections: N
[Download] === INITIATE DOWNLOAD START ===
[Download] Step 1: Getting P connection for 'username'
[Download] Using stored P connection from search for 'username'
[Download] Step 2: Sending QueueUpload
[Download] Step 3: Waiting for peer's TransferRequest
[Download] Step 4: Transfer ALLOWED
[Download] Step 5: Waiting for F connection
[F-CONN] === WAITING FOR F CONNECTION ===
[F-CONN] Token MATCHES! Returning connection.
[Download] Step 6: F connection established
[Transfer] === DOWNLOAD COMPLETE ===
```

**Potential Failure Points & Fixes:**

1. **No stored P connection** (Step 1 shows "No stored P connection")
   - Cause: `connect_to_peer_for_results()` failing during search
   - Look for: `[ConnectToPeer]` errors during search phase
   - Likely issue: Response parsing changed, peer sends unexpected format
   - Fix: Adjust response handling in `connect_to_peer_for_results()`

2. **QueueUpload fails** (Step 2 shows error)
   - Cause: P connection was stored but is now stale/closed
   - Fix: Add connection health check or reconnection logic

3. **TransferRequest timeout** (Step 3 times out after 60s)
   - Cause: Peer not responding to QueueUpload
   - Check: Is peer online? Are we sending correct message format?

4. **F connection timeout** (Step 5 times out after 30s)
   - Cause: Neither direct nor indirect F connection established
   - Check `[F-CONN]` logs for ConnectToPeer F messages
   - Possible: Our listener not working (zero incoming connections issue)

5. **Token mismatch** (`[F-CONN] Token MISMATCH`)
   - Cause: Wrong F connection received
   - Fix: Check token handling in negotiation

**Key Code Locations:**
- `peer.rs:471-601` - `connect_to_peer_for_results()` - Search result retrieval
- `peer.rs:951-1023` - `wait_for_file_connection()` - F connection waiting
- `client.rs:1003-1247` - `initiate_download()` - Full download flow
- `client.rs:539-731` - `search()` - Peer connection storage

**After Identifying the Issue:**
Once logs reveal the failure point, the fix will likely be in one of:
- Response parsing in `connect_to_peer_for_results()`
- Connection storage/retrieval logic
- Token handling between negotiation and F connection

**Quick Reference - What Changed in Fix #4:**
The changes to `connect_to_peer_for_results()` (lines 471-601 in peer.rs) modified how we handle responses after sending PierceFireWall:
- Added more specific logging for message types
- Changed some `tracing::debug!` to include more detail
- Added retry loop logging
- The core protocol logic was NOT changed, only logging/comments

If the issue is in search result retrieval, revert or simplify the response handling.
If the issue is in download flow, check if peer_streams are being populated correctly.

#### 🔍 Needs Diagnosis
- [ ] **Zero incoming connections** - UPnP claims success but no peers connect to listener
  - Test: External port check, local listener test, UPnP verification, macOS firewall
  - May indicate CGNAT, UPnP not working, or listener bug

- [ ] **List search popup tracking** - `queueList` doesn't pass per-item client_id
  - Files: `api/queue.rs`, `services/queue.rs`, `frontend/lib/api.ts`

#### 📋 Future Investigation
- [ ] **Peer blacklisting after failed downloads** - Peers that worked reject us after timeout
- [ ] **F connection establishment issues** - Some indirect F connections fail
- [ ] **Connection rate limiting** - May be connecting too aggressively

**Benchmark Process Established:**
1. Breakdown and analyze task
2. Benchmark existing functionality
3. Implement solution
4. Benchmark results
5. If successful, proceed
6. Update relevant documents

### 2. Fix File Descriptor Exhaustion (HIGH PRIORITY)
**Problem:** "Too many open files" errors during search bursts.

**Solution Options:**
- Add semaphore to limit concurrent connection handlers
- Add timeout to drop slow/hung connections
- See `docs/fd-exhaustion-analysis.md` for full analysis

**Files to modify:**
- `src/protocol/client.rs` - Add semaphore/timeout to listener loop

### 3. Add client_id to List Searches (MEDIUM PRIORITY)
**Problem:** List queue searches don't have per-item `client_id`, causing popup orphaning.

**Current:** `queueList` API doesn't support per-query client_ids
**Solution:** Either update backend list API to accept client_ids per query, or use a different tracking approach for lists.

**Files to modify:**
- `backend/src/api/queue.rs` - Accept client_ids array in list request
- `backend/src/services/queue.rs` - Pass client_ids to enqueue_searches_for_list
- `frontend/src/lib/api.ts` - Generate and pass client_ids for list searches
- `frontend/src/pages/Search.tsx` - Update list mutation

### 4. Delete from List Should Remove from List
**Current behavior**: "Delete" on a track in a list only soft-deletes the item; it stays in the list with "Deleted" status
**Desired behavior**: "Delete" on a track in a list should ALSO remove it from the list

**Files to modify:**
- `frontend/src/pages/ListDetail.tsx` - Delete action should call both delete and remove

---

## Session Update - January 31, 2026

### Completed That Session

#### 1. Audio Player with Waveform Visualization (DONE)
Added a persistent bottom-bar audio player using wavesurfer.js:
- **Backend**: Added HTTP Range Request support in `api/items.rs` for audio seeking (206 Partial Content)
- **Frontend**: Created `AudioPlayer.tsx` component with waveform, play/pause, time display
- **Store**: Added audio player state (`currentTrack`, `isPlaying`, `playTrack`, etc.) to Zustand
- **Integration**: Play buttons added to Items table, ListDetail table, and ItemDetail page
- **Bug fixes**: Fixed multiple issues including previous track not stopping, play button not working, and waveform glitching due to React re-render loops (solved with local state pattern)

#### 2. Retry Button for Failed Items (DONE)
- Added retry button (RotateCcw icon) to Items page actions column for failed downloads
- Retries by queuing a new search with the same query and deleting the failed item

#### 3. Early Abort for No Results (DONE)
- Modified `protocol/client.rs` to abort search after 5 seconds if no results found
- Modified `services/queue.rs` to skip retry for "no results" errors (permanent failure)

#### 4. List Item Removal Fix (DONE)
- Fixed `db/mod.rs` to properly decrement `total_items` count when removing items from lists

#### 5. Soft Delete for Items (DONE)
- Items now have soft delete with `deleted_at` timestamp
- Lists show "Deleted" status for items deleted from outside the list

---

## Important Instructions for Claude

**READ THIS FIRST - CRITICAL BEHAVIORAL GUIDELINES**:

1. **Be CAREFUL** - Double-check assumptions before acting
2. **Be THOUGHTFUL** - Consider edge cases and side effects
3. **Be SLOW** - Don't rush; quality over speed
4. **Be METHODICAL** - Follow a systematic approach, one step at a time
5. **Be LOGICAL** - Trace through code flows completely before and after changes
6. **ASK QUESTIONS OFTEN** - When in doubt, ask the user for clarification rather than guessing

When working on this codebase, you must also:
- **Understand before changing** - Read and comprehend existing code before modifications
- **Prioritize code quality** - Clean separation of concerns, proper abstractions, no hacks
- **Keep code tidy** - Remove dead code, unused imports; maintain consistent style
- **Consult protocol references** - When working on protocol logic, ALWAYS refer to:
  - Soulseek protocol docs: https://nicotine-plus.org/doc/SLSKPROTOCOL.html
  - Nicotine+ source code: `backend/nicotine_reference/` (Python reference implementation)

The Soulseek protocol is complex with subtle timing issues. Changes that seem simple can break the networking flow. Always trace through the complete flow before and after changes.

---

## Overview

Rinse is a Soulseek P2P music download client with a Rust backend and React/TypeScript frontend. The backend handles all Soulseek protocol communication, file downloads, and provides a REST API + WebSocket interface for the frontend.

## Tech Stack

- **Backend**: Rust, Axum, SQLite (sqlx), Tokio
- **Frontend**: React, TypeScript, TanStack Query, Zustand, Tailwind CSS
- **Protocol**: Soulseek P2P (server.slsknet.org:2242)

## Project Structure

```
backend/
├── src/
│   ├── main.rs              # Entry point, server setup, service initialization
│   ├── protocol/
│   │   ├── client.rs        # SoulseekClient - search, download orchestration
│   │   │                    # Key methods: search(), download_file(), initiate_download()
│   │   │                    # Uses LRU cache for peer_streams (max 100 connections)
│   │   ├── peer.rs          # Peer protocol - P/F connections, file transfer
│   │   │                    # Key functions: wait_for_file_connection(), receive_file_data()
│   │   ├── messages.rs      # Protocol message types and encoding
│   │   ├── file_selection.rs # File scoring algorithm + format filtering
│   │   │                    # **NEEDS WORK**: Picks remixes over originals (see Known Issues)
│   │   ├── connection.rs    # Low-level connection handling
│   │   ├── codec.rs         # Message framing
│   │   └── obfuscation.rs   # Soulseek obfuscation support
│   ├── services/
│   │   ├── download.rs      # DownloadService - CRUD operations + Soulseek connection
│   │   │                    # (Legacy search code removed - now uses QueueService)
│   │   ├── queue.rs         # QueueService - non-blocking queue system
│   │   │                    # Manages search queue, spawned transfers, completion handling
│   │   ├── fuzzy.rs         # Fuzzy matching for duplicate detection
│   │   └── upnp.rs          # UPnP port forwarding (ports 2234, 2235)
│   ├── api/
│   │   ├── items.rs         # Item endpoints (CRUD, download with Range support)
│   │   ├── lists.rs         # List management endpoints
│   │   ├── queue.rs         # Queue API endpoints
│   │   │                    # POST /api/queue/search, POST /api/queue/list, etc.
│   │   ├── ws.rs            # WebSocket events + broadcast channel
│   │   │                    # Events include client_id for popup tracking
│   │   └── auth.rs          # JWT authentication
│   ├── db/
│   │   └── mod.rs           # Database operations (SQLite via sqlx)
│   └── models/
│       └── mod.rs           # Data models (Item, List, User, QueuedSearch, etc.)
├── storage/                 # Downloaded files stored here
├── data/                    # SQLite database (rinse.db)
├── docs/
│   └── fd-exhaustion-analysis.md  # Analysis of file descriptor issue
└── migrations/
    ├── 005_create_search_queue.sql  # Queue tables
    └── 007_add_client_id.sql        # Client ID for popup tracking
```

---

## Current Development State: Queue System

### What's Been Implemented (January 2026)

The queue system enables **non-blocking concurrent downloads**:

1. **Database Schema** (`migrations/005_create_search_queue.sql`)
   - `search_queue` table with status tracking (pending, processing, completed, failed)
   - Links to users, lists, and items

2. **Queue Service** (`services/queue.rs`)
   - `QueueService` manages the search queue
   - `start_worker()` - Background task that processes queue FIFO
   - `start_transfer_monitor()` - Handles transfer completion notifications
   - `execute_search_and_download()` - Does search, creates item, initiates download

3. **Non-Blocking Downloads** (`protocol/client.rs`)
   - `initiate_download()` - Negotiates with peer, waits for F connection, spawns file receive
   - File transfers run in background via `tokio::spawn()`
   - Completion notifications sent via `mpsc` channel to TransferMonitor

4. **Queue API** (`api/queue.rs`)
   - `POST /api/queue/search` - Enqueue single search
   - `POST /api/queue/list` - Enqueue list of searches
   - `GET /api/queue` - Get queue status
   - `GET /api/queue/items` - Get pending queue items
   - `DELETE /api/queue/:id` - Cancel pending search

5. **WebSocket Events** (added to `api/ws.rs`)
   - `SearchQueued`, `SearchProcessing`, `SearchFailed`
   - `ListCreated`, `ListProgress`, `QueueStatus`

### How the Queue System Works

```
1. Client POSTs to /api/queue/search
   └─> QueueService.enqueue_search() adds to DB with status="pending"

2. Queue Worker (background loop) picks next pending search
   └─> Marks status="processing"
   └─> Calls execute_search_and_download()

3. execute_search_and_download():
   └─> Acquires slsk_client write lock
   └─> Executes search (10 sec timeout)
   └─> Selects best file via file_selection scoring
   └─> Creates item in DB with status="downloading"
   └─> Calls client.initiate_download()

4. initiate_download():
   └─> Gets/establishes P connection with peer
   └─> Sends QueueUpload
   └─> Waits for TransferRequest, sends TransferReply
   └─> Calls wait_for_file_connection() (CRITICAL - handles both direct and server-brokered)
   └─> Spawns receive_file_data() as background task
   └─> Returns immediately (transfer runs concurrently)

5. Transfer Monitor receives completion notification
   └─> Updates item status in DB
   └─> Broadcasts WebSocket events

6. Queue Worker moves to next pending search
```

### What Still Needs Work

**TODO - High Priority:**
- [ ] **Refactor Search popup tracking** - See "NEXT TASKS" section above
- [ ] **Fix file selection algorithm** - Currently picks remixes over originals (see Known Issues below)
- [ ] **Investigate duplicate item entries** - Items sometimes created multiple times for same query

**TODO - Medium Priority:**
- [ ] Search/handshake performance - Currently slow, investigate bottlenecks
- [ ] Retry logic when peer queues us (NegotiationResult::Queued path)

**DONE:**
- [x] ~~Integrate queue system with frontend~~ - DONE
- [x] ~~Better progress tracking in frontend for queued items~~ - DONE (multi-popup system)
- [x] ~~Audio player with waveform~~ - DONE (wavesurfer.js)
- [x] ~~Retry button for failed items~~ - DONE
- [x] ~~Early abort for no results~~ - DONE
- [x] ~~List item removal fixes~~ - DONE
- [x] ~~Remove legacy blocking search endpoint~~ - DONE

---

## Known Issues - MUST ADDRESS

### 1. File Selection Picks Remixes Over Originals (HIGH PRIORITY)

**Problem**: The scoring algorithm in `file_selection.rs` frequently selects remixes, edits, or alternate versions instead of the original track.

**Examples of wrong selections:**
- Query: "The Chemical Brothers Block Rockin Beats" → Selected: "Block Rockin' Beats (The Micronauts Mix).mp3"
- Query: "Fatboy Slim Right Here Right Now" → Selected: "RIGHT HERE RIGHT NOW (SOUBERIAN EDIT).mp3"
- Query: "Aphex Twin Windowlicker" → Selected: "Windowlicker (Acid Edit).mp3"

**Root Cause**: The scoring algorithm (`score_file()`, `score_title_match()`) doesn't penalize files with remix/edit/mix indicators in the filename.

**Fix Needed**: Add negative scoring for files containing:
- "(... Mix)", "(... Remix)", "(... Edit)", "(... Version)"
- "remix", "edit", "instrumental", "acapella", "radio edit", "extended"
- Unless the query explicitly contains these terms

**Location**: `src/protocol/file_selection.rs`

### 2. Search/Handshake Performance (MEDIUM PRIORITY)

**Problem**: The search and peer handshake process is slow. Many peers timeout during handshake.

**Observations from logs:**
- Many "handshake timeout (connected but no response)" errors
- Many "TCP connect timed out (peer unreachable)" errors
- Search takes full 10 seconds even when good results arrive early

**Potential Improvements:**
- Consider early termination when enough good results found
- Tune timeout values
- Investigate if handshake protocol can be optimized

---

## Soulseek Protocol

### Connection Types
- **Server connection**: Main connection to server.slsknet.org:2242
- **P connection**: Peer negotiation (QueueUpload, TransferRequest, TransferReply)
- **F connection**: File transfer (actual data bytes)

### Download Flow (Current Implementation)
```
1. Send QueueUpload on P connection
2. Wait for peer's TransferRequest (direction=1)
3. Send TransferReply (allowed=true)
4. Wait for F connection:
   - Direct: Peer connects to our listener (ports 2234/2235)
   - Indirect: Server sends ConnectToPeer type="F", we connect to peer
5. Send FileOffset (0)
6. Receive file data with progress callbacks
```

### Critical Implementation Details

**F Connection Handling (Fixed in this session)**:
After TransferReply, we must actively wait for the F connection by reading BOTH:
- `file_conn_rx` channel (for direct connections via listener)
- `server_stream` (for ConnectToPeer type="F" messages)

The `wait_for_file_connection()` function in `peer.rs` handles this. Previously, `initiate_download()` just registered a pending download and returned - this broke indirect connections because no one was reading server messages.

**Stored P connections**: During search, peers connect to us to send results. We store these in `peer_streams` HashMap. When downloading, we REUSE these stored connections because most peers have firewalls.

**Progress callbacks**: Use `ArcProgressCallback` (Arc<dyn Fn(u64, u64, f64) + Send + Sync>) for thread-safe progress updates that can be passed to spawned tasks.

---

## API Endpoints

### Queue API (NEW - Use This)
- `POST /api/queue/search` - Enqueue single search `{query, format?}`
- `POST /api/queue/list` - Enqueue list `{name?, queries[], format?}`
- `GET /api/queue` - Queue status `{pending, processing, active_downloads, ...}`
- `GET /api/queue/items` - Pending items for user
- `DELETE /api/queue/:id` - Cancel pending search

### Legacy API (To Be Removed)
- `POST /api/items/search` - Blocking search+download (still used by frontend)

### Other Endpoints
- `GET /api/items` - List all items
- `GET /api/items/:id` - Get item
- `DELETE /api/items/:id` - Delete item
- `GET /api/items/:id/download` - Download file
- Similar patterns for `/api/lists`

---

## Commands

### Start Backend (use tmux session "rinse")
```bash
cd backend
cargo run --bin rinse-backend
```

### Start Frontend
```bash
cd frontend
npm run dev
```

### Check tmux logs
```bash
tmux capture-pane -t rinse -p | tail -50
```

### Debug logging
```bash
RUST_LOG=debug cargo run --bin rinse-backend
```

---

## Log Prefixes
- `[SoulseekClient]` - Client-level operations
- `[Search]` - Search result collection
- `[Download]` - Download orchestration in client.rs
- `[DOWNLOAD]` - File transfer in peer.rs
- `[Transfer]` - Spawned transfer tasks
- `[QueueWorker]` - Queue processing
- `[TransferMonitor]` - Transfer completion handling
- `[Listener]` - Incoming peer connections

---

## Architecture Notes

### Concurrency Model
- Single `SoulseekClient` with one server connection
- Client protected by `Arc<RwLock<Option<SoulseekClient>>>`
- Queue worker holds write lock during search+negotiation
- File transfers spawned as independent tasks (don't hold lock)
- Multiple transfers can run concurrently

### Data Flow
```
Frontend -> API -> QueueService -> SoulseekClient -> Peer
                                                   |
                                      spawned task: receive_file_data()
                                                   |
                                      TransferMonitor <- completion notification
                                                   |
                                      Database update + WebSocket broadcast
```

---

## Environment Variables (.env)
```
DATABASE_URL=sqlite:data/rinse.db
SLSK_USERNAME=your_username
SLSK_PASSWORD=your_password
STORAGE_PATH=storage
BIND_ADDR=127.0.0.1:3000
JWT_SECRET=change-me-in-production
# Optional SMTP for email verification
SMTP_HOST=smtp.example.com
SMTP_PORT=465
SMTP_USERNAME=...
SMTP_PASSWORD=...
```

---

## Tips for Development

1. **Test with real Soulseek network** - Protocol quirks only show up with real peers
2. **Watch the logs** - Use log prefixes to trace issues through the stack
3. **Trace the complete flow** - Before changing anything, understand the full path
4. **Peers are unreliable** - They disconnect, timeout, queue you; always handle errors
5. **UPnP is critical** - Ports 2234/2235 must be forwarded for peers to connect to us
6. **Backend needs restart** - Rust changes require recompilation
7. **Frontend hot-reloads** - TypeScript changes apply immediately
8. **Check both direct and indirect paths** - F connections can come either way
