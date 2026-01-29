# Soulseek Protocol Reference

## Complete File Transfer Flow

### Overview
The Soulseek protocol has two methods for initiating downloads:
1. **Modern method**: Downloader sends QueueUpload, waits for uploader's TransferRequest
2. **Legacy method**: Downloader sends TransferRequest directly

Both methods result in the same file connection flow: **THE UPLOADER ALWAYS INITIATES THE F CONNECTION TO THE DOWNLOADER**.

---

## Connection Types

| Type | Code | Purpose |
|------|------|---------|
| P | "P" | Peer messages (search results, transfer negotiation, chat) |
| F | "F" | File data transfer |
| D | "D" | Distributed network (search relay) |

---

## Message Formats

### Peer Messages (over "P" connection)

#### QueueUpload (Code 43) - Modern Method
Downloader sends to request file be added to uploader's queue.
```
[length:u32][code:u32=43][filename:string]
```

#### TransferRequest (Code 40)
**Sent by uploader** when ready to transfer (modern), or **sent by downloader** to request (legacy).
```
[length:u32][code:u32=40][direction:u32][token:u32][filename:string][filesize:u64 if direction=1]
```
- direction=0: Download request (legacy, sent by downloader)
- direction=1: Upload notification (modern, sent by uploader)

#### TransferReply (Code 41)
Response to TransferRequest.
```
If allowed:
[length:u32][code:u32=41][token:u32][allowed:u8=1][filesize:u64]

If denied:
[length:u32][code:u32=41][token:u32][allowed:u8=0][reason:string]
```

#### PlaceInQueueRequest (Code 51)
Ask uploader for queue position.
```
[length:u32][code:u32=51][filename:string]
```

#### PlaceInQueueResponse (Code 44)
```
[length:u32][code:u32=44][filename:string][place:u32]
```

#### UploadDenied (Code 50)
```
[length:u32][code:u32=50][filename:string][reason:string]
```

### File Messages (over "F" connection)

#### FileTransferInit
First message on F connection. **Sent by uploader**.
```
[token:u32]   # Just 4 bytes, no length prefix
```

#### FileOffset
Response to FileTransferInit. **Sent by downloader**.
```
[offset:u64]  # Just 8 bytes, no length prefix
```

After FileOffset, uploader streams raw file bytes until complete.
**IMPORTANT: Downloader closes the connection to signal completion.**

---

## Complete Download Flow (Modern Method)

```
DOWNLOADER                          UPLOADER
    |                                   |
    |------- QueueUpload (43) --------->|  "Please queue this file"
    |                                   |
    |        [Uploader queues file]     |
    |                                   |
    |<------ TransferRequest (40) ------|  "Ready to upload, here's token & size"
    |                                   |
    |------- TransferReply (41) ------->|  "OK, I accept" (allowed=1)
    |                                   |
    |<=== UPLOADER OPENS F CONNECTION ==|  (or indirect via server)
    |                                   |
    |<------ FileTransferInit ----------|  [token:u32]
    |                                   |
    |------- FileOffset --------------->|  [offset:u64] (0 for new download)
    |                                   |
    |<====== RAW FILE BYTES ============|
    |                                   |
    |-- DOWNLOADER CLOSES CONNECTION -->|  Signals completion
```

---

## Complete Download Flow (Legacy Method)

```
DOWNLOADER                          UPLOADER
    |                                   |
    |--- TransferRequest (40, dir=0) -->|  "I want to download this file"
    |                                   |
    |<------ TransferReply (41) --------|  "OK" (allowed=1, filesize)
    |                                   |
    |<=== UPLOADER OPENS F CONNECTION ==|  (or indirect via server)
    |                                   |
    |<------ FileTransferInit ----------|  [token:u32]
    |                                   |
    |------- FileOffset --------------->|  [offset:u64]
    |                                   |
    |<====== RAW FILE BYTES ============|
    |                                   |
    |-- DOWNLOADER CLOSES CONNECTION -->|
```

---

## Indirect Connections (When Direct Fails)

When the uploader cannot directly connect to the downloader:

1. Uploader sends **ConnectToPeer (18)** to server with:
   - `type="F"`
   - `token` (indirect token)
   - `username` (downloader's name)

2. Server relays ConnectToPeer to downloader

3. Downloader receives ConnectToPeer with uploader's IP/port

4. **Downloader connects to uploader** and sends **PierceFirewall**:
   ```
   [length:u32=5][type:u8=0][token:u32]
   ```

5. Once connection established, continue with FileTransferInit flow

---

## F Connection Handshake Details

### Direct Connection (uploader connects to downloader)

```
UPLOADER                            DOWNLOADER
    |                                   |
    |==== TCP CONNECT to port 2234 ====>|
    |                                   |
    |------- PeerInit ----------------->|  [len][type=1][username][conntype="F"][token=0]
    |                                   |
    |<------ PeerInit ------------------|  [len][type=1][username][conntype="F"][token=0]
    |                                   |
    |------- FileTransferInit --------->|  [transfer_token:u32]
    |                                   |
    |<------ FileOffset ----------------|  [offset:u64]
    |                                   |
    |======= RAW FILE DATA ============>|
```

**Note**: The token in PeerInit is always 0. The actual transfer token is in FileTransferInit.

### Indirect Connection (downloader connects to uploader via server coordination)

```
UPLOADER                   SERVER                  DOWNLOADER
    |                         |                         |
    |-- ConnectToPeer(F) ---->|                         |
    |                         |---- ConnectToPeer ----->|
    |                         |    (uploader IP/port)   |
    |<================ TCP CONNECT =====================|
    |                         |                         |
    |<---- PierceFirewall ----|-------------------------|  [len=5][type=0][token]
    |                         |                         |
    |-- PeerInit (optional) ->|------------------------>|
    |                         |                         |
    |-- FileTransferInit ---->|------------------------>|  [transfer_token:u32]
    |                         |                         |
    |<---- FileOffset --------|-------------------------|  [offset:u64]
    |                         |                         |
    |======= RAW FILE DATA ==>|========================>|
```

---

## Current Implementation Issues

### What We Have Working:
1. Server connection, login, search
2. Receiving ConnectToPeer for search results (type "P")
3. Direct peer connections for search results
4. Sending TransferRequest, receiving TransferReply (allowed=1)

### What's NOT Working:
1. **File transfer after TransferReply** - Peer can't connect to us

### Root Causes:
1. **NAT/Firewall**: Uploader tries to connect to us, but we're behind NAT
2. **Indirect Connection Handling**: We may not be handling ConnectToPeer type="F" from server
3. **Listener Issues**: Our listener on port 2234 may not be properly handling F connections

### Required Fixes:

1. **Handle ConnectToPeer type="F"** from server:
   - When we receive ConnectToPeer with type="F", the uploader is asking us to connect to THEM
   - We should connect to their IP:port and send PierceFirewall with the token
   - Then wait for FileTransferInit

2. **Proper listener handling for F connections**:
   - When uploader connects directly to us with PeerInit type="F"
   - Respond with PeerInit type="F"
   - Wait for FileTransferInit
   - Send FileOffset
   - Receive file data

3. **Verify UPnP is working**:
   - Port 2234 must be accessible from internet
   - Test by checking if peers can connect to us

---

## Server Messages We Need to Handle

### ConnectToPeer (Code 18) - FROM Server
Server telling us a peer wants to connect:
```
[length:u32][code:u32=18][username:string][type:string][ip:4bytes][port:u32][token:u32]
```

When type="F":
- This means an uploader wants US to connect to THEM (indirect)
- We should connect to their ip:port
- Send PierceFirewall with the token
- Then the F connection flow continues

When type="P":
- This is for search results (what we already handle)

---

## Testing Strategy

1. **First**: Verify listener receives connections properly
2. **Second**: Handle ConnectToPeer type="F" - connect back to peer
3. **Third**: Full download test with proper F connection handling

---

## Sources

- [Nicotine+ Protocol Documentation](https://nicotine-plus.org/doc/SLSKPROTOCOL.html)
- [aioslsk Documentation](https://aioslsk.readthedocs.io/en/latest/SOULSEEK.html)
- [Museek+ Protocol Docs](https://museek-plus.sourceforge.net/soulseekprotocol.html)
- Nicotine+ source code analysis
