# Concurrent Download Architecture Analysis

## Current Architecture Overview

### Component Structure
```
DownloadService
    └── slsk_client: Arc<RwLock<Option<SoulseekClient>>>
            └── SoulseekClient
                    ├── server_stream: TcpStream (single connection to server.slsknet.org)
                    ├── file_conn_rx: Arc<Mutex<mpsc::Receiver<FileConnection>>>
                    ├── peer_streams: Arc<Mutex<HashMap<String, TcpStream>>>
                    ├── incoming_results: Arc<Mutex<Vec<SearchResult>>>
                    └── pending_downloads: Arc<Mutex<HashMap<u32, PendingDownload>>>
```

### Current Flow for List Downloads
1. `search_and_download_list` spawns N tasks (one per query)
2. Semaphore limits to 3 concurrent tasks
3. Each task calls `search_and_download_item_with_timeout`
4. Each call acquires `slsk_client.write().await` (exclusive lock)
5. Lock held for ENTIRE duration of search + download
6. Lock released, next task acquires

---

## Issues Identified

### Issue 1: RwLock Serialization (Critical)
**Location**: `download.rs:100`
```rust
let mut client_guard = self.slsk_client.write().await;
```

**Problem**: The write lock is held for the entire search + download operation (potentially 15-60+ seconds). Even with 3 semaphore permits, only ONE task can execute at a time because they all serialize on this lock.

**Impact**: List downloads are effectively sequential despite concurrent task spawning.

---

### Issue 2: Single Server Stream (Architectural)
**Location**: `client.rs:105`
```rust
server_stream: TcpStream
```

**Problem**: Soulseek uses a single TCP connection to the server for:
- Search requests and responses
- GetPeerAddress requests
- ConnectToPeer messages (for both P and F connections)

A TcpStream can only have one reader at a time. Methods that read from it (`search`, `download_file`, `get_peer_address`) require `&mut self`.

**Impact**: True concurrent operations require multiplexing this stream.

---

### Issue 3: Search Result Collision (Bug)
**Location**: `client.rs:384`
```rust
self.incoming_results.lock().await.clear();
```

**Problem**: When a search starts, it clears `incoming_results`. The listener populates this vec when peers connect to us with search results. If two searches overlap:
1. Search A starts, clears incoming_results
2. Peers start connecting with results for Search A
3. Search B starts, clears incoming_results (erasing A's results!)
4. Search A finishes, gets incomplete results

**Impact**: Concurrent searches would lose results from peers that connected to us.

---

### Issue 4: File Transfer Blocks Everything
**Location**: `peer.rs:976-1204` (`download_file` function)

**Problem**: The file transfer phase (receiving bytes) holds the client lock even though it only needs the `FileConnection` stream, not the server connection.

**Impact**: A large file download (e.g., FLAC at 30MB) blocks all other operations for the entire transfer duration.

---

### Issue 5: Progress/Completion Callback Architecture
**Location**: `download.rs:240-255`

**Problem**: Progress callbacks are `Box<dyn Fn>` passed through the call stack. For spawned background tasks, we'd need:
- `Arc<dyn Fn>` for shared ownership
- A completion callback mechanism
- Thread-safe status updates

**Impact**: Spawning file transfers as background tasks requires callback refactoring.

---

## Proposed Solutions

### Solution A: Quick Win - Split Lock Scope
**Complexity**: Low | **Impact**: Medium

Split `search_and_download_item_with_timeout` into two lock acquisitions:
```rust
// Phase 1: Search (release lock after)
let (results, scored_file) = {
    let mut guard = self.slsk_client.write().await;
    // ... search and select file ...
}; // lock released

// Phase 2: Download (re-acquire)
let mut guard = self.slsk_client.write().await;
// ... download ...
```

**Pros**:
- Minimal code changes
- Allows search A → search B → download A → download B interleaving

**Cons**:
- Still serializes on the lock (just with smaller critical sections)
- Search result collision issue remains
- File transfers still block

---

### Solution B: Spawn File Transfers
**Complexity**: Medium | **Impact**: High

After negotiation succeeds and we have a `FileConnection`, spawn the file receive as an independent task:
```rust
// In download_file, after getting FileConnection:
let task_handle = tokio::spawn(async move {
    receive_file_data(file_conn, filesize, output_path, progress_callback).await
});
return DownloadResult::TransferStarted { task_id };
```

**Pros**:
- File transfers don't block other operations
- Uses already-implemented `receive_file_data` function
- Significant speedup for lists with large files

**Cons**:
- Requires completion callback mechanism
- Status updates need thread-safe handling
- Search phase still serialized

---

### Solution C: Server Stream Multiplexer
**Complexity**: High | **Impact**: Very High

Create a dedicated task that reads from `server_stream` and dispatches messages to appropriate channels:
```rust
struct ServerMultiplexer {
    search_responses: HashMap<u32, mpsc::Sender<ConnectToPeer>>,
    peer_address_responses: mpsc::Sender<PeerAddress>,
    connect_to_peer_f: mpsc::Sender<ConnectToPeerF>,
}

// Single reader task
async fn server_reader(stream: TcpStream, mux: Arc<ServerMultiplexer>) {
    loop {
        let (code, data) = read_message(&stream).await;
        match code {
            18 => mux.dispatch_connect_to_peer(data),
            3 => mux.dispatch_peer_address(data),
            // ...
        }
    }
}
```

**Pros**:
- Enables true concurrent searches
- Clean separation of concerns
- Fixes search result collision issue

**Cons**:
- Significant refactor of client architecture
- Need to handle message routing and timeouts
- More complex error handling

---

### Solution D: Per-Search Channels
**Complexity**: Medium | **Impact**: Medium

Instead of shared `incoming_results`, each search gets its own channel:
```rust
pub async fn search(&mut self, query: &str) -> Result<(SearchReceiver, SearchToken)> {
    let token = rand::random();
    let (tx, rx) = mpsc::channel(50);
    self.active_searches.insert(token, tx);
    Self::send_search(&mut self.server_stream, token, query).await?;
    Ok((rx, token))
}
```

**Pros**:
- Fixes search result collision
- Searches can overlap without losing results
- Moderate complexity

**Cons**:
- Still need to address server_stream contention
- Listener needs to route by search token

---

### Solution E: Pipeline Architecture
**Complexity**: Medium-High | **Impact**: High

Separate the process into distinct phases with their own concurrency:
```
Phase 1: Search Queue (sequential, uses server_stream)
    ↓
Phase 2: Download Negotiation Queue (sequential, uses server_stream + peer_streams)
    ↓
Phase 3: File Transfer Pool (concurrent, only uses FileConnections)
```

**Pros**:
- Clear separation of serial vs parallel work
- File transfers fully concurrent
- Easier to reason about

**Cons**:
- Requires refactoring download service interface
- More complex state management
- Results not available until phase completion

---

## Recommendation

For immediate improvement with manageable effort:

**Phase 1**: Implement **Solution B (Spawn File Transfers)**
- Highest impact for list downloads
- Uses existing `receive_file_data` function
- Moderate implementation effort

**Phase 2** (Future): Implement **Solution D (Per-Search Channels)**
- Fixes the search collision bug
- Prepares for more concurrent architecture

**Phase 3** (Future): Consider **Solution C (Server Multiplexer)**
- Only if true concurrent searches become a requirement
- Significant effort, save for when needed

---

## Questions for Decision

1. **How important is concurrent search?** If lists typically search one-at-a-time anyway, Solution B alone may be sufficient.

2. **What's the typical file size?** For small MP3s (5-10MB), the file transfer blocking time is brief. For FLACs (30-50MB), it's significant.

3. **Acceptable complexity level?** Solution B is isolated to client.rs. Solutions C/D touch multiple files and change the architecture.

4. **Timeline?** Quick wins (A, B) can be done in a session. Full multiplexer (C) is a multi-session project.
