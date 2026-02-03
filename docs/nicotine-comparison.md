# Nicotine+ vs Rinse Protocol Comparison

## Executive Summary

After deep analysis of both implementations, I've identified several differences that likely contribute to our lower success rate with peers. The most significant issues are:

1. **Missing TCP_NODELAY** - We don't set this, causing potential packet delays
2. **No connection reuse** - We create new connections for each operation
3. **No CantConnectToPeer feedback** - We don't tell the server when connections fail
4. **Peer state issues** - Failed downloads may cause peers to temporarily reject us

---

## Detailed Comparison

### 1. Socket Configuration

**Nicotine+:**
```python
sock.setblocking(False)
sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
```

**Rinse:**
```rust
// We don't set TCP_NODELAY at all
TcpStream::connect(&addr).await
```

**Impact:** TCP_NODELAY disables Nagle's algorithm, ensuring small packets (like handshake messages) are sent immediately. Without it, our handshake messages might be delayed, potentially causing timing issues with peers that have short timeouts.

**Fix:** Add `stream.set_nodelay(true)?` after connecting.

---

### 2. Connection Architecture

**Nicotine+:**
- Single event loop using `selectors` for ALL connections
- Maintains persistent connections per peer/connection-type
- Reuses existing connections when sending messages to the same peer
- Connection keyed by `username + conn_type`

```python
def _send_message_to_peer(self, username, msg):
    init_key = username + conn_type
    if init_key in self._username_init_msgs:
        # Reuse existing connection
        init = self._username_init_msgs[init_key]
        init.outgoing_msgs.append(msg)
```

**Rinse:**
- Spawns a new task per connection
- Creates new connections for each search result retrieval
- Stores connections in `peer_streams` LruCache but doesn't reuse them for search results

**Impact:**
- We may be seen as making "too many connections" by peers
- We lose the benefit of established connections
- More connection overhead

---

### 3. Indirect Connection Feedback

**Nicotine+:**
```python
def _connect_error(self, error, conn):
    pierce_token = conn.pierce_token
    if pierce_token is not None:
        # Tell server we couldn't connect
        self._send_message_to_server(CantConnectToPeer(pierce_token, username))
```

**Rinse:**
- We don't send `CantConnectToPeer` when connection attempts fail
- The server and peer don't know we failed to connect

**Impact:** The peer continues waiting for our connection, wasting their resources. This could affect our reputation with peers or the server.

---

### 4. Token and Connection Timeout

**Nicotine+:**
```python
INDIRECT_REQUEST_TIMEOUT = 20  # seconds
```
Peers expire their pending indirect connection tokens after 20 seconds.

**Rinse:**
- Our TCP connect timeout is 5 seconds
- We respond quickly (< 1 second typically)
- But we saw peers closing connections immediately

**Finding from logs:** The EOF errors happen in < 500ms, well within timeout. This suggests token/state issues rather than timing.

---

### 5. Connection Lifecycle Analysis

Looking at the logs, `beatrice.songbird2`:
- **Search 1:** Succeeded - gave us 2 files
- **Download attempt:** Failed after 30 seconds (F connection timeout)
- **Search 2:** Failed immediately - closed connection on handshake

**Hypothesis:** After our failed download attempt, the peer may have:
1. Marked us as a "bad" peer temporarily
2. Had connection state corruption
3. Been rate-limiting us

**Nicotine+ handles this** by maintaining cleaner connection state and properly closing connections after use.

---

### 6. Message Framing Comparison

**PierceFireWall message format:**

| Field | Nicotine+ | Rinse |
|-------|-----------|-------|
| Length | `pack_uint32(4 + 1) = 5` | `write_u32_le(5)` ✓ |
| Type | `pack_uint8(0)` | `write_u8(0)` ✓ |
| Token | `pack_uint32(token)` | `write_u32_le(token)` ✓ |

**Format is identical** - not the cause of failures.

**PeerInit message format:**

| Field | Nicotine+ | Rinse |
|-------|-----------|-------|
| Length | `len(content) + 1` | `1 + 4 + username.len + 4 + conn_type.len + 4` ✓ |
| Type | `1` | `1` ✓ |
| Username | our username | our username ✓ |
| ConnType | "P"/"F"/"D" | "P"/"F" ✓ |
| Token | `0` (legacy) | `token` (we pass the actual token) ⚠️ |

**Potential issue:** We pass the actual token in PeerInit, but Nicotine+ always sends `0`. The protocol doc says "token is always zero and ignored today." This might not matter, but it's a difference.

---

### 7. Search Result Flow

**Nicotine+ (when receiving ConnectToPeer for type "P"):**
1. Server sends ConnectToPeer with peer's address and pierce_token
2. Create PeerInit with `target_user=username`
3. Connect to peer address
4. Send PierceFireWall(pierce_token)
5. Wait for peer to send FileSearchResponse
6. Forcibly close connection after receiving results (to prevent pileup)

**Rinse:**
1. Server sends ConnectToPeer
2. Spawn a task to connect
3. Connect to peer address
4. Send PierceFireWall(peer_token)
5. Wait for response (PeerInit or FileSearchResponse)
6. Parse response
7. Store connection in LruCache

**Difference:** We expect a PeerInit response, but Nicotine+ just waits for FileSearchResponse directly after sending PierceFireWall.

---

### 8. Listen Port and Incoming Connections

**Observation:** We have ZERO incoming connections in the logs.

**Nicotine+ flow:**
1. Peer tries to connect to our listen port directly
2. Peer sends PeerInit
3. We parse it and associate with peer
4. Peer sends FileSearchResponse
5. We close connection after receiving

**Our flow:**
1. Peer tries to connect to our listen port
2. ... (nothing happens in logs)

**Possible issues:**
- UPnP might not be working correctly despite success logs
- Firewall might be blocking incoming connections
- Our listener might have a bug

---

## Recommended Fixes (Priority Order)

### High Priority

1. **Add TCP_NODELAY**
   ```rust
   stream.set_nodelay(true)?;
   ```

2. **Send CantConnectToPeer on failure**
   ```rust
   // When connection fails
   send_message_to_server(CantConnectToPeer { token, username }).await;
   ```

3. **Verify incoming connections work**
   - Test with external port checker
   - Add more detailed logging to listener
   - Verify UPnP is actually working

### Medium Priority

4. **Simplify post-PierceFireWall handling**
   - Don't expect PeerInit response
   - Wait directly for FileSearchResponse (code 9)

5. **Send token=0 in PeerInit**
   - Match Nicotine+ behavior exactly

6. **Implement connection reuse**
   - Track connections by username+type
   - Reuse for subsequent messages

### Lower Priority

7. **Implement connection limits**
   - MAX_SOCKETS like Nicotine+
   - Proper back-pressure

8. **Clean connection lifecycle**
   - Close connections after search results
   - Proper error handling throughout

---

## Test Plan

After implementing fixes:

1. Run search and verify TCP_NODELAY is set
2. Check if CantConnectToPeer is sent on failures
3. Monitor for incoming connections
4. Compare success rate with before
5. Check if same peer succeeds on retry (no blacklisting)

---

## Log Evidence

### Success pattern (beatrice.songbird2, Search 1):
```
01:29:30.700002Z ConnectToPeer #12: beatrice.songbird2
01:29:31.323622Z Got 2 files from 'beatrice.songbird2' (FREE, 5754 KB/s)
```
Time: 623ms, Result: 2 files

### Failure pattern (beatrice.songbird2, Search 2):
```
01:29:39.212235Z [Download] Initiating download from 'beatrice.songbird2'
01:30:09.584006Z ERROR Failed to establish F connection: Timeout
01:30:10.586280Z ConnectToPeer #3: beatrice.songbird2
01:30:10.915377Z Failed ... handshake read error: unexpected end of file
```
Time: 329ms, Result: Connection closed by peer (possibly due to failed download)
