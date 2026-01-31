# Implementation Plan: Search Queue + Spawned File Transfers

## Overview

**Goal**: Clients can submit searches/lists at any time. Backend queues them, executes searches sequentially (server limitation), but spawns file transfers as independent background tasks. This allows:
- Multiple file downloads happening concurrently
- Searches continuing while downloads progress
- Users submitting multiple lists without waiting

```
┌─────────────────────────────────────────────────────────────────────────┐
│                        New Architecture                                  │
├─────────────────────────────────────────────────────────────────────────┤
│                                                                          │
│   Client A ──┐                                                           │
│              ├──► Search Queue ──► Queue Worker ──┬──► Search 1          │
│   Client B ──┘    (DB-backed)     (sequential)    │      ↓               │
│                                                   │   Negotiate          │
│                                                   │      ↓               │
│                                                   │   Get FileConn       │
│                                                   │      ↓               │
│                                                   │   SPAWN Transfer ────┼──► Transfer Pool
│                                                   │      ↓               │    (concurrent)
│                                                   └──► Search 2          │
│                                                          ↓               │
│                                                       ...etc             │
│                                                                          │
│   Transfer Pool (unlimited): [Download A] [Download B] [Download C] ...  │
│                       ↓            ↓            ↓                        │
│                   Complete      Complete     In Progress                 │
│                       ↓            ↓                                     │
│                   Update DB + Broadcast WebSocket                        │
│                                                                          │
└─────────────────────────────────────────────────────────────────────────┘
```

---

## Component Design

### 1. Database Schema Changes

**New table: `search_queue`**
```sql
CREATE TABLE search_queue (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL,
    query TEXT NOT NULL,
    format TEXT,                          -- Optional format filter
    list_id INTEGER,                      -- NULL for single searches, set for list items
    list_position INTEGER,                -- Position within list (for ordering)
    status TEXT NOT NULL DEFAULT 'pending', -- pending, processing, completed, failed
    item_id INTEGER,                      -- Created item ID (after search succeeds)
    error_message TEXT,                   -- Error if failed
    created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    started_at DATETIME,
    completed_at DATETIME,
    FOREIGN KEY (user_id) REFERENCES users(id),
    FOREIGN KEY (list_id) REFERENCES lists(id) ON DELETE CASCADE,
    FOREIGN KEY (item_id) REFERENCES items(id)
);

CREATE INDEX idx_search_queue_status ON search_queue(status);
CREATE INDEX idx_search_queue_list ON search_queue(list_id);
```

**New table: `active_transfers`**
```sql
CREATE TABLE active_transfers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id INTEGER NOT NULL UNIQUE,
    transfer_token INTEGER NOT NULL,
    filesize INTEGER NOT NULL,
    output_path TEXT NOT NULL,
    started_at DATETIME DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (item_id) REFERENCES items(id) ON DELETE CASCADE
);
```

---

### 2. Queue Service (`src/services/queue.rs`)

```rust
pub struct QueueService {
    db: Database,
    slsk_client: Arc<RwLock<Option<SoulseekClient>>>,
    ws_broadcast: broadcast::Sender<WsEvent>,
    transfer_completion_tx: mpsc::Sender<TransferCompletion>,
}

impl QueueService {
    /// Add a single search to the queue
    pub async fn enqueue_search(
        &self,
        query: &str,
        user_id: i64,
        format: Option<&str>,
    ) -> Result<QueuedSearch>;

    /// Add a list of searches to the queue
    pub async fn enqueue_list(
        &self,
        queries: Vec<String>,
        list_name: Option<String>,
        user_id: i64,
        format: Option<String>,
    ) -> Result<List>;

    /// Get queue status for a user
    pub async fn get_queue_status(&self, user_id: i64) -> Result<QueueStatus>;

    /// Cancel a queued search (if still pending)
    pub async fn cancel_search(&self, queue_id: i64) -> Result<()>;

    /// Start the queue worker (call once at startup)
    pub fn start_worker(&self) -> JoinHandle<()>;

    /// Start the transfer monitor (call once at startup)
    pub fn start_transfer_monitor(&self) -> JoinHandle<()>;
}
```

---

### 3. Queue Worker

Single background task that:
1. Polls for next pending search
2. Marks as "processing"
3. Executes search (acquires slsk_client lock)
4. Selects best file
5. Negotiates with peer
6. Gets FileConnection
7. Spawns file transfer as independent task
8. Marks search as "completed" (download still in progress)
9. Records active transfer in DB
10. Releases lock, loops to next

```rust
async fn queue_worker(service: Arc<QueueService>) {
    loop {
        // Get next pending search
        let next = service.db.get_next_pending_search().await;

        match next {
            Some(queued) => {
                service.process_queued_search(queued).await;
            }
            None => {
                // No pending searches, wait a bit
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}
```

---

### 4. Spawned File Transfers

**Changes to `client.rs`**:

```rust
/// New return type for download initiation
pub enum DownloadInitResult {
    /// File connection established, ready for spawned transfer
    Ready {
        file_conn: FileConnection,
        filesize: u64,
        transfer_token: u32,
    },
    /// Queued at peer, will receive F connection later
    Queued {
        transfer_token: u32,
        position: Option<u32>,
        reason: String,
    },
    /// Failed
    Failed { reason: String },
}

impl SoulseekClient {
    /// Initiate download - returns FileConnection for spawning
    /// Does NOT receive file data (caller spawns that)
    pub async fn initiate_download(
        &mut self,
        username: &str,
        filename: &str,
        filesize: u64,
        peer_ip: &str,
        peer_port: u32,
    ) -> DownloadInitResult {
        // ... existing negotiation code ...
        // But returns FileConnection instead of calling receive_file
    }
}
```

**Spawning the transfer** (in queue worker):

```rust
match client.initiate_download(...).await {
    DownloadInitResult::Ready { file_conn, filesize, transfer_token } => {
        // Record active transfer
        self.db.insert_active_transfer(item_id, transfer_token, filesize, &output_path).await?;

        // Spawn file receive as independent task
        let completion_tx = self.transfer_completion_tx.clone();
        let item_id = item.id;
        let path = output_path.clone();

        tokio::spawn(async move {
            let result = receive_file_data(file_conn, filesize, path, None).await;
            let _ = completion_tx.send(TransferCompletion { item_id, result }).await;
        });

        // Search is done, move to next (download continues in background)
    }
    // ... handle Queued and Failed ...
}
```

---

### 5. Transfer Monitor

Background task that receives completion notifications and updates state:

```rust
async fn transfer_monitor(
    mut completion_rx: mpsc::Receiver<TransferCompletion>,
    db: Database,
    ws_broadcast: broadcast::Sender<WsEvent>,
) {
    while let Some(completion) = completion_rx.recv().await {
        match completion.result {
            Ok(bytes) => {
                // Update item status to completed
                db.update_item_status(completion.item_id, "completed", 1.0, None).await;
                db.remove_active_transfer(completion.item_id).await;

                // Broadcast completion
                ws_broadcast.send(WsEvent::DownloadCompleted { ... });
            }
            Err(e) => {
                // Update item status to failed
                db.update_item_status(completion.item_id, "failed", 0.0, Some(&e.to_string())).await;
                db.remove_active_transfer(completion.item_id).await;

                // Broadcast failure
                ws_broadcast.send(WsEvent::DownloadFailed { ... });
            }
        }
    }
}
```

---

### 6. API Changes

**New endpoints**:

| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/api/queue/search` | Add single search to queue |
| POST | `/api/queue/list` | Add list of searches to queue |
| GET | `/api/queue` | Get queue status (pending, processing counts) |
| GET | `/api/queue/items` | Get all queued items for user |
| DELETE | `/api/queue/:id` | Cancel a pending search |

**Modify existing endpoints**:

| Endpoint | Change |
|----------|--------|
| `POST /api/items/search` | Now enqueues and returns immediately (with queue_id) |
| `POST /api/lists` | Now enqueues all items and returns immediately |

---

### 7. WebSocket Events

**New events**:

```rust
pub enum WsEvent {
    // Existing...

    /// Search added to queue
    SearchQueued {
        queue_id: i64,
        query: String,
        position: i32,  // Position in queue
    },

    /// Search started processing
    SearchProcessing {
        queue_id: i64,
        query: String,
    },

    /// Queue status update
    QueueStatus {
        pending: i32,
        processing: i32,
        active_transfers: i32,
    },
}
```

---

### 8. Progress Callbacks for Spawned Transfers

Since transfers run in spawned tasks, we need Arc-wrapped callbacks:

```rust
// In peer.rs - already exists:
pub type ArcProgressCallback = Arc<dyn Fn(u64, u64, f64) + Send + Sync>;

// Create callback that broadcasts to WebSocket
let ws_tx = ws_broadcast.clone();
let item_id = item.id;
let progress_callback: ArcProgressCallback = Arc::new(move |bytes, total, speed| {
    let _ = ws_tx.send(WsEvent::DownloadProgress {
        item_id,
        bytes_downloaded: bytes,
        total_bytes: total,
        progress_pct: (bytes as f64 / total as f64) * 100.0,
        speed_kbps: speed,
    });
});

// Pass to spawned task
tokio::spawn(async move {
    receive_file_data(file_conn, filesize, path, Some(progress_callback)).await
});
```

---

## Implementation Order

### Phase 1: Database & Models
1. Add migration for `search_queue` and `active_transfers` tables
2. Add model structs (`QueuedSearch`, `ActiveTransfer`)
3. Add database methods for queue operations

### Phase 2: Queue Service Core
4. Create `QueueService` struct
5. Implement `enqueue_search` and `enqueue_list`
6. Implement `get_next_pending_search`

### Phase 3: Spawned Transfers
7. Create `DownloadInitResult` enum in client.rs
8. Refactor `download_file` → `initiate_download` (stops at FileConnection)
9. Update `receive_file_data` to accept `ArcProgressCallback`

### Phase 4: Workers
10. Implement queue worker loop
11. Implement transfer monitor
12. Start both in main.rs

### Phase 5: API & WebSocket
13. Add new API endpoints
14. Modify existing endpoints to use queue
15. Add new WebSocket events

### Phase 6: Testing & Polish
16. Test single search flow
17. Test list submission flow
18. Test concurrent downloads
19. Test error handling (failed searches, failed transfers)
20. Test restart recovery (pending queue items)

---

## Files to Modify/Create

| File | Action | Description |
|------|--------|-------------|
| `migrations/XXXXXX_search_queue.sql` | Create | New tables |
| `src/models/mod.rs` | Modify | Add QueuedSearch, ActiveTransfer |
| `src/db/mod.rs` | Modify | Add queue database methods |
| `src/services/queue.rs` | Create | QueueService, workers |
| `src/services/mod.rs` | Modify | Export QueueService |
| `src/protocol/client.rs` | Modify | Add initiate_download, DownloadInitResult |
| `src/protocol/peer.rs` | Modify | Update receive_file_data signature |
| `src/api/queue.rs` | Create | New queue endpoints |
| `src/api/mod.rs` | Modify | Add queue routes |
| `src/api/ws.rs` | Modify | Add new event types |
| `src/main.rs` | Modify | Start queue worker and transfer monitor |

---

## Estimated Effort

| Phase | Effort |
|-------|--------|
| Phase 1: Database & Models | 1 hour |
| Phase 2: Queue Service Core | 1-2 hours |
| Phase 3: Spawned Transfers | 2 hours |
| Phase 4: Workers | 2 hours |
| Phase 5: API & WebSocket | 1-2 hours |
| Phase 6: Testing | 1-2 hours |
| **Total** | **8-11 hours** |

---

## Design Decisions

1. **Queue persistence on restart**: Yes - pending queue items resume after backend restart

2. **Maximum concurrent transfers**: No limit - transfers are independent tasks, system resources are the only constraint

3. **Queue priority**: FIFO (first in, first out)

4. **Duplicate handling**: Yes - use existing fuzzy matching system (note: needs refinement in future)

5. **List failure handling**: Yes - continue processing queue when individual searches fail
