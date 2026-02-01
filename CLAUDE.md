# Rinse Backend - Project Documentation

---

## Session Update - January 31, 2026 (Latest)

### Completed This Session

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

### Key Files Modified This Session

**Backend:**
- `src/api/items.rs` - HTTP Range Request support for streaming
- `src/api/lists.rs` - Remove item from list endpoints
- `src/protocol/client.rs` - Early abort for no results
- `src/services/queue.rs` - Skip retry for no results
- `src/db/mod.rs` - List item removal with total_items update

**Frontend:**
- `src/components/AudioPlayer.tsx` - New wavesurfer.js audio player
- `src/components/Layout.tsx` - Integrated AudioPlayer
- `src/store/index.ts` - Audio player state, list item selection
- `src/pages/Items.tsx` - Play button, retry button columns
- `src/pages/ListDetail.tsx` - Play button, selection/actions
- `src/pages/ItemDetail.tsx` - Large play button

---

## NEXT TASKS TO WORK ON

### 1. Refactor Search Popup Tracking (HIGH PRIORITY)
**Problem**: Search popups become disconnected from their relevant search context, leaving components floating and miscommunicating search status.

**Solution**: Create client-generated IDs that link frontend to backend:
- When client creates a new search, generate a local tracking ID
- Pass this ID to the backend with the search request
- Backend associates client ID with its internal search ID
- All WebSocket events include this client ID for matching
- This creates an unbreakable link preventing orphaned popups

**Files to modify:**
- `frontend/src/store/index.ts` - Generate and track client IDs
- `frontend/src/lib/api.ts` - Pass client ID in queue requests
- `backend/src/api/queue.rs` - Accept and store client ID
- `backend/src/services/queue.rs` - Include client ID in WebSocket events
- `backend/src/db/mod.rs` - Store client ID in search_queue table

### 2. Move Play Button to Track Name Column
**Current**: Play button is in its own separate column
**Desired**: Play button should be inline with track name, sitting right in front of the filename

**Files to modify:**
- `frontend/src/pages/Items.tsx` - Combine play button into filename column
- `frontend/src/pages/ListDetail.tsx` - Same change

### 3. Delete from List Should Remove from List
**Current behavior**: "Delete" on a track in a list only soft-deletes the item; it stays in the list with "Deleted" status
**Desired behavior**: "Delete" on a track in a list should ALSO remove it from the list
- The "Deleted" status should only appear for tracks deleted from OUTSIDE the list (e.g., from Items page)

**Files to modify:**
- `frontend/src/pages/ListDetail.tsx` - Delete action should call both delete and remove
- Potentially `backend/src/api/items.rs` - Consider if backend should handle this

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
│   │   ├── peer.rs          # Peer protocol - P/F connections, file transfer
│   │   │                    # Key functions: wait_for_file_connection(), receive_file_data()
│   │   ├── messages.rs      # Protocol message types and encoding
│   │   ├── file_selection.rs # File scoring algorithm + format filtering
│   │   │                    # **NEEDS WORK**: Picks remixes over originals (see Known Issues)
│   │   ├── connection.rs    # Low-level connection handling
│   │   ├── codec.rs         # Message framing
│   │   └── obfuscation.rs   # Soulseek obfuscation support
│   ├── services/
│   │   ├── download.rs      # DownloadService - blocking download path (legacy)
│   │   ├── queue.rs         # QueueService - non-blocking queue system (NEW)
│   │   │                    # Manages search queue, spawned transfers, completion handling
│   │   ├── fuzzy.rs         # Fuzzy matching for duplicate detection
│   │   └── upnp.rs          # UPnP port forwarding (ports 2234, 2235)
│   ├── api/
│   │   ├── items.rs         # Item endpoints - has legacy blocking search (to be removed)
│   │   ├── lists.rs         # List management endpoints
│   │   ├── queue.rs         # Queue API endpoints (NEW)
│   │   │                    # POST /api/queue/search, POST /api/queue/list, etc.
│   │   ├── ws.rs            # WebSocket events + broadcast channel
│   │   └── auth.rs          # JWT authentication
│   ├── db/
│   │   └── mod.rs           # Database operations (SQLite via sqlx)
│   └── models/
│       └── mod.rs           # Data models (Item, List, User, QueuedSearch, etc.)
├── storage/                 # Downloaded files stored here
├── data/                    # SQLite database (rinse.db)
└── migrations/              # Database schema
    └── 005_create_search_queue.sql  # Queue tables (NEW)
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
