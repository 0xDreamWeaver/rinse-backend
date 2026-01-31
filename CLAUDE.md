# Rinse Backend - Project Documentation

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
│   ├── main.rs              # Entry point, server setup
│   ├── protocol/
│   │   ├── client.rs        # Main SoulseekClient - search, download orchestration
│   │   ├── peer.rs          # Peer protocol - P/F connections, download_file()
│   │   ├── messages.rs      # Protocol message types
│   │   ├── file_selection.rs # File scoring algorithm + format filtering
│   │   └── ...
│   ├── services/
│   │   ├── download.rs      # DownloadService - high-level download orchestration
│   │   ├── fuzzy.rs         # Fuzzy matching for duplicate detection
│   │   └── upnp.rs          # UPnP port forwarding
│   ├── api/
│   │   ├── items.rs         # Item endpoints (search, download, list)
│   │   ├── lists.rs         # List management endpoints
│   │   ├── ws.rs            # WebSocket events + broadcast channel
│   │   └── auth.rs          # Authentication
│   ├── db/
│   │   └── mod.rs           # Database operations
│   └── models/
│       └── mod.rs           # Data models (Item, List, User)
├── storage/                 # Downloaded files stored here
├── data/                    # SQLite database (rinse.db)
└── migrations/              # Database schema

frontend/
├── src/
│   ├── pages/
│   │   ├── Search.tsx       # Search UI with format toggles + progress popup
│   │   ├── Items.tsx        # Downloaded items list
│   │   └── Lists.tsx        # Batch download lists
│   ├── store/
│   │   └── index.ts         # Zustand store - handles WebSocket events
│   ├── hooks/
│   │   └── useWebSocket.ts  # WebSocket connection hook
│   ├── lib/
│   │   └── api.ts           # API client functions
│   └── types/
│       └── index.ts         # TypeScript types including WsEvent types
```

## Key Features

### Real-time WebSocket Updates
The backend broadcasts events via `tokio::sync::broadcast` channel. Events include:
- `SearchStarted` / `SearchProgress` / `SearchCompleted`
- `DownloadStarted` / `DownloadProgress` / `DownloadCompleted` / `DownloadFailed`
- `DownloadQueued` - when peer queues the download
- `DuplicateFound` - when searching for an already-downloaded track
- `ItemUpdated` - generic status update

Frontend store (`store/index.ts`) handles all events via `handleWsEvent()` and tracks active downloads in a Map.

### Format Filter
Users can filter search results by format (MP3, FLAC, M4A, WAV, or Any):
- Toggle buttons in Search.tsx UI
- `matches_format()` function in `file_selection.rs`
- Format passed through API to `find_best_file()`

### Duplicate Detection
When searching for a track that already exists:
- Backend detects via fuzzy matching in `download.rs`
- Broadcasts `DuplicateFound` event
- Frontend shows "Already in library" with cyan styling
- Returns existing item without network search

### Search Architecture (Important!)
Search uses **non-blocking ConnectToPeer** to avoid timeout issues:
- Server sends `ConnectToPeer` messages telling us which peers have results
- Each ConnectToPeer attempt is spawned in background via `tokio::spawn()`
- Results collected via `mpsc::channel`
- Search loop continues processing while peers respond
- This prevents slow/unresponsive peers from blocking the entire search

**Previous bug**: ConnectToPeer attempts were blocking (5s timeout each), consuming the entire 10s search window. Peers connecting to us (incoming connections) would arrive after the search ended.

## Soulseek Protocol

### Connection Types
- **Server connection**: Main connection to server.slsknet.org:2242
- **P connection**: Peer negotiation (QueueUpload, TransferRequest, TransferReply)
- **F connection**: File transfer (actual data bytes)

### Download Flow
```
1. Send QueueUpload on P connection
2. Wait for peer's TransferRequest (direction=1)
3. Send TransferReply (allowed=true)
4. Wait for F connection (peer connects to us OR server brokers indirect)
5. Send FileOffset (0)
6. Receive file data with progress callbacks
```

### Critical Implementation Details

**Stored P connections**: During search, peers connect to us to send results. We store these in `peer_streams` HashMap. When downloading, we REUSE these stored connections because most peers have firewalls - they can connect to US (via UPnP ports 2234/2235) but we can't connect to them.

**Progress callbacks**: `peer::download_file()` accepts an optional `ProgressCallback` that reports `(bytes_downloaded, total_bytes, speed_kbps)`. The download service creates callbacks that broadcast `DownloadProgress` WebSocket events.

**File selection scoring** (`file_selection.rs`):
- Title/query match similarity (most important)
- Audio format preference (FLAC > WAV > MP3)
- Bitrate for lossy formats (320kbps ideal)
- Peer speed and slot availability
- Format filter (when specified)

## Commands

### Start Backend
```bash
cd backend
cargo run --bin rinse-backend
```

### Start Frontend
```bash
cd frontend
npm run dev
```

### Kill stuck processes (if ports in use)
```bash
lsof -ti:3000 | xargs kill -9  # Backend API
lsof -ti:2234 | xargs kill -9  # Soulseek listener
```

### Check backend logs
```bash
# If running in background via Claude
tail -50 /tmp/claude/-Users-brocsmith-Documents-Development-17-Rinse/tasks/<task-id>.output
```

### Debug logging
```bash
RUST_LOG=debug cargo run --bin rinse-backend
```

## Log Prefixes
- `[SoulseekClient]` - Client-level operations (search, download orchestration)
- `[Search]` - Search result collection, ConnectToPeer handling
- `[Download]` - Download orchestration in client.rs
- `[DOWNLOAD]` - File transfer in peer.rs (progress updates)
- `[DownloadService]` - Service-level operations
- `[Listener]` - Incoming peer connections (P and F)

## Recent Session Work (January 2026)

### Completed
1. **WebSocket real-time updates** - Full implementation with broadcast channel
2. **Format filter** - Users can select MP3/FLAC/M4A/WAV/Any
3. **Non-blocking search** - ConnectToPeer spawned in background
4. **Duplicate detection UX** - "Already in library" popup state
5. **Popup state management** - Clears old states when new search starts

### Architecture Decisions
- WebSocket uses broadcast channel, not database polling
- Progress callbacks flow: peer.rs -> client.rs -> download.rs -> WebSocket broadcast
- Frontend Zustand store tracks `activeDownloads` Map with stages
- Popup shows most recent download, clears finished states on new search

## Known Issues / TODO

### High Priority
- [ ] Handle queued downloads resume (when peer F-connects later)
- [ ] Better error messages for common failures

### Medium Priority
- [ ] Metadata extraction from downloaded files (ID3 tags)
- [ ] Download queue management UI
- [ ] Resume interrupted downloads

### Low Priority
- [ ] Upload/sharing functionality
- [ ] Chat functionality
- [ ] User preferences/settings

## Tips for Development

1. **Test with real network** - Soulseek quirks only show up with real peers
2. **Peers are unreliable** - Always have retry logic, peers disconnect randomly
3. **Watch the logs** - The log prefixes help trace issues through the stack
4. **Check both connection types** - Issues often involve P vs F connection handling
5. **Frontend hot-reloads** - TypeScript changes apply immediately in dev mode
6. **Backend needs restart** - Rust changes require `cargo run` restart
7. **UPnP is critical** - Ports 2234/2235 must be forwarded for peers to connect to us
8. **Search timing matters** - If search returns 0 results, check if peers arrived AFTER timeout

## Environment Variables (.env)
```
DATABASE_URL=sqlite:data/rinse.db
SLSK_USERNAME=your_username
SLSK_PASSWORD=your_password
STORAGE_PATH=storage
SMTP_HOST=smtp.example.com  # Optional, for email verification
SMTP_PORT=465
SMTP_USERNAME=...
SMTP_PASSWORD=...
```
