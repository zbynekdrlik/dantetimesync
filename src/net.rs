use anyhow::{anyhow, Result};
use socket2::{Domain, Protocol, Socket, Type};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};

#[cfg(unix)]
use nix::sys::socket::{setsockopt, sockopt};

pub fn get_default_interface() -> Result<(String, Ipv4Addr)> {
    let ifaces = if_addrs::get_if_addrs()?;

    let mut best_iface = None;

    for iface in &ifaces {
        // Skip loopback and non-IPv4
        let ip = match iface.addr.ip() {
            IpAddr::V4(ip) if !ip.is_loopback() => ip,
            _ => continue,
        };

        // Skip wireless interfaces if possible
        let name_lower = iface.name.to_lowercase();
        let is_wireless = name_lower.contains("wireless")
            || name_lower.contains("wi-fi")
            || name_lower.contains("wlan");

        // Verify we can actually bind to this IP
        if is_ip_bindable(ip) {
            if !is_wireless {
                return Ok((iface.name.clone(), ip));
            } else if best_iface.is_none() {
                best_iface = Some((iface.name.clone(), ip));
            }
        }
    }

    if let Some(res) = best_iface {
        return Ok(res);
    }

    // Diagnostics
    log::warn!("No suitable IPv4 interface found. Diagnostics:");
    for iface in &ifaces {
        log::warn!(" - Name: {}, Addr: {:?}", iface.name, iface.addr);
    }

    Err(anyhow!("No suitable IPv4 interface found"))
}

fn is_ip_bindable(ip: Ipv4Addr) -> bool {
    let socket = match Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let addr = SocketAddrV4::new(ip, 0); // Port 0 (ephemeral)
    socket.bind(&addr.into()).is_ok()
}

pub fn create_multicast_socket(port: u16, interface_ip: Ipv4Addr) -> Result<UdpSocket> {
    // Standard UDP socket creation for TX (Transmission) or legacy RX
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;

    socket.set_reuse_address(true)?;

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&addr.into())?;

    let multi_addr: Ipv4Addr = "224.0.1.129".parse()?;
    socket.join_multicast_v4(&multi_addr, &interface_ip)?;

    socket.set_multicast_loop_v4(false)?;
    socket.set_nonblocking(true)?;

    let udp_socket: UdpSocket = socket.into();

    #[cfg(unix)]
    {
        match setsockopt(&udp_socket, sockopt::ReceiveTimestampns, &true) {
            Ok(_) => log::info!("Kernel timestamping (SO_TIMESTAMPNS) enabled."),
            Err(e) => log::warn!("Failed to enable kernel timestamping: {}", e),
        }
    }

    Ok(udp_socket)
}

#[cfg(unix)]
pub fn recv_with_timestamp(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> Result<Option<(usize, std::time::SystemTime)>> {
    use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags, SockaddrStorage};
    use nix::sys::time::TimeSpec;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, SystemTime};

    let fd = sock.as_raw_fd();
    let mut iov = [std::io::IoSliceMut::new(buf)];
    let mut cmsg_buf = nix::cmsg_space!(TimeSpec);

    match recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty()) {
        Ok(msg) => {
            let timestamp = msg
                .cmsgs()
                .find_map(|cmsg| {
                    if let ControlMessageOwned::ScmTimestampns(ts) = cmsg {
                        let duration = Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32);
                        Some(SystemTime::UNIX_EPOCH + duration)
                    } else {
                        None
                    }
                })
                .unwrap_or_else(SystemTime::now);

            Ok(Some((msg.bytes, timestamp)))
        }
        Err(nix::errno::Errno::EAGAIN) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(unix))]
pub fn recv_with_timestamp(
    sock: &UdpSocket,
    buf: &mut [u8],
) -> Result<Option<(usize, std::time::SystemTime)>> {
    match sock.recv_from(buf) {
        Ok((size, _)) => Ok(Some((size, std::time::SystemTime::now()))),
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that get_default_interface filters out loopback addresses
    #[test]
    fn test_get_default_interface_returns_non_loopback() {
        // This test verifies the interface selection logic runs without panic
        // On systems with valid network interfaces, it should succeed
        // On systems without interfaces, it returns an error (which is valid)
        let result = get_default_interface();
        if let Ok((name, ip)) = result {
            assert!(!name.is_empty(), "Interface name should not be empty");
            assert!(!ip.is_loopback(), "Should not return loopback address");
        }
        // Error case is acceptable on minimal test environments
    }

    /// Test is_ip_bindable with loopback (should always work)
    #[test]
    fn test_is_ip_bindable_loopback() {
        // Loopback should always be bindable on any system
        let loopback = Ipv4Addr::new(127, 0, 0, 1);
        assert!(is_ip_bindable(loopback), "Loopback should be bindable");
    }

    /// Test is_ip_bindable with UNSPECIFIED address
    #[test]
    fn test_is_ip_bindable_unspecified() {
        // 0.0.0.0 should be bindable (binds to all interfaces)
        let unspecified = Ipv4Addr::UNSPECIFIED;
        assert!(
            is_ip_bindable(unspecified),
            "UNSPECIFIED (0.0.0.0) should be bindable"
        );
    }

    /// Test PTP multicast address constant
    #[test]
    fn test_ptp_multicast_address() {
        let multi_addr: Ipv4Addr = "224.0.1.129".parse().unwrap();
        assert!(multi_addr.is_multicast(), "PTP address should be multicast");
        assert_eq!(multi_addr.octets(), [224, 0, 1, 129]);
    }

    /// Test recv_with_timestamp returns None for non-blocking socket with no data
    #[test]
    fn test_recv_with_timestamp_no_data() {
        // Create a simple non-blocking UDP socket
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        socket.set_nonblocking(true).unwrap();
        socket
            .bind(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0).into())
            .unwrap();

        let udp_socket: UdpSocket = socket.into();
        let mut buf = [0u8; 512];

        let result = recv_with_timestamp(&udp_socket, &mut buf);
        assert!(result.is_ok());
        // Should return None since no data is available
        assert!(result.unwrap().is_none());
    }

    /// Test wireless interface detection keywords
    #[test]
    fn test_wireless_interface_detection() {
        // Test the wireless detection logic used in get_default_interface
        let wireless_names = ["Wireless LAN", "Wi-Fi", "wlan0", "WIRELESS"];
        let wired_names = ["eth0", "Ethernet", "enp3s0", "Local Area Connection"];

        for name in &wireless_names {
            let lower = name.to_lowercase();
            let is_wireless =
                lower.contains("wireless") || lower.contains("wi-fi") || lower.contains("wlan");
            assert!(is_wireless, "{} should be detected as wireless", name);
        }

        for name in &wired_names {
            let lower = name.to_lowercase();
            let is_wireless =
                lower.contains("wireless") || lower.contains("wi-fi") || lower.contains("wlan");
            assert!(!is_wireless, "{} should NOT be detected as wireless", name);
        }
    }
}
