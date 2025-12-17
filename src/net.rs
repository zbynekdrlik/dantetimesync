use anyhow::{Result, anyhow};
use socket2::{Socket, Domain, Type, Protocol};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};
use pcap::Device;

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
            if let Some(socket_addr) = a.addr.as_socket_addr() {
                socket_addr.is_ipv4() && !socket_addr.ip().is_loopback()
            } else {
                false
            }
        });

        if let Some(ipv4_addr) = ipv4 {
            let ip = if let std::net::SocketAddr::V4(addr) = ipv4_addr.addr.as_socket_addr().unwrap() {
                *addr.ip()
            } else {
                continue;
            };

            // Prefer non-wireless/non-loopback (already filtered loopback).
            // Pcap names are like \Device\NPF_{...}. Description has text.
            let desc = dev.description.as_deref().unwrap_or("").to_lowercase();
            let is_wireless = desc.contains("wireless") || desc.contains("wi-fi") || desc.contains("wlan");
            
            if !is_wireless {
                return Ok((dev.name.clone(), ip));
            } else if best_iface.is_none() {
                best_iface = Some((dev.name.clone(), ip));
            }
        }
    }

    if let Some(res) = best_iface {
        return Ok(res);
    }

    // Diagnostics
    log::warn!("No suitable IPv4 interface found. Diagnostics:");
    for dev in devices {
        log::warn!(" - Name: {}, Desc: {:?}, Addrs: {:?}", dev.name, dev.description, dev.addresses);
    }

    Err(anyhow!("No suitable IPv4 interface found"))
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