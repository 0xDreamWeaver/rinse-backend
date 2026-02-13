//! UPnP port forwarding service
//!
//! This module provides automatic port forwarding using UPnP (Universal Plug and Play).
//! It allows the application to automatically open ports on the router for incoming
//! peer connections.

use anyhow::{Result, Context};
use igd_next::PortMappingProtocol;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

/// Default listen port for peer connections
pub const PEER_LISTEN_PORT: u16 = 2234;
/// Obfuscated port (main port + 1)
pub const PEER_OBFUSCATED_PORT: u16 = 2235;

/// Get the local IP address for this machine by connecting to a known external address
async fn get_local_ip() -> Result<Ipv4Addr> {
    // Create a UDP socket and connect to a public DNS server to determine local IP
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect("8.8.8.8:80").await?;
    let local_addr = socket.local_addr()?;

    match local_addr.ip() {
        std::net::IpAddr::V4(ip) => Ok(ip),
        _ => Err(anyhow::anyhow!("Expected IPv4 address")),
    }
}

/// Initialize UPnP and set up port mappings for Soulseek
/// Returns Ok(true) if UPnP was successful, Ok(false) if not available
pub async fn init_upnp() -> Result<bool> {
    use igd_next::aio::tokio::search_gateway;

    tracing::info!("[UPnP] Searching for UPnP gateway...");

    // Discover gateway
    let gateway = match search_gateway(Default::default()).await {
        Ok(gw) => {
            tracing::info!("[UPnP] Found gateway at {}", gw.addr);
            gw
        }
        Err(e) => {
            tracing::warn!("[UPnP] UPnP not available: {}. Manual port forwarding may be required.", e);
            return Ok(false);
        }
    };

    // Get local IP
    let local_ip = match get_local_ip().await {
        Ok(ip) => {
            tracing::info!("[UPnP] Local IP: {}", ip);
            ip
        }
        Err(e) => {
            tracing::warn!("[UPnP] Failed to determine local IP: {}", e);
            return Ok(false);
        }
    };

    // Try to get external IP
    if let Ok(external_ip) = gateway.get_external_ip().await {
        tracing::info!("[UPnP] External IP: {}", external_ip);
    }

    // Try to remove any existing mappings first (from crashed sessions)
    tracing::info!("[UPnP] Removing any stale port mappings...");
    match gateway.remove_port(PortMappingProtocol::TCP, PEER_LISTEN_PORT).await {
        Ok(()) => tracing::info!("[UPnP] Removed existing mapping for port {}", PEER_LISTEN_PORT),
        Err(_) => tracing::debug!("[UPnP] No existing mapping for port {} (or removal failed)", PEER_LISTEN_PORT),
    }
    match gateway.remove_port(PortMappingProtocol::TCP, PEER_OBFUSCATED_PORT).await {
        Ok(()) => tracing::info!("[UPnP] Removed existing mapping for port {}", PEER_OBFUSCATED_PORT),
        Err(_) => tracing::debug!("[UPnP] No existing mapping for port {} (or removal failed)", PEER_OBFUSCATED_PORT),
    }

    // Small delay to let router process the removal
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Map main listen port (2234)
    let local_addr_main = SocketAddr::V4(SocketAddrV4::new(local_ip, PEER_LISTEN_PORT));
    match gateway.add_port(
        PortMappingProtocol::TCP,
        PEER_LISTEN_PORT,
        local_addr_main,
        3600, // 1 hour lease
        "Rinse - Soulseek Peer",
    ).await {
        Ok(()) => {
            tracing::info!("[UPnP] Successfully mapped port {}", PEER_LISTEN_PORT);
        }
        Err(e) => {
            tracing::warn!("[UPnP] Failed to map port {}: {}", PEER_LISTEN_PORT, e);
            // Continue anyway, may already be mapped to us
        }
    }

    // Map obfuscated port (2235)
    let local_addr_obfs = SocketAddr::V4(SocketAddrV4::new(local_ip, PEER_OBFUSCATED_PORT));
    match gateway.add_port(
        PortMappingProtocol::TCP,
        PEER_OBFUSCATED_PORT,
        local_addr_obfs,
        3600,
        "Rinse - Soulseek Obfuscated",
    ).await {
        Ok(()) => {
            tracing::info!("[UPnP] Successfully mapped port {}", PEER_OBFUSCATED_PORT);
        }
        Err(e) => {
            tracing::warn!("[UPnP] Failed to map port {}: {}", PEER_OBFUSCATED_PORT, e);
        }
    }

    tracing::info!("[UPnP] Port forwarding setup complete");
    tracing::info!("[UPnP] Ports: {} (peer), {} (obfuscated)", PEER_LISTEN_PORT, PEER_OBFUSCATED_PORT);

    Ok(true)
}

/// Remove UPnP port mappings (call on shutdown)
pub async fn cleanup_upnp() -> Result<()> {
    use igd_next::aio::tokio::search_gateway;

    tracing::info!("[UPnP] Cleaning up port mappings...");

    let gateway = match search_gateway(Default::default()).await {
        Ok(gw) => gw,
        Err(_) => return Ok(()), // No gateway, nothing to clean up
    };

    // Try to remove mappings, ignore errors (may not exist)
    let _ = gateway.remove_port(PortMappingProtocol::TCP, PEER_LISTEN_PORT).await;
    let _ = gateway.remove_port(PortMappingProtocol::TCP, PEER_OBFUSCATED_PORT).await;

    tracing::info!("[UPnP] Port mappings removed");
    Ok(())
}
