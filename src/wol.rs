//! Wake-on-LAN magic packet: 6×0xFF + MAC×16, UDP broadcast to `<broadcast>`:9 and :7.

use std::net::{Ipv4Addr, SocketAddrV4};

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tracing::debug;

/// Length of a magic packet: 6 + 16×6.
pub const PACKET_LEN: usize = 102;

/// UDP ports the magic packet is sent to (discard and echo).
const PORTS: [u16; 2] = [9, 7];

/// Build the magic packet for `mac`.
pub fn magic_packet(mac: &[u8; 6]) -> [u8; PACKET_LEN] {
    let mut packet = [0xFF; PACKET_LEN];
    for chunk in packet[6..].as_chunks_mut::<6>().0 {
        chunk.copy_from_slice(mac);
    }
    packet
}

/// Send the magic packet to `broadcast` on ports 9 and 7.
pub async fn send(mac: &[u8; 6], broadcast: Ipv4Addr) -> Result<()> {
    let packet = magic_packet(mac);
    let socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .context("binding UDP socket for Wake-on-LAN")?;
    socket
        .set_broadcast(true)
        .context("enabling SO_BROADCAST on Wake-on-LAN socket")?;
    for port in PORTS {
        let target = SocketAddrV4::new(broadcast, port);
        socket
            .send_to(&packet, target)
            .await
            .with_context(|| format!("sending Wake-on-LAN packet to {target}"))?;
        debug!(%target, mac = %format_mac(mac), "sent Wake-on-LAN magic packet");
    }
    Ok(())
}

fn format_mac(mac: &[u8; 6]) -> String {
    mac.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TV_MAC: [u8; 6] = [0xd0, 0xcd, 0xbf, 0x66, 0x69, 0xc2];

    #[test]
    fn packet_length() {
        assert_eq!(magic_packet(&TV_MAC).len(), 102);
        assert_eq!(PACKET_LEN, 6 + 16 * 6);
    }

    #[test]
    fn packet_layout() {
        let packet = magic_packet(&TV_MAC);
        let mut expected = vec![0xFF; 6];
        for _ in 0..16 {
            expected.extend_from_slice(&TV_MAC);
        }
        assert_eq!(packet.as_slice(), expected.as_slice());
        assert_eq!(&packet[..6], &[0xFF; 6]);
        for rep in packet[6..].as_chunks::<6>().0 {
            assert_eq!(rep, &TV_MAC);
        }
    }

    #[test]
    fn mac_formatting() {
        assert_eq!(format_mac(&TV_MAC), "d0:cd:bf:66:69:c2");
    }
}
