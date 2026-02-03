use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::codec::{Framed, FramedRead, FramedWrite};
use anyhow::{Result, bail, Context};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::sync::Mutex;

use super::codec::{ServerCodec, PeerCodec};
use super::messages::{ServerMessage, PeerMessage, MessageEncoder};

const SERVER_HOST: &str = "server.slsknet.org";
const SERVER_PORT: u16 = 2242;

/// Connection to the Soulseek server
/// Split into separate read/write halves to avoid deadlock
pub struct ServerConnection {
    writer: Arc<Mutex<FramedWrite<OwnedWriteHalf, ServerCodec>>>,
    reader: Arc<Mutex<FramedRead<OwnedReadHalf, ServerCodec>>>,
}

impl ServerConnection {
    /// Connect to the Soulseek server
    pub async fn connect() -> Result<Self> {
        let addr = format!("{}:{}", SERVER_HOST, SERVER_PORT);
        tracing::info!("Connecting to Soulseek server at {}", addr);

        let stream = TcpStream::connect(&addr)
            .await
            .context("Failed to connect to Soulseek server")?;

        // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
        stream.set_nodelay(true)?;

        // Split into separate read/write halves to avoid deadlock
        let (read_half, write_half) = stream.into_split();
        let reader = FramedRead::new(read_half, ServerCodec);
        let writer = FramedWrite::new(write_half, ServerCodec);

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            reader: Arc::new(Mutex::new(reader)),
        })
    }

    /// Send a message to the server
    pub async fn send(&self, message: ServerMessage) -> Result<()> {
        tracing::debug!("[ServerConnection] Sending: {:?}", message);
        let mut writer = self.writer.lock().await;
        writer.send(message).await
            .context("Failed to send message to server")?;
        Ok(())
    }

    /// Receive a message from the server
    pub async fn receive(&self) -> Result<Option<ServerMessage>> {
        let mut reader = self.reader.lock().await;
        match reader.next().await {
            Some(Ok(msg)) => {
                tracing::trace!("[ServerConnection] Received: {:?}", msg);
                Ok(Some(msg))
            }
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    /// Login to the server
    pub async fn login(&self, username: &str, password: &str) -> Result<()> {
        // MD5 hash is concatenation of username + password
        let combined = format!("{}{}", username, password);
        let md5_hash = format!("{:x}", md5::compute(combined.as_bytes()));

        let login_msg = ServerMessage::Login {
            username: username.to_string(),
            password: password.to_string(),
            version: 160, // Nicotine+ protocol version
            md5_hash,
            minor_version: 1, // Standard minor version
        };

        self.send(login_msg).await?;

        // Wait for login response
        match self.receive().await? {
            Some(ServerMessage::LoginResponse { success, reason, .. }) => {
                if success {
                    tracing::info!("Successfully logged in as {}", username);
                    Ok(())
                } else {
                    bail!("Login failed: {}", reason);
                }
            }
            Some(other) => bail!("Unexpected response to login: {:?}", other),
            None => bail!("Connection closed during login"),
        }
    }

    /// Set listening port for peer connections
    pub async fn set_wait_port(&self, port: u32, obfuscated_port: Option<u32>) -> Result<()> {
        if let Some(obfs_port) = obfuscated_port {
            tracing::info!("[ServerConnection] Setting wait port to {} (obfuscated: {})", port, obfs_port);
            self.send(ServerMessage::SetWaitPort {
                port,
                obfuscation_type: Some(1), // 1 = Rotated obfuscation
                obfuscated_port: Some(obfs_port),
            }).await
        } else {
            tracing::info!("[ServerConnection] Setting wait port to {}", port);
            self.send(ServerMessage::SetWaitPort {
                port,
                obfuscation_type: None,
                obfuscated_port: None,
            }).await
        }
    }

    /// Get peer address
    pub async fn get_peer_address(&self, username: &str) -> Result<(String, u16)> {
        self.send(ServerMessage::GetPeerAddress {
            username: username.to_string(),
        }).await?;

        match self.receive().await? {
            Some(ServerMessage::PeerAddress { ip, port, .. }) => Ok((ip, port)),
            Some(other) => bail!("Unexpected response: {:?}", other),
            None => bail!("Connection closed"),
        }
    }

    /// Send shared folder/file count
    pub async fn set_shared_counts(&self, folders: u32, files: u32) -> Result<()> {
        self.send(ServerMessage::SharedFoldersFiles { folders, files }).await
    }

    /// Perform a file search
    pub async fn file_search(&self, token: u32, query: &str) -> Result<()> {
        tracing::info!("[ServerConnection] Sending file search: token={}, query='{}'", token, query);
        self.send(ServerMessage::FileSearch {
            token,
            query: query.to_string(),
        }).await?;
        tracing::info!("[ServerConnection] File search sent successfully");
        Ok(())
    }
}

/// Connection to a peer
pub struct PeerConnection {
    stream: Framed<TcpStream, PeerCodec>,
    username: String,
}

impl PeerConnection {
    /// Connect to a peer
    pub async fn connect(ip: &str, port: u16, username: String) -> Result<Self> {
        let addr = format!("{}:{}", ip, port);
        tracing::info!("Connecting to peer {} at {}", username, addr);

        let stream = TcpStream::connect(&addr)
            .await
            .context("Failed to connect to peer")?;

        // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
        stream.set_nodelay(true)?;

        let framed = Framed::new(stream, PeerCodec);

        Ok(Self { stream: framed, username })
    }

    /// Send a message to the peer
    pub async fn send(&mut self, message: PeerMessage) -> Result<()> {
        self.stream.send(message).await
            .context("Failed to send message to peer")?;
        Ok(())
    }

    /// Receive a message from the peer
    pub async fn receive(&mut self) -> Result<Option<PeerMessage>> {
        match self.stream.next().await {
            Some(Ok(msg)) => Ok(Some(msg)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    /// Request file transfer
    pub async fn request_transfer(&mut self, token: u32, filename: &str) -> Result<u64> {
        self.send(PeerMessage::TransferRequest {
            direction: 0, // download
            token,
            filename: filename.to_string(),
        }).await?;

        match self.receive().await? {
            Some(PeerMessage::TransferReply { allowed: true, filesize, .. }) => Ok(filesize),
            Some(PeerMessage::TransferReply { allowed: false, .. }) => {
                bail!("Transfer denied by peer");
            }
            Some(other) => bail!("Unexpected response: {:?}", other),
            None => bail!("Connection closed"),
        }
    }

    /// Queue upload request
    pub async fn queue_upload(&mut self, filename: &str) -> Result<u32> {
        self.send(PeerMessage::QueueUpload {
            filename: filename.to_string(),
        }).await?;

        match self.receive().await? {
            Some(PeerMessage::PlaceInQueueReply { place, .. }) => Ok(place),
            Some(other) => bail!("Unexpected response: {:?}", other),
            None => bail!("Connection closed"),
        }
    }
}

/// File transfer connection
pub struct FileTransferConnection {
    stream: TcpStream,
}

impl FileTransferConnection {
    /// Connect for file transfer
    pub async fn connect(ip: &str, port: u16) -> Result<Self> {
        let addr = format!("{}:{}", ip, port);
        tracing::info!("Opening file transfer connection to {}", addr);

        let stream = TcpStream::connect(&addr)
            .await
            .context("Failed to connect for file transfer")?;

        // Disable Nagle's algorithm for immediate packet sending (matches Nicotine+)
        stream.set_nodelay(true)?;

        Ok(Self { stream })
    }

    /// Initialize file transfer with token
    pub async fn init(&mut self, token: u32) -> Result<()> {
        // Send piercing token (4 bytes, little-endian)
        self.stream.write_u32_le(token).await?;
        Ok(())
    }

    /// Download file
    pub async fn download_file(&mut self, output_path: &std::path::Path) -> Result<u64> {
        use tokio::fs::File;
        use tokio::io::AsyncWriteExt;

        // Send offset (0 for new download)
        self.stream.write_u64_le(0).await?;

        let mut file = File::create(output_path).await?;
        let mut total_bytes = 0u64;

        let mut buffer = vec![0u8; 8192];
        loop {
            let n = self.stream.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n]).await?;
            total_bytes += n as u64;
        }

        file.flush().await?;
        tracing::info!("Downloaded {} bytes to {:?}", total_bytes, output_path);

        Ok(total_bytes)
    }
}
