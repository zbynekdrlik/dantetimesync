use anyhow::{Result, anyhow};
use socket2::{Socket, Domain, Type, Protocol};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};
use pcap::{Device, Capture, Active};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use nix::sys::socket::{setsockopt, sockopt};

pub fn get_default_interface() -> Result<(String, Ipv4Addr)> {
    let devices = Device::list()?;
    
    let valid_devices: Vec<_> = devices.iter()
        .filter(|d| !d.addresses.is_empty())
        .collect();

    if valid_devices.is_empty() {
        log::warn!("No network interfaces found via Pcap.");
        return Err(anyhow!("No suitable network interface found"));
    }

    let mut best_iface = None;
    
    for dev in valid_devices {
        // Find IPv4
        let ipv4 = dev.addresses.iter().find(|a| {
            // pcap::Address.addr is std::net::IpAddr
            match a.addr {
                IpAddr::V4(ip) => !ip.is_loopback(),
                _ => false,
            }
        });

        if let Some(ipv4_addr) = ipv4 {
            let ip = if let IpAddr::V4(addr) = ipv4_addr.addr {
                addr
            } else {
                continue;
            };

            // Prefer non-wireless/non-loopback
            // pcap::Device has `desc` field (Option<String>)
            let desc_str = dev.desc.as_deref().unwrap_or("").to_lowercase();
            let is_wireless = desc_str.contains("wireless") || desc_str.contains("wi-fi") || desc_str.contains("wlan");
            
            // Verify we can actually bind to this IP (WinSock check)
            if is_ip_bindable(ip) {
                if !is_wireless {
                    return Ok((dev.name.clone(), ip));
                } else if best_iface.is_none() {
                    best_iface = Some((dev.name.clone(), ip));
                }
            }
        }
    }

    if let Some(res) = best_iface {
        return Ok(res);
    }

    // Diagnostics
    log::warn!("No suitable IPv4 interface found. Diagnostics:");
    for dev in devices {
        log::warn!(" - Name: {}, Desc: {:?}, Addrs: {:?}", dev.name, dev.desc, dev.addresses);
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
pub fn recv_with_timestamp(sock: &UdpSocket, buf: &mut [u8]) -> Result<Option<(usize, std::time::SystemTime)>> {
    use std::os::fd::AsRawFd;
    use nix::sys::socket::{recvmsg, MsgFlags, ControlMessageOwned, SockaddrStorage};
    use nix::sys::time::TimeSpec;
    use std::time::{Duration, SystemTime};

    let fd = sock.as_raw_fd();
    let mut iov = [std::io::IoSliceMut::new(buf)];
    let mut cmsg_buf = nix::cmsg_space!(TimeSpec);
    
    match recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty()) {
        Ok(msg) => {
            let timestamp = msg.cmsgs().find_map(|cmsg| {
                if let ControlMessageOwned::ScmTimestampns(ts) = cmsg {
                    let duration = Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32);
                    Some(SystemTime::UNIX_EPOCH + duration)
                } else {
                    None
                }
            }).unwrap_or_else(SystemTime::now);
            
            Ok(Some((msg.bytes, timestamp)))
        }
        Err(nix::errno::Errno::EAGAIN) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

#[cfg(not(unix))]
pub fn recv_with_timestamp(sock: &UdpSocket, buf: &mut [u8]) -> Result<Option<(usize, std::time::SystemTime)>> {
    match sock.recv_from(buf) {
        Ok((size, _)) => Ok(Some((size, std::time::SystemTime::now()))),
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Create a pcap capture handle for PTP traffic on the specified interface
pub fn create_pcap_capture(interface_name: &str) -> Result<Capture<Active>> {
    let mut cap = Capture::from_device(interface_name)?
        .promisc(true)
        .snaplen(256)  // PTP packets are small
        .timeout(1)    // 1ms timeout for polling
        .immediate_mode(true)  // Get packets immediately
        .open()?;

    // Set filter for PTP multicast traffic (UDP ports 319, 320 to 224.0.1.129)
    cap.filter("udp and (port 319 or port 320) and dst host 224.0.1.129", true)?;

    log::info!("Pcap capture created on {} with PTP filter", interface_name);
    Ok(cap)
}

/// Parse an Ethernet frame and extract the PTP payload with timestamp
/// Returns (ptp_payload, size, timestamp) where timestamp is from pcap header
pub fn parse_ptp_from_pcap(data: &[u8], ts_sec: i64, ts_usec: i64) -> Option<(Vec<u8>, usize, SystemTime)> {
    // Minimum sizes: Ethernet (14) + IP (20) + UDP (8) + PTP Header (36)
    if data.len() < 78 {
        return None;
    }

    // Check Ethernet type: 0x0800 = IPv4
    let eth_type = u16::from_be_bytes([data[12], data[13]]);
    if eth_type != 0x0800 {
        return None;
    }

    // Parse IP header
    let ip_header_start = 14;
    let ip_version_ihl = data[ip_header_start];
    let ip_ihl = (ip_version_ihl & 0x0F) as usize * 4;

    if ip_ihl < 20 {
        return None;
    }

    // Check IP protocol: 17 = UDP
    let ip_protocol = data[ip_header_start + 9];
    if ip_protocol != 17 {
        return None;
    }

    // Parse UDP header
    let udp_header_start = ip_header_start + ip_ihl;
    if data.len() < udp_header_start + 8 {
        return None;
    }

    let dst_port = u16::from_be_bytes([data[udp_header_start + 2], data[udp_header_start + 3]]);

    // Verify PTP ports (319 = event, 320 = general)
    if dst_port != 319 && dst_port != 320 {
        return None;
    }

    // Extract PTP payload
    let ptp_start = udp_header_start + 8;
    if data.len() <= ptp_start {
        return None;
    }

    let ptp_payload = data[ptp_start..].to_vec();
    let ptp_size = ptp_payload.len();

    // Convert pcap timestamp to SystemTime
    let timestamp = SystemTime::UNIX_EPOCH + Duration::new(ts_sec as u64, (ts_usec * 1000) as u32);

    Some((ptp_payload, ptp_size, timestamp))
}

/// Receive a PTP packet from pcap capture with accurate kernel timestamp
pub fn recv_pcap_packet(cap: &mut Capture<Active>) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
    match cap.next_packet() {
        Ok(packet) => {
            let ts = &packet.header.ts;
            // tv_sec is i32 on Windows, i64 on Linux - use Into to handle both
            if let Some(result) = parse_ptp_from_pcap(packet.data, ts.tv_sec.into(), ts.tv_usec as i64) {
                Ok(Some(result))
            } else {
                Ok(None)
            }
        }
        Err(pcap::Error::TimeoutExpired) => Ok(None),
        Err(e) => Err(anyhow!("Pcap error: {}", e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a mock Ethernet + IP + UDP + PTP packet
    fn create_mock_ptp_packet(dst_port: u16, ptp_payload: &[u8]) -> Vec<u8> {
        let mut packet = Vec::new();

        // Ethernet header (14 bytes)
        packet.extend_from_slice(&[0x01, 0x00, 0x5e, 0x00, 0x01, 0x81]); // Dst MAC (multicast)
        packet.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]); // Src MAC
        packet.extend_from_slice(&[0x08, 0x00]); // EtherType: IPv4

        // IP header (20 bytes, IHL=5)
        packet.push(0x45); // Version 4, IHL 5
        packet.push(0x00); // DSCP/ECN
        let total_len = 20 + 8 + ptp_payload.len(); // IP + UDP + payload
        packet.extend_from_slice(&(total_len as u16).to_be_bytes()); // Total length
        packet.extend_from_slice(&[0x00, 0x00]); // Identification
        packet.extend_from_slice(&[0x00, 0x00]); // Flags/Fragment
        packet.push(0x40); // TTL
        packet.push(0x11); // Protocol: UDP
        packet.extend_from_slice(&[0x00, 0x00]); // Checksum (don't care)
        packet.extend_from_slice(&[0x0a, 0x00, 0x00, 0x01]); // Src IP
        packet.extend_from_slice(&[0xe0, 0x00, 0x01, 0x81]); // Dst IP: 224.0.1.129

        // UDP header (8 bytes)
        packet.extend_from_slice(&1234u16.to_be_bytes()); // Src port
        packet.extend_from_slice(&dst_port.to_be_bytes()); // Dst port
        let udp_len = 8 + ptp_payload.len();
        packet.extend_from_slice(&(udp_len as u16).to_be_bytes()); // UDP length
        packet.extend_from_slice(&[0x00, 0x00]); // Checksum (don't care)

        // PTP payload
        packet.extend_from_slice(ptp_payload);

        packet
    }

    #[test]
    fn test_parse_ptp_from_pcap_valid_sync() {
        // Create a mock PTP Sync packet (just header for testing)
        let ptp_payload = [0x10u8; 36]; // PTPv1 header
        let packet = create_mock_ptp_packet(319, &ptp_payload);

        let result = parse_ptp_from_pcap(&packet, 1000, 500000);

        assert!(result.is_some());
        let (payload, size, ts) = result.unwrap();
        assert_eq!(size, 36);
        assert_eq!(payload, ptp_payload.to_vec());

        // Verify timestamp: 1000 seconds + 500ms
        let expected_ts = SystemTime::UNIX_EPOCH + Duration::new(1000, 500_000_000);
        assert_eq!(ts, expected_ts);
    }

    #[test]
    fn test_parse_ptp_from_pcap_valid_general() {
        let ptp_payload = [0x10u8; 52]; // PTPv1 FollowUp packet
        let packet = create_mock_ptp_packet(320, &ptp_payload);

        let result = parse_ptp_from_pcap(&packet, 2000, 0);

        assert!(result.is_some());
        let (payload, size, _) = result.unwrap();
        assert_eq!(size, 52);
        assert_eq!(payload, ptp_payload.to_vec());
    }

    #[test]
    fn test_parse_ptp_from_pcap_wrong_port() {
        let ptp_payload = [0x10u8; 36];
        let packet = create_mock_ptp_packet(12345, &ptp_payload); // Wrong port

        let result = parse_ptp_from_pcap(&packet, 1000, 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_ptp_from_pcap_too_short() {
        let packet = vec![0u8; 50]; // Too short for full headers
        let result = parse_ptp_from_pcap(&packet, 1000, 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_ptp_from_pcap_wrong_ethertype() {
        let mut packet = create_mock_ptp_packet(319, &[0x10u8; 36]);
        // Change EtherType from IPv4 (0x0800) to something else
        packet[12] = 0x86;
        packet[13] = 0xDD; // IPv6

        let result = parse_ptp_from_pcap(&packet, 1000, 0);
        assert!(result.is_none());
    }

    #[test]
    fn test_parse_ptp_from_pcap_wrong_protocol() {
        let mut packet = create_mock_ptp_packet(319, &[0x10u8; 36]);
        // Change IP protocol from UDP (17) to TCP (6)
        packet[14 + 9] = 6;

        let result = parse_ptp_from_pcap(&packet, 1000, 0);
        assert!(result.is_none());
    }
}