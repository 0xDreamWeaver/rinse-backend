use bytes::{Buf, BufMut, Bytes, BytesMut};
use std::collections::HashMap;
use anyhow::{Result, bail};

/// Server message codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ServerCode {
    Login = 1,
    SetWaitPort = 2,
    GetPeerAddress = 3,
    WatchUser = 5,
    UnwatchUser = 6,
    GetUserStatus = 7,
    SayChatroom = 13,
    JoinRoom = 14,
    LeaveRoom = 15,
    UserJoinedRoom = 16,
    UserLeftRoom = 17,
    ConnectToPeer = 18,
    MessageUser = 22,
    MessageAcked = 23,
    FileSearch = 26,
    SetStatus = 28,
    ServerPing = 32,
    SendUploadSpeed = 34,
    SharedFoldersFiles = 35,
    GetUserStats = 36,
    QueuedDownloads = 40,
    Relogged = 41,
    UserSearch = 42,
    AddUser = 51,
    GetRecommendations = 54,
    GetGlobalRecommendations = 56,
    GetUserInterests = 57,
    AdminCommand = 58,
    PlaceInLineResponse = 60,
    RoomAdded = 62,
    RoomRemoved = 63,
    RoomList = 64,
    ExactFileSearch = 65,
    AdminMessage = 66,
    GlobalUserList = 67,
    TunneledMessage = 68,
    PrivilegedUsers = 69,
    HaveNoParent = 71,
    SearchParent = 73,
    ParentMinSpeed = 83,
    ParentSpeedRatio = 84,
    ParentInactivityTimeout = 86,
    SearchInactivityTimeout = 87,
    MinParentsInCache = 88,
    DistribAliveInterval = 90,
    AddToPrivileged = 91,
    CheckPrivileges = 92,
    EmbeddedMessage = 93,
    AcceptChildren = 100,
    PossibleParents = 102,
    WishlistSearch = 103,
    WishlistInterval = 104,
    GetSimilarUsers = 110,
    GetItemRecommendations = 111,
    GetItemSimilarUsers = 112,
    RoomTickers = 113,
    RoomTickerAdd = 114,
    RoomTickerRemove = 115,
    SetRoomTicker = 116,
    AddHatedInterest = 117,
    RemoveHatedInterest = 118,
    RoomSearch = 120,
    SendUploadSpeed2 = 121,
    UserPrivileged = 122,
    GivePrivileges = 123,
    NotifyPrivileges = 124,
    AckNotifyPrivileges = 125,
    BranchLevel = 126,
    BranchRoot = 127,
    ChildDepth = 129,
    ResetDistributed = 130,
    PrivateRoomUsers = 133,
    PrivateRoomAddUser = 134,
    PrivateRoomRemoveUser = 135,
    PrivateRoomDropMembership = 136,
    PrivateRoomDropOwnership = 137,
    PrivateRoomUnknown = 138,
    PrivateRoomAdded = 139,
    PrivateRoomRemoved = 140,
    PrivateRoomToggle = 141,
    ChangePassword = 142,
    PrivateRoomAddOperator = 143,
    PrivateRoomRemoveOperator = 144,
    PrivateRoomOperatorAdded = 145,
    PrivateRoomOperatorRemoved = 146,
    PrivateRoomOwned = 148,
    MessageUsers = 149,
    JoinGlobalRoom = 150,
    LeaveGlobalRoom = 151,
    GlobalRoomMessage = 152,
    RelatedSearch = 153,
    CantConnectToPeer = 1001,
    CantCreateRoom = 1003,
}

/// Peer message codes
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PeerCode {
    GetSharedFileList = 4,
    SharedFileList = 5,
    FileSearchRequest = 8,
    FileSearchResult = 9,
    UserInfoRequest = 15,
    UserInfoReply = 16,
    FolderContentsRequest = 36,
    FolderContentsReply = 37,
    TransferRequest = 40,
    TransferReply = 41,
    PlaceholdUpload = 42,
    QueueUpload = 43,
    PlaceInQueueReply = 44,
    UploadFailed = 46,
    QueueFailed = 50,
    PlaceInQueueRequest = 51,
    UploadQueueNotification = 52,
}

/// File attributes for search results
#[derive(Debug, Clone)]
pub struct FileAttribute {
    pub attribute_type: u32,
    pub value: u32,
}

/// Search result file entry
#[derive(Debug, Clone)]
pub struct SearchFile {
    pub code: u8,
    pub filename: String,
    pub size: u64,
    pub extension: String,
    pub attributes: Vec<FileAttribute>,
}

impl SearchFile {
    /// Get bitrate from attributes (if present)
    pub fn bitrate(&self) -> Option<u32> {
        self.attributes.iter()
            .find(|attr| attr.attribute_type == 0)
            .map(|attr| attr.value)
    }

    /// Get duration from attributes (if present)
    pub fn duration(&self) -> Option<u32> {
        self.attributes.iter()
            .find(|attr| attr.attribute_type == 1)
            .map(|attr| attr.value)
    }

    /// Get VBR flag from attributes (if present)
    pub fn is_vbr(&self) -> Option<bool> {
        self.attributes.iter()
            .find(|attr| attr.attribute_type == 2)
            .map(|attr| attr.value == 1)
    }
}

/// Server messages
#[derive(Debug, Clone)]
pub enum ServerMessage {
    Login {
        username: String,
        password: String,
        version: u32,
        md5_hash: String,
        minor_version: u32,
    },
    LoginResponse {
        success: bool,
        reason: String,
        greeting: Option<String>,
        ip: Option<String>,
    },
    SetWaitPort {
        port: u32,
    },
    GetPeerAddress {
        username: String,
    },
    PeerAddress {
        username: String,
        ip: String,
        port: u16,
    },
    ConnectToPeer {
        token: u32,
        username: String,
        connection_type: String,
    },
    FileSearch {
        token: u32,
        query: String,
    },
    UserSearch {
        username: String,
        token: u32,
        query: String,
    },
    SetStatus {
        status: u32,
    },
    SharedFoldersFiles {
        folders: u32,
        files: u32,
    },
    GetUserStats {
        username: String,
    },
    UserStats {
        username: String,
        avg_speed: u32,
        uploads: u64,
        shared_files: u32,
        shared_folders: u32,
    },
    ServerPing,
    ServerPong,
    Relogged,
    MessageUser {
        username: String,
        message: String,
    },
    MessageUserReceived {
        message_id: u32,
        timestamp: u32,
        username: String,
        message: String,
    },
    RoomList,
    RoomListResponse {
        rooms: Vec<(String, u32)>,
        owned_private_rooms: Vec<(String, u32)>,
        private_rooms: Vec<(String, u32)>,
        operated_private_rooms: Vec<(String, u32)>,
    },
    /// Embedded message from distributed network (contains peer messages like search results)
    EmbeddedMessage {
        message_type: u8,
        data: Vec<u8>,
    },
    /// Unknown message (for debugging)
    Unknown {
        code: u32,
        data: Vec<u8>,
    },
    /// Server requests us to connect to a peer (indirect connection)
    ConnectToPeerRequest {
        username: String,
        connection_type: String,
        ip: String,
        port: u16,
        token: u32,
        privileged: bool,
    },
}

/// Peer messages
#[derive(Debug, Clone)]
pub enum PeerMessage {
    GetSharedFileList,
    SharedFileList {
        files: Vec<(String, u64)>,
    },
    FileSearchRequest {
        token: u32,
        query: String,
    },
    FileSearchResult {
        username: String,
        token: u32,
        files: Vec<SearchFile>,
        has_slots: bool,
        avg_speed: u32,
        queue_length: u64,
    },
    TransferRequest {
        direction: u32, // 0 = download, 1 = upload
        token: u32,
        filename: String,
    },
    TransferReply {
        token: u32,
        allowed: bool,
        filesize: u64,
    },
    QueueUpload {
        filename: String,
    },
    PlaceInQueueReply {
        filename: String,
        place: u32,
    },
    UploadFailed {
        filename: String,
    },
}

/// Helper functions for encoding/decoding
pub trait MessageEncoder {
    fn encode(&self) -> Result<Bytes>;
}

pub trait MessageDecoder: Sized {
    fn decode(buf: &mut BytesMut) -> Result<Option<Self>>;
}

/// Encode a string (length-prefixed)
pub fn encode_string(buf: &mut BytesMut, s: &str) {
    buf.put_u32_le(s.len() as u32);
    buf.put_slice(s.as_bytes());
}

/// Decode a string (length-prefixed)
pub fn decode_string(buf: &mut impl Buf) -> Result<String> {
    if buf.remaining() < 4 {
        bail!("Not enough bytes for string length");
    }
    let len = buf.get_u32_le() as usize;
    if buf.remaining() < len {
        bail!("Not enough bytes for string data");
    }
    let mut bytes = vec![0u8; len];
    buf.copy_to_slice(&mut bytes);
    String::from_utf8(bytes).map_err(|e| anyhow::anyhow!("Invalid UTF-8: {}", e))
}

impl MessageEncoder for ServerMessage {
    fn encode(&self) -> Result<Bytes> {
        let mut buf = BytesMut::new();

        match self {
            ServerMessage::Login { username, password, version, md5_hash, minor_version } => {
                buf.put_u32_le(ServerCode::Login as u32);
                encode_string(&mut buf, username);
                encode_string(&mut buf, password);
                buf.put_u32_le(*version);
                encode_string(&mut buf, md5_hash);
                buf.put_u32_le(*minor_version);
            }
            ServerMessage::SetWaitPort { port } => {
                buf.put_u32_le(ServerCode::SetWaitPort as u32);
                buf.put_u32_le(*port);
            }
            ServerMessage::GetPeerAddress { username } => {
                buf.put_u32_le(ServerCode::GetPeerAddress as u32);
                encode_string(&mut buf, username);
            }
            ServerMessage::ConnectToPeer { token, username, connection_type } => {
                buf.put_u32_le(ServerCode::ConnectToPeer as u32);
                buf.put_u32_le(*token);
                encode_string(&mut buf, username);
                encode_string(&mut buf, connection_type);
            }
            ServerMessage::FileSearch { token, query } => {
                buf.put_u32_le(ServerCode::FileSearch as u32);
                buf.put_u32_le(*token);
                encode_string(&mut buf, query);
            }
            ServerMessage::UserSearch { username, token, query } => {
                buf.put_u32_le(ServerCode::UserSearch as u32);
                encode_string(&mut buf, username);
                buf.put_u32_le(*token);
                encode_string(&mut buf, query);
            }
            ServerMessage::SetStatus { status } => {
                buf.put_u32_le(ServerCode::SetStatus as u32);
                buf.put_u32_le(*status);
            }
            ServerMessage::SharedFoldersFiles { folders, files } => {
                buf.put_u32_le(ServerCode::SharedFoldersFiles as u32);
                buf.put_u32_le(*folders);
                buf.put_u32_le(*files);
            }
            ServerMessage::GetUserStats { username } => {
                buf.put_u32_le(ServerCode::GetUserStats as u32);
                encode_string(&mut buf, username);
            }
            ServerMessage::ServerPing => {
                buf.put_u32_le(ServerCode::ServerPing as u32);
            }
            ServerMessage::MessageUser { username, message } => {
                buf.put_u32_le(ServerCode::MessageUser as u32);
                encode_string(&mut buf, username);
                encode_string(&mut buf, message);
            }
            ServerMessage::RoomList => {
                buf.put_u32_le(ServerCode::RoomList as u32);
            }
            _ => bail!("Message encoding not implemented for {:?}", self),
        }

        // Prepend message length
        let mut final_buf = BytesMut::new();
        final_buf.put_u32_le(buf.len() as u32);
        final_buf.extend_from_slice(&buf);

        Ok(final_buf.freeze())
    }
}

impl MessageEncoder for PeerMessage {
    fn encode(&self) -> Result<Bytes> {
        let mut buf = BytesMut::new();

        match self {
            PeerMessage::FileSearchRequest { token, query } => {
                buf.put_u32_le(PeerCode::FileSearchRequest as u32);
                buf.put_u32_le(*token);
                encode_string(&mut buf, query);
            }
            PeerMessage::TransferRequest { direction, token, filename } => {
                buf.put_u32_le(PeerCode::TransferRequest as u32);
                buf.put_u32_le(*direction);
                buf.put_u32_le(*token);
                encode_string(&mut buf, filename);
            }
            PeerMessage::QueueUpload { filename } => {
                buf.put_u32_le(PeerCode::QueueUpload as u32);
                encode_string(&mut buf, filename);
            }
            _ => bail!("Message encoding not implemented for {:?}", self),
        }

        // Prepend message length
        let mut final_buf = BytesMut::new();
        final_buf.put_u32_le(buf.len() as u32);
        final_buf.extend_from_slice(&buf);

        Ok(final_buf.freeze())
    }
}

impl MessageDecoder for ServerMessage {
    fn decode(buf: &mut BytesMut) -> Result<Option<Self>> {
        if buf.remaining() < 4 {
            return Ok(None);
        }

        // Peek at message length
        let msg_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

        if buf.remaining() < 4 + msg_len {
            return Ok(None); // Not enough data yet
        }

        // Skip length prefix
        buf.advance(4);

        if buf.remaining() < 4 {
            bail!("Message too short");
        }

        let code = buf.get_u32_le();

        let message = match code {
            1 => { // Login response
                let success = buf.get_u8() != 0;
                let reason = decode_string(buf)?;

                if success {
                    // For successful login, read additional fields but ignore them
                    let greeting = if buf.remaining() >= 4 {
                        Some(decode_string(buf).unwrap_or_default())
                    } else {
                        None
                    };

                    let ip = if buf.remaining() >= 4 {
                        Some(decode_string(buf).unwrap_or_default())
                    } else {
                        None
                    };

                    // Skip any remaining fields (password hash MD5, supporter status, etc.)
                    // by consuming the rest of the buffer
                    buf.clear();

                    ServerMessage::LoginResponse { success, reason, greeting, ip }
                } else {
                    ServerMessage::LoginResponse { success, reason, greeting: None, ip: None }
                }
            }
            3 => { // Peer address
                let username = decode_string(buf)?;
                let ip = decode_string(buf)?;
                let port = buf.get_u16_le();
                ServerMessage::PeerAddress { username, ip, port }
            }
            7 => { // User stats
                let username = decode_string(buf)?;
                let avg_speed = buf.get_u32_le();
                let uploads = buf.get_u64_le();
                let shared_files = buf.get_u32_le();
                let shared_folders = buf.get_u32_le();
                ServerMessage::UserStats { username, avg_speed, uploads, shared_files, shared_folders }
            }
            32 => ServerMessage::ServerPong,
            41 => ServerMessage::Relogged,
            22 => { // Message user received
                let message_id = buf.get_u32_le();
                let timestamp = buf.get_u32_le();
                let username = decode_string(buf)?;
                let message = decode_string(buf)?;
                ServerMessage::MessageUserReceived { message_id, timestamp, username, message }
            }
            18 => { // ConnectToPeer - server tells us to connect to a peer
                let username = decode_string(buf)?;
                let connection_type = decode_string(buf)?;

                // IP address as 4 bytes (little-endian - need to reverse)
                let ip_bytes = [buf.get_u8(), buf.get_u8(), buf.get_u8(), buf.get_u8()];
                let ip = format!("{}.{}.{}.{}", ip_bytes[3], ip_bytes[2], ip_bytes[1], ip_bytes[0]);

                let port = buf.get_u32_le() as u16;
                let token = buf.get_u32_le();
                let privileged = if buf.remaining() >= 1 { buf.get_u8() != 0 } else { false };

                tracing::info!(
                    "[Protocol] ConnectToPeer: user='{}', type='{}', addr={}:{}, token={}",
                    username, connection_type, ip, port, token
                );

                ServerMessage::ConnectToPeerRequest {
                    username,
                    connection_type,
                    ip,
                    port,
                    token,
                    privileged,
                }
            }
            93 => { // Embedded message (distributed search results)
                let message_type = buf.get_u8();
                // Read remaining data
                let remaining = buf.remaining();
                let mut data = vec![0u8; remaining];
                buf.copy_to_slice(&mut data);
                tracing::debug!("[Protocol] Received EmbeddedMessage type={}, {} bytes", message_type, data.len());
                ServerMessage::EmbeddedMessage { message_type, data }
            }
            _ => {
                // Log unknown message instead of failing
                let remaining = buf.remaining();
                let mut data = vec![0u8; remaining];
                if remaining > 0 {
                    buf.copy_to_slice(&mut data);
                }
                tracing::warn!("[Protocol] Unknown server message code: {} ({} bytes of data)", code, data.len());
                ServerMessage::Unknown { code, data }
            }
        };

        Ok(Some(message))
    }
}

impl MessageDecoder for PeerMessage {
    fn decode(buf: &mut BytesMut) -> Result<Option<Self>> {
        if buf.remaining() < 4 {
            return Ok(None);
        }

        // Peek at message length
        let msg_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;

        if buf.remaining() < 4 + msg_len {
            return Ok(None);
        }

        buf.advance(4);

        if buf.remaining() < 4 {
            bail!("Message too short");
        }

        let code = buf.get_u32_le();

        let message = match code {
            9 => { // File search result
                let username = decode_string(buf)?;
                let token = buf.get_u32_le();
                let file_count = buf.get_u32_le();

                let mut files = Vec::new();
                for _ in 0..file_count {
                    let code = buf.get_u8();
                    let filename = decode_string(buf)?;
                    let size = buf.get_u64_le();
                    let extension = decode_string(buf)?;
                    let attr_count = buf.get_u32_le();

                    let mut attributes = Vec::new();
                    for _ in 0..attr_count {
                        let attribute_type = buf.get_u32_le();
                        let value = buf.get_u32_le();
                        attributes.push(FileAttribute { attribute_type, value });
                    }

                    files.push(SearchFile { code, filename, size, extension, attributes });
                }

                let has_slots = buf.get_u8() != 0;
                let avg_speed = buf.get_u32_le();
                let queue_length = buf.get_u64_le();

                PeerMessage::FileSearchResult {
                    username,
                    token,
                    files,
                    has_slots,
                    avg_speed,
                    queue_length,
                }
            }
            41 => { // Transfer reply
                let token = buf.get_u32_le();
                let allowed = buf.get_u8() != 0;
                let filesize = if allowed { buf.get_u64_le() } else { 0 };
                PeerMessage::TransferReply { token, allowed, filesize }
            }
            44 => { // Place in queue reply
                let filename = decode_string(buf)?;
                let place = buf.get_u32_le();
                PeerMessage::PlaceInQueueReply { filename, place }
            }
            46 => { // Upload failed
                let filename = decode_string(buf)?;
                PeerMessage::UploadFailed { filename }
            }
            _ => bail!("Unknown peer message code: {}", code),
        };

        Ok(Some(message))
    }
}
