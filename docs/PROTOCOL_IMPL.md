# Protocol Implementation Guide

This document tracks the differences between our Soulseek protocol implementation and the reference Nicotine+ client, along with fixes needed to improve reliability.

**Last Updated:** February 3, 2026

---

## Reference Materials

### Protocol Documentation
- **Soulseek Protocol Spec:** https://nicotine-plus.org/doc/SLSKPROTOCOL.html
- **Local copy should be fetched fresh** - Use WebFetch tool to get latest

### Nicotine+ Source Code (Local Reference)
- **Main protocol handling:** `backend/nicotine_reference/pynicotine/slskproto.py`
- **Message definitions:** `backend/nicotine_reference/pynicotine/slskmessages.py`
- **Search logic:** `backend/nicotine_reference/pynicotine/search.py`
- **Downloads:** `backend/nicotine_reference/pynicotine/downloads.py`
- **Transfers:** `backend/nicotine_reference/pynicotine/transfers.py`

### Our Implementation
- **Main client:** `backend/src/protocol/client.rs`
- **Peer protocol:** `backend/src/protocol/peer.rs`
- **Message types:** `backend/src/protocol/messages.rs`
- **File selection:** `backend/src/protocol/file_selection.rs`

---

## Current Issues Summary

Based on log analysis (February 2026):
- **21% peer connection success rate** (4/19 peers in sample search)
- **Zero incoming connections** on listener ports
- **Failed downloads cause subsequent search failures** with same peer
- **Many "unexpected end of file" errors** during handshake

---

## Identified Differences

### 1. TCP_NODELAY Not Set

**Status:** ✅ FIXED (Feb 3, 2026)

**Nicotine+ Code** (`slskproto.py:1677`):
```python
incoming_sock.setblocking(False)
incoming_sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
```

**Our Code** (`peer.rs`, `client.rs`):
```rust
// We don't set TCP_NODELAY anywhere
let mut stream = TcpStream::connect(&addr).await?;
```

**Impact:**
- Nagle's algorithm buffers small packets, causing delays
- Handshake messages (5-50 bytes) may be delayed up to 200ms
- Peers with short timeouts may close connection before receiving our messages

**Fix Required:**
```rust
stream.set_nodelay(true)?;
```

**Files to modify:**
- `src/protocol/peer.rs` - All `TcpStream::connect()` calls
- `src/protocol/client.rs` - Listener accept and outgoing connections

---

### 2. No CantConnectToPeer Feedback

**Status:** ✅ FIXED (Feb 3, 2026)

**Nicotine+ Code** (`slskproto.py:825-828`):
```python
def _connect_error(self, error, conn):
    pierce_token = conn.pierce_token
    if pierce_token is not None:
        self._send_message_to_server(CantConnectToPeer(pierce_token, username))
```

**Our Code:**
- We don't send any feedback when connection attempts fail
- Server and peer don't know we failed

**Impact:**
- Peer continues waiting for our connection (wastes their resources)
- May affect our reputation with the server/network
- Server can't attempt alternative routing

**Message Format** (Server Code 1001):
```
uint32 token      - The pierce_token from ConnectToPeer
string username   - The peer we couldn't connect to
```

**Files to modify:**
- `src/protocol/client.rs` - Add in ConnectToPeer failure handler
- `src/protocol/peer.rs` - `connect_to_peer_for_results()` error paths

---

### 3. PeerInit Token Should Be Zero

**Status:** ✅ FIXED (Feb 3, 2026)

**Nicotine+ Code** (`slskmessages.py:3038-3043`):
```python
def make_network_message(self):
    msg = bytearray()
    msg += self.pack_string(self.init_user)
    msg += self.pack_string(self.conn_type)
    msg += self.pack_uint32(0)  # Empty token - ALWAYS ZERO
    return msg
```

**Our Code** (`peer.rs:57-75`):
```rust
pub async fn send_peer_init(
    stream: &mut TcpStream,
    username: &str,
    conn_type: &str,
    token: u32,  // We pass and send the actual token
) -> std::io::Result<()> {
    // ...
    stream.write_u32_le(token).await?;  // Should always be 0
```

**Protocol Documentation:**
> "The token is always zero and ignored today, but used to be non-zero and included in a concurrent SendConnectToken server message for connection verification."

**Impact:** Unknown - token is supposedly ignored, but sending non-zero may confuse some clients.

**Files to modify:**
- `src/protocol/peer.rs` - `send_peer_init()` - always send 0

---

### 4. Post-PierceFireWall Response Handling

**Status:** ✅ IMPROVED (Feb 3, 2026)

**Nicotine+ Behavior:**
After sending PierceFireWall to respond to an indirect connection request:
1. Do NOT expect a PeerInit response
2. Wait directly for peer messages (FileSearchResponse, etc.)
3. The peer already knows who we are from the token match

**Our Code** (`peer.rs:473-510`):
```rust
// After sending PierceFireWall, we try to parse various responses
if first_u32 == 5 {
    // PierceFirewall response - shouldn't happen
} else if first_u32 > 5 && first_u32 < 200 {
    // PeerInit response - also shouldn't happen
} else if first_u32 > 100 && first_u32 < 10_000_000 {
    // FileSearchResponse - THIS is what we should expect
}
```

**What Should Happen:**
1. We connect and send PierceFireWall
2. Peer receives it, matches token, associates connection with pending request
3. Peer sends FileSearchResponse (code 9, zlib compressed)
4. We receive and parse it

**Files to modify:**
- `src/protocol/peer.rs` - `connect_to_peer_for_results()` - simplify response handling

---

### 5. Zero Incoming Connections

**Status:** 🔍 Needs Diagnosis

**Symptom:**
```
[SoulseekClient] Incoming connections: 0
```

Every search shows zero incoming connections on our listener ports (2234, 2235).

**Expected Behavior:**
- Peers should try connecting to us directly first
- Only if that fails should we receive ConnectToPeer from server
- We should see SOME incoming connections

**Possible Causes:**

1. **UPnP Not Actually Working**
   - Logs show success, but router might not honor the mapping
   - Test: Use external port checker

2. **Double NAT / CGNAT**
   - ISP-level NAT prevents port forwarding
   - Test: Check if external IP is in private range (10.x, 172.16-31.x, 192.168.x)

3. **Listener Bug**
   - Our listener might not be accepting correctly
   - Test: Add detailed logging, try connecting locally

**Diagnostic Steps:**
```bash
# 1. Get external IP
curl ifconfig.me

# 2. Check if port is open (from different network)
nc -vz <external-ip> 2234

# 3. Check local listener
nc -vz 127.0.0.1 2234

# 4. Check UPnP mappings
# (varies by router - check router admin page)

# 5. Check macOS firewall
# System Settings → Network → Firewall
```

---

### 6. Connection Reuse (Future Enhancement)

**Status:** 📋 Not Implemented (Lower Priority)

**Nicotine+ Approach:**
- Maintains connections keyed by `username + conn_type`
- Reuses existing connections for subsequent messages
- Single event loop manages all connections

**Our Approach:**
- Create new connection per operation
- Store in LruCache but don't reuse for search results
- Spawn separate task per connection

**Impact:**
- More connection overhead
- Peers may see us as "noisy"
- Miss optimization opportunities

**Files to modify (future):**
- `src/protocol/client.rs` - Connection management refactor

---

## Implementation TODO

### Phase 1: Quick Fixes (Do First)

- [x] **1.1 Add TCP_NODELAY to all connections** ✅ DONE (Feb 3, 2026)
  - Files modified: `peer.rs`, `client.rs`, `connection.rs`
  - Added `stream.set_nodelay(true)?` to all 10 connection points
  - **Results:** Success rate improved from 1.4-3.9% to 6.9% (2-5x improvement)
  - **Handshake EOF errors reduced from ~31 to ~10** (3x reduction)

- [x] **1.2 Change PeerInit token to always be 0** ✅ DONE (Feb 3, 2026)
  - File: `src/protocol/peer.rs`
  - Function: `send_peer_init()` - token param now ignored, always sends 0
  - Matches Nicotine+ behavior and protocol spec

### Phase 2: Protocol Corrections

- [x] **2.1 Implement CantConnectToPeer** ✅ DONE (Feb 3, 2026)
  - Added `send_cant_connect_to_peer()` helper in `peer.rs`
  - Added failure channel in search loop
  - Spawned tasks report failures via channel
  - After search, drains failures and sends CantConnectToPeer to server
  - Format: `[length][code=1001][token:u32][username:string]`

- [x] **2.2 Simplify post-PierceFireWall handling** ✅ DONE (Feb 3, 2026)
  - File: `src/protocol/peer.rs`
  - Function: `connect_to_peer_for_results()`
  - Clarified expected flow: FileSearchResponse expected directly after PierceFireWall
  - Added debug logging for unexpected PeerInit/PierceFireWall responses
  - Kept backward-compatible handling for robustness

### Phase 3: Diagnostics

- [ ] **3.1 Diagnose zero incoming connections**
  - Add detailed logging to listener in `client.rs`
  - Test port accessibility from external network
  - Verify UPnP is actually working
  - Check macOS firewall settings

- [ ] **3.2 Add connection metrics logging**
  - Track: connections attempted, succeeded, failed by reason
  - Track: incoming vs outgoing connection ratio
  - Track: time spent in each connection phase

---

## Next Steps (Further Diagnosis Needed)

After implementing the above fixes, these areas need investigation:

### 1. Why Do Peers Close Connections After Failed Downloads?

**Observation:** `beatrice.songbird2` worked on Search 1, but after our download failed (30s F connection timeout), they rejected us on Search 2.

**Questions:**
- Do peers blacklist us after failed transfers?
- Are we leaving connections in bad state?
- Is there cleanup we should do after failed downloads?

**Investigation:**
- Look at Nicotine+ download failure handling
- Check if we properly close connections on failure
- Look for any "ban" or "ignore" logic in Nicotine+

### 2. F Connection Establishment Issues

**Observation:** First download attempt failed with "Timeout waiting for file connection" despite peer being ready.

**Questions:**
- Is our `wait_for_file_connection` handling both paths correctly?
- Are we missing indirect F connection messages?
- Is there a race condition?

**Investigation:**
- Compare F connection flow with Nicotine+ `downloads.py`
- Add detailed logging to F connection waiting
- Check if we handle all ConnectToPeer type="F" cases

### 3. Connection Rate Limiting

**Questions:**
- Are we connecting too aggressively?
- Do peers have per-IP connection limits?
- Should we implement connection throttling?

**Investigation:**
- Look for rate limiting in Nicotine+
- Check if adding delays between connections helps
- Monitor if same peer works on retry after delay

### 4. Distributed Network Integration

**Observation:** We may not be fully participating in the distributed search network.

**Questions:**
- Are we handling D (distributed) connections?
- Should we be connecting to parent nodes?
- Are we missing search results that come via distributed network?

**Investigation:**
- Look at `_accept_child_peer_connection` in Nicotine+
- Check distributed message handling
- This is a larger feature - may explain why we get fewer results

---

## Instructions for Future Claude Instances

When working on this protocol implementation:

1. **Always reference Nicotine+ source code** - It's the most reliable Soulseek client
   - Path: `backend/nicotine_reference/pynicotine/`
   - Key files: `slskproto.py`, `slskmessages.py`

2. **Fetch fresh protocol documentation**
   - URL: https://nicotine-plus.org/doc/SLSKPROTOCOL.html
   - Use WebFetch tool to get current version

3. **Test with real network**
   - Protocol quirks only show with real peers
   - Use logs to trace exact message flow
   - Compare timing with expected behavior

4. **Read the logs carefully**
   - Log prefixes: `[Listener]`, `[Search]`, `[Download]`, `[Transfer]`
   - Look for patterns in failures
   - Check timing between messages

5. **When making changes:**
   - Change one thing at a time
   - Test thoroughly before moving to next fix
   - Update this document with findings

6. **Key Nicotine+ functions to study:**
   - `_initiate_connection_to_peer()` - How they start connections
   - `_establish_outgoing_peer_connection()` - Handshake completion
   - `_process_peer_init_message()` - Incoming handshake handling
   - `_connect_error()` - Error handling and CantConnectToPeer
   - `_file_search_response()` - Search result processing

7. **Our key functions:**
   - `connect_to_peer_for_results()` - Main search connection logic
   - `handle_incoming_connection()` - Listener handling
   - `send_peer_init()` / `send_pierce_firewall()` - Handshake messages
   - `start_listener_on_port()` - Incoming connection listener

---

## Changelog

### February 3, 2026
- Initial document created
- Identified 6 major differences with Nicotine+
- Created implementation TODO with phases
- Added diagnostic steps for incoming connections
- Added next steps for further investigation
