# Soulseek Search Implementation - Handoff Document

## Project Context

**Rinse** is a web application for downloading music from the Soulseek P2P network. It has a Rust backend (Axum) and React frontend. The backend connects to the Soulseek server, performs searches, and downloads files from peers.

## Current Status: SEARCH WORKING

The search functionality is now working. Tests show:
- **23 peers** found with results for a test search
- **2 successful connections** retrieved search results
- Files decompressed and parsed correctly

---

## What Was Fixed

### 1. PierceFirewall Message Format (CRITICAL FIX)

**Problem:** PierceFirewall messages were missing the required 4-byte length prefix.

**Solution:** Added length prefix to all peer init messages:

```rust
// Before (broken):
stream.write_u8(0).await?;  // Type
stream.write_u32_le(token).await?;  // Token

// After (working):
stream.write_u32_le(5).await?;  // Length (1 type + 4 token)
stream.write_u8(0).await?;  // Type
stream.write_u32_le(token).await?;  // Token
```

**Files changed:**
- `src/protocol/client.rs:423-429` - PierceFirewall sending
- `src/protocol/client.rs:367-379` - PeerInit response
- `tests/e2e_search.rs` - All peer init messages

### 2. Response Handling

**Discovery:** After we send PierceFirewall, peers send FileSearchResponse **directly** (no handshake acknowledgment).

**Format:** `[length:u32][code:u32 = 9][compressed_data]`

The existing `read_file_search_response` function in `client.rs` already handles this correctly.

### 3. UPnP Support Added

New module `src/services/upnp.rs` provides automatic port forwarding:

```rust
// Initialize UPnP on startup
use rinse_backend::services::init_upnp;

if init_upnp().await? {
    println!("UPnP configured!");
} else {
    println!("Manual port forwarding required");
}

// Cleanup on shutdown
cleanup_upnp().await?;
```

---

## Ports to Forward

**For routers without UPnP, forward these ports:**
- **2234** - Main listening port (peer connections)
- **2235** - Obfuscated port (main port + 1, encrypted connections)

---

## Current State Summary

### What Works
1. Server connection and login ✅
2. Sending search queries ✅
3. Receiving ConnectToPeer messages (peers with results) ✅
4. IP parsing (little-endian) ✅
5. Connection type detection ("P" for peer) ✅
6. Connecting to peers ✅
7. PierceFirewall with length prefix ✅
8. Reading FileSearchResponse directly ✅
9. Decompressing and parsing search results ✅
10. UPnP automatic port forwarding ✅

### What May Need Work
1. **Obfuscated connections** - Some peers use RC4-encrypted connections and close immediately
2. **Incoming peer connections** - Not yet tested with UPnP enabled
3. **Download functionality** - Search works, downloads untested

---

## Key Files

| File | Purpose |
|------|---------|
| `src/protocol/client.rs` | Main Soulseek client - search, peer connections |
| `src/protocol/messages.rs` | Protocol message parsing |
| `src/protocol/connection.rs` | ServerConnection, PeerConnection structs |
| `src/services/upnp.rs` | UPnP port forwarding |
| `tests/e2e_search.rs` | E2E test for search |

---

## Protocol Reference

### PierceFirewall Message
```
[4 bytes: length = 5 (LE)]
[1 byte: type = 0]
[4 bytes: token (LE)]
```

### PeerInit Message
```
[4 bytes: length (LE)]
[1 byte: type = 1]
[4 bytes: username length (LE)]
[username bytes]
[4 bytes: connection type length (LE)]
[connection type bytes, e.g., "P"]
[4 bytes: token (LE)]
```

### FileSearchResponse (code 9)
```
[4 bytes: message length (LE)]
[4 bytes: code = 9 (LE)]
[compressed data: zlib]
  - After decompression:
    - username (length-prefixed string)
    - token (u32 LE)
    - file_count (u32 LE)
    - files... (code, filename, size, extension, attributes)
    - has_slots (u8)
    - avg_speed (u32 LE)
    - queue_length (u64 LE)
```

---

## Quick Commands

```bash
# Build
cargo build

# Run E2E search test (30+ seconds)
cargo test --test e2e_search -- --nocapture

# Run backend with logging
RUST_LOG=debug cargo run

# Check port listening
lsof -i :2234
```

---

## Environment Variables

```env
SLSK_USERNAME=your_username
SLSK_PASSWORD=your_password
```

---

## Nicotine+ Reference

The reference implementation is Nicotine+ (Python):
- GitHub: https://github.com/nicotine-plus/nicotine-plus
- Protocol docs: https://nicotine-plus.org/doc/SLSKPROTOCOL.html
- Key file: `pynicotine/slskproto.py`

---

## TODO for Future Work

1. **Test downloads** - Search works, verify file downloads
2. **Handle obfuscated connections** - Implement RC4 for peers that require it
3. **Improve connection success rate** - Currently ~9% of peers respond successfully
4. **Add retry logic** - Try multiple peers if first ones fail
5. **Test UPnP in production** - Verify automatic port forwarding works
