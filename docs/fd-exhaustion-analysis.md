# File Descriptor Exhaustion Analysis

## The Problem

After implementing an LRU cache for `peer_streams`, we still see "Too many open files" errors immediately when starting a search:

```
2026-02-01T11:51:20.661871Z ERROR [Listener] Accept error on obfuscated port: Too many open files (os error 24)
```

The errors occur on `accept()` itself - meaning we hit the FD limit BEFORE we can even accept new connections.

## Why the LRU Cache Didn't Fix It

The LRU cache limits how many connections we **store for later reuse**. But the error happens during `accept()` - at this point:

1. The connection hasn't been accepted yet
2. The connection hasn't been stored in `peer_streams` yet
3. We're failing because OTHER file descriptors are exhausted

The LRU cache only helps with long-term accumulation. It doesn't help with **burst scenarios** where hundreds of peers connect simultaneously.

## Root Cause: Burst of Concurrent Connections

When you search on Soulseek:

1. Your search is broadcast to potentially thousands of peers
2. Hundreds of peers respond nearly simultaneously
3. Each peer opens a connection to YOUR listener ports (2234, 2235)
4. Each accepted connection = 1 file descriptor
5. Each spawned handler task holds that FD until complete

### Timeline of a Search

```
T+0ms:    Search sent
T+100ms:  First peer connects → spawn task → 1 FD
T+110ms:  10 more peers connect → spawn tasks → 11 FDs
T+200ms:  50 more peers → 61 FDs
T+300ms:  100 more peers → 161 FDs
T+400ms:  150 more peers → 311 FDs  ← EXCEEDS DEFAULT LIMIT (256)
T+500ms:  First tasks completing, FDs released
```

The problem is the **rate of incoming connections** exceeds the **rate of task completion**.

### Default macOS/Linux Limits

```bash
$ ulimit -n
256   # macOS default (sometimes 1024)
```

256 FDs for the entire process must cover:
- Server connection to slsknet.org (1)
- Listener on port 2234 (1)
- Listener on port 2235 (1)
- SQLite database connection (1-5)
- WebSocket connections (1 per browser tab)
- Log file handles (1-2)
- Active download files (1 per download)
- **Peer connections: 200-500 during search burst**

## Other Potential FD Leaks

### 1. Spawned Tasks Never Complete

```rust
tokio::spawn(async move {
    match handle_incoming_connection(stream, &our_username).await {
        // If this hangs, the stream stays open forever
    }
});
```

If `handle_incoming_connection` or `receive_search_results_from_peer` blocks/hangs, the task never completes and the FD is never released.

### 2. F Connection Channel

```rust
// Line 420 in client.rs
let _ = file_conn_tx.send(conn).await;
```

F connections not matched to pending downloads are sent to a channel. If nothing consumes `file_conn_rx`, these connections accumulate with their FDs.

### 3. Pending Downloads Map

```rust
pending_downloads: Arc<Mutex<HashMap<u32, PendingDownload>>>
```

If pending downloads are never resolved (e.g., peer never sends F connection), they stay in the map forever. Each `PendingDownload` might hold resources.

### 4. Outgoing Connections During Download

When initiating a download, we may create outgoing connections:
- P connection to negotiate
- F connection to receive file

If these fail or timeout, are they properly cleaned up?

## Potential Solutions

### Solution 1: Increase ulimit (Immediate Workaround)

```bash
# In terminal before running backend
ulimit -n 4096

# Or add to ~/.zshrc or ~/.bashrc
ulimit -n 4096
```

**Pros**: Immediate fix, no code changes
**Cons**: Doesn't fix the underlying issue, might just delay the problem

### Solution 2: Semaphore to Limit Concurrent Handlers

```rust
const MAX_CONCURRENT_PEER_HANDLERS: usize = 100;

// In start_listener_on_port
let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_PEER_HANDLERS));

loop {
    match listener.accept().await {
        Ok((stream, addr)) => {
            let permit = semaphore.clone().acquire_owned().await.unwrap();
            tokio::spawn(async move {
                // Handle connection
                drop(permit); // Release when done
            });
        }
        // ...
    }
}
```

**Pros**: Bounds concurrent FD usage, back-pressure on connections
**Cons**: Some peers might timeout waiting, need to tune the limit

### Solution 3: Connection Timeout

```rust
tokio::spawn(async move {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        handle_incoming_connection(stream, &our_username)
    ).await;

    match result {
        Ok(Ok(conn)) => { /* process */ }
        Ok(Err(e)) => { /* handle error */ }
        Err(_) => { /* timeout - connection dropped automatically */ }
    }
});
```

**Pros**: Prevents hung connections from accumulating
**Cons**: Might drop slow but valid peers

### Solution 4: Process Connections Inline (No Spawn)

Instead of spawning a task for each connection, process them sequentially or in a bounded pool:

```rust
// Use a channel with bounded capacity
let (conn_tx, mut conn_rx) = mpsc::channel(100);

// Listener just sends to channel
tokio::spawn(async move {
    loop {
        if let Ok((stream, addr)) = listener.accept().await {
            let _ = conn_tx.send((stream, addr)).await;
        }
    }
});

// Fixed number of workers process connections
for _ in 0..10 {
    tokio::spawn(async move {
        while let Some((stream, addr)) = conn_rx.recv().await {
            handle_incoming_connection(stream, &our_username).await;
        }
    });
}
```

**Pros**: Strict bound on concurrent handlers
**Cons**: More complex, potential bottleneck

### Solution 5: Drain Unused F Connections

Add a task that drains `file_conn_rx` with a timeout, closing connections that aren't claimed:

```rust
tokio::spawn(async move {
    while let Some(conn) = file_conn_rx.recv().await {
        // If not claimed within 5 seconds, drop it
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(conn); // Close if still held
        });
    }
});
```

## Recommended Approach

**Immediate**: Increase ulimit to 4096

**Short-term**: Implement Solution 2 (Semaphore) + Solution 3 (Timeout)
- Limits concurrent handlers to ~100
- Times out slow connections after 10-15 seconds
- Prevents both burst exhaustion and slow leak

**Long-term**: Investigate all FD sources
- Add logging to track FD count: `ls /proc/self/fd | wc -l` (Linux) or `lsof -p $PID | wc -l` (macOS)
- Ensure file_conn_rx is being drained
- Add periodic cleanup of stale entries in all maps

## Testing

After implementing fixes:

1. Start backend with low ulimit: `ulimit -n 256 && cargo run`
2. Perform several searches
3. Monitor for "Too many open files" errors
4. Check FD count: `lsof -p $(pgrep rinse) | wc -l`

## Related Code Locations

- Listener: `src/protocol/client.rs` lines 280-485
- F connection channel: `file_conn_tx` / `file_conn_rx`
- Pending downloads: `pending_downloads` HashMap
- Peer streams: `peer_streams` LruCache (now bounded)
