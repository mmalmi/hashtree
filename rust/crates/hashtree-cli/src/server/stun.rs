//! STUN server for WebRTC NAT traversal
//!
//! Implements a simple STUN server that responds to binding requests
//! with the client's reflexive transport address (XOR-MAPPED-ADDRESS).

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::UdpSocket;
use tracing::{debug, error, info};
use webrtc_stun::message::{
    Message, MessageType, Setter, BINDING_REQUEST,
    CLASS_SUCCESS_RESPONSE, METHOD_BINDING,
};
use webrtc_stun::xoraddr::XORMappedAddress;

/// Default STUN port (RFC 5389)
pub const DEFAULT_STUN_PORT: u16 = 3478;

/// STUN server handle for graceful shutdown
pub struct StunServerHandle {
    pub addr: SocketAddr,
    shutdown: Arc<tokio::sync::Notify>,
}

impl StunServerHandle {
    /// Signal the server to shutdown
    pub fn shutdown(&self) {
        self.shutdown.notify_one();
    }
}

/// Start a STUN server on the specified address
pub async fn start_stun_server(addr: SocketAddr) -> anyhow::Result<StunServerHandle> {
    let socket = UdpSocket::bind(addr).await?;
    let bound_addr = socket.local_addr()?;
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let shutdown_clone = shutdown.clone();

    info!("STUN server listening on {}", bound_addr);

    tokio::spawn(async move {
        run_stun_server(socket, shutdown_clone).await;
    });

    Ok(StunServerHandle {
        addr: bound_addr,
        shutdown,
    })
}

async fn run_stun_server(socket: UdpSocket, shutdown: Arc<tokio::sync::Notify>) {
    let mut buf = vec![0u8; 1500]; // Standard MTU size

    loop {
        tokio::select! {
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((len, src_addr)) => {
                        if let Err(e) = handle_stun_packet(&socket, &buf[..len], src_addr).await {
                            debug!("Error handling STUN packet from {}: {}", src_addr, e);
                        }
                    }
                    Err(e) => {
                        error!("Error receiving UDP packet: {}", e);
                    }
                }
            }
            _ = shutdown.notified() => {
                info!("STUN server shutting down");
                break;
            }
        }
    }
}

async fn handle_stun_packet(
    socket: &UdpSocket,
    data: &[u8],
    src_addr: SocketAddr,
) -> anyhow::Result<()> {
    // Parse the incoming message
    let mut msg = Message::new();
    msg.raw = data.to_vec();

    if let Err(e) = msg.decode() {
        debug!("Failed to decode STUN message from {}: {}", src_addr, e);
        return Ok(()); // Silently ignore non-STUN packets
    }

    // Check if it's a binding request
    if msg.typ != BINDING_REQUEST {
        debug!("Received non-binding STUN message type {:?} from {}", msg.typ, src_addr);
        return Ok(());
    }

    debug!("STUN binding request from {}", src_addr);

    // Build binding success response
    let mut response = Message::new();
    response.typ = MessageType {
        method: METHOD_BINDING,
        class: CLASS_SUCCESS_RESPONSE,
    };
    response.transaction_id = msg.transaction_id;

    // Add XOR-MAPPED-ADDRESS with the client's reflexive address
    let xor_addr = XORMappedAddress {
        ip: src_addr.ip(),
        port: src_addr.port(),
    };
    xor_addr.add_to(&mut response)?;

    // Encode and send the response
    response.encode();
    socket.send_to(&response.raw, src_addr).await?;

    debug!("STUN binding response sent to {} (mapped: {})", src_addr, src_addr);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use webrtc_stun::message::Getter;

    #[tokio::test]
    async fn test_stun_server_responds_to_binding_request() {
        // Start STUN server on random port
        let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let handle = start_stun_server(server_addr).await.unwrap();
        let server_addr = handle.addr;

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Create client socket
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client_addr = client.local_addr().unwrap();

        // Build binding request
        let mut request = Message::new();
        request.typ = BINDING_REQUEST;
        request.new_transaction_id().expect("Failed to generate transaction ID");
        request.encode();

        // Send request
        client.send_to(&request.raw, server_addr).await.unwrap();

        // Receive response
        let mut buf = vec![0u8; 1500];
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            client.recv_from(&mut buf)
        ).await;

        let (len, _) = result.expect("Timeout waiting for response").unwrap();

        // Parse response
        let mut response = Message::new();
        response.raw = buf[..len].to_vec();
        response.decode().expect("Failed to decode response");

        // Verify response type
        assert_eq!(response.typ.method, METHOD_BINDING);
        assert_eq!(response.typ.class, CLASS_SUCCESS_RESPONSE);
        assert_eq!(response.transaction_id, request.transaction_id);

        // Extract XOR-MAPPED-ADDRESS
        let mut xor_addr = XORMappedAddress::default();
        xor_addr.get_from(&response).expect("Failed to get XOR-MAPPED-ADDRESS");

        // The mapped address should match our client's address
        assert_eq!(xor_addr.ip, client_addr.ip());
        assert_eq!(xor_addr.port, client_addr.port());

        // Cleanup
        handle.shutdown();
    }
}
