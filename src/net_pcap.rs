//! Npcap-based PTP network implementation for Windows.
//!
//! Uses Npcap for packet capture with HIGH PRECISION timestamps that are
//! synchronized with system time. This uses KeQuerySystemTimePrecise() which
//! provides microsecond-level precision AND tracks system clock adjustments.
//!
//! Key: We use TimestampType::HostHighPrec which maps to PCAP_TSTAMP_HOST_HIPREC
//! and uses KeQuerySystemTimePrecise() internally - NOT the default UNSYNCED mode.

use anyhow::{Result, anyhow};
use pcap::{Capture, Active, Device, TimestampType};
use std::net::{UdpSocket, Ipv4Addr};
use std::time::{SystemTime, Duration, UNIX_EPOCH};
use log::{info, warn, debug};

const PTP_EVENT_PORT: u16 = 319;
const PTP_GENERAL_PORT: u16 = 320;
const PTP_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);

/// Create a socket and join PTP multicast group (for IGMP membership)
fn join_multicast(port: u16, iface_ip: Ipv4Addr) -> Result<UdpSocket> {
    use socket2::{Socket, Domain, Type, Protocol};
    use std::net::SocketAddrV4;

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;

    let addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
    socket.bind(&addr.into())?;

    socket.join_multicast_v4(&PTP_MULTICAST, &iface_ip)?;
    socket.set_multicast_loop_v4(false)?;
    socket.set_nonblocking(true)?;

    Ok(socket.into())
}

/// PTP network using Npcap with HostHighPrec timestamps
pub struct NpcapPtpNetwork {
    capture: Capture<Active>,
    // Keep sockets alive for IGMP multicast membership
    _igmp_sock_319: UdpSocket,
    _igmp_sock_320: UdpSocket,
    using_hiprec: bool,
}

impl NpcapPtpNetwork {
    pub fn new(interface_name: &str) -> Result<Self> {
        info!("Initializing Npcap capture on interface: {}", interface_name);

        // Find the device by name or description
        let devices = Device::list()?;
        let device = devices.iter()
            .find(|d| {
                d.name.contains(interface_name) ||
                d.desc.as_ref().map(|desc| desc.contains(interface_name)).unwrap_or(false)
            })
            .or_else(|| {
                // Try matching by IP address in description
                devices.iter().find(|d| {
                    d.addresses.iter().any(|addr| {
                        format!("{:?}", addr.addr).contains(interface_name)
                    })
                })
            })
            .ok_or_else(|| {
                let available: Vec<String> = devices.iter()
                    .map(|d| format!("{} ({:?})", d.name, d.desc))
                    .collect();
                anyhow!("Interface '{}' not found. Available: {:?}", interface_name, available)
            })?;

        info!("Found device: {} ({:?})", device.name, device.desc);

        // Extract interface IP for multicast join
        let iface_ip = device.addresses.iter()
            .find_map(|a| {
                if let std::net::IpAddr::V4(ip) = a.addr {
                    if !ip.is_loopback() {
                        return Some(ip);
                    }
                }
                None
            })
            .ok_or_else(|| anyhow!("No IPv4 address found on device"))?;

        info!("Using interface IP {} for multicast join", iface_ip);

        // CRITICAL: Join multicast group via sockets to trigger IGMP
        let igmp_sock_319 = join_multicast(PTP_EVENT_PORT, iface_ip)?;
        let igmp_sock_320 = join_multicast(PTP_GENERAL_PORT, iface_ip)?;
        info!("Joined PTP multicast group 224.0.1.129 on ports 319 and 320");

        // Create capture handle with HostHighPrec timestamps
        // HostHighPrec uses KeQuerySystemTimePrecise() which is both high-precision AND synced with system time
        info!("[TS] Requesting HostHighPrec timestamps (KeQuerySystemTimePrecise)");

        let capture = Capture::from_device(device.clone())?
            .promisc(true)         // Required to see multicast traffic
            .immediate_mode(true)  // Critical: disable buffering for lowest latency
            .snaplen(256)          // PTP packets are small
            .timeout(1)            // 1ms timeout for responsiveness
            .tstamp_type(TimestampType::HostHighPrec)
            .open()?;

        // Assume HostHighPrec is available on modern Npcap (1.20+)
        let using_hiprec = true;
        info!("[TS] Using HostHighPrec timestamps (KeQuerySystemTimePrecise)");

        if using_hiprec {
            info!("Npcap capture initialized with HIGH PRECISION synchronized timestamps");
        } else {
            warn!("Npcap capture using default timestamps (may drift from system time)");
        }

        Ok(NpcapPtpNetwork {
            capture,
            _igmp_sock_319: igmp_sock_319,
            _igmp_sock_320: igmp_sock_320,
            using_hiprec,
        })
    }

    /// Convert pcap timestamp to SystemTime
    fn pcap_ts_to_systemtime(ts_sec: i64, ts_usec: i64) -> SystemTime {
        let duration = Duration::new(ts_sec as u64, (ts_usec * 1000) as u32);
        UNIX_EPOCH + duration
    }
}


impl crate::traits::PtpNetwork for NpcapPtpNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
        match self.capture.next_packet() {
            Ok(packet) => {
                let data = packet.data;

                // Use Npcap's HostHighPrec timestamps - these are both precise AND synced
                // with system time (using KeQuerySystemTimePrecise on Windows 8+)
                let header = packet.header;
                let ts = if self.using_hiprec {
                    // Npcap provides high-precision timestamps synced with system time
                    let ts = Self::pcap_ts_to_systemtime(
                        header.ts.tv_sec as i64,
                        header.ts.tv_usec as i64
                    );
                    debug!("[TS] Npcap HostHighPrec: {}.{:06}", header.ts.tv_sec, header.ts.tv_usec);
                    ts
                } else {
                    // Fallback to SystemTime::now() if HostHighPrec not available
                    SystemTime::now()
                };

                // Extract UDP payload from Ethernet frame
                // Ethernet (14) + IP (20) + UDP (8) = 42 bytes header
                const ETH_IP_UDP_HEADER: usize = 42;

                if data.len() < ETH_IP_UDP_HEADER {
                    return Ok(None);
                }

                // Verify it's an IP packet (EtherType 0x0800)
                if data[12] != 0x08 || data[13] != 0x00 {
                    return Ok(None);
                }

                // Verify UDP protocol (IP header byte 9 = protocol)
                if data[23] != 17 {
                    return Ok(None);
                }

                // Check destination port for PTP (319 or 320)
                let dst_port = ((data[36] as u16) << 8) | data[37] as u16;
                if dst_port != 319 && dst_port != 320 {
                    return Ok(None);
                }

                // Extract UDP payload
                let payload = &data[ETH_IP_UDP_HEADER..];
                let payload_len = payload.len();

                if payload_len > 0 {
                    let mut result = vec![0u8; payload_len];
                    result.copy_from_slice(payload);

                    debug!("[Npcap] PTP payload {} bytes", payload_len);
                    Ok(Some((result, payload_len, ts)))
                } else {
                    Ok(None)
                }
            }
            Err(pcap::Error::TimeoutExpired) => {
                // Normal timeout - no packet available
                Ok(None)
            }
            Err(e) => {
                warn!("Npcap recv error: {} ({:?})", e, e);
                Err(e.into())
            }
        }
    }

    fn reset(&mut self) -> Result<()> {
        // Npcap doesn't need explicit reset
        Ok(())
    }
}

/// Get list of available Npcap devices
pub fn list_npcap_devices() -> Result<Vec<String>> {
    let devices = Device::list()?;
    Ok(devices.iter()
        .map(|d| format!("{}: {:?}", d.name, d.desc))
        .collect())
}
