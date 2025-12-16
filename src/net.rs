use anyhow::{Result, anyhow};
use socket2::{Socket, Domain, Type, Protocol};
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};
use if_addrs::get_if_addrs;

#[cfg(unix)]
use std::os::fd::AsFd;
#[cfg(unix)]
use nix::sys::socket::{setsockopt, sockopt};

pub fn get_default_interface() -> Result<(String, Ipv4Addr)> {
    let interfaces = get_if_addrs()?;
    
    // We want a non-loopback IPv4 interface.
    // if-addrs returns one entry per IP.
    
    let valid_interfaces: Vec<_> = interfaces.iter()
        .filter(|iface| !iface.is_loopback() && iface.ip().is_ipv4())
        .collect();

    if valid_interfaces.is_empty() {
        log::warn!("No suitable network interface found. Diagnostics:");
        for iface in &interfaces {
            log::warn!(" - Name: '{}', IP: {:?}, Loopback: {}", 
                iface.name, iface.ip(), iface.is_loopback());
        }
        return Err(anyhow!("No suitable network interface found"));
    }

    // Heuristic: Prefer non-wireless? 
    // On Windows, names might be GUIDs, so "wifi" check is unreliable without description.
    // We'll just pick the first valid one, or try to avoid 169.254 (APIPA).
    
    let mut best_iface = None;
    
    for iface in valid_interfaces {
        let ip = if let IpAddr::V4(ip) = iface.ip() { ip } else { continue };
        
        // Skip APIPA (169.254.x.x) if possible, unless it's the only one.
        if ip.is_link_local() {
            if best_iface.is_none() {
                best_iface = Some((iface.name.clone(), ip));
            }
            continue;
        }
        
        // Found a good IP
        return Ok((iface.name.clone(), ip));
    }

    // Fallback to link-local if found
    if let Some(res) = best_iface {
        return Ok(res);
    }

    Err(anyhow!("No suitable IPv4 interface found"))
}

pub fn create_multicast_socket(port: u16, interface_ip: Ipv4Addr) -> Result<UdpSocket> {
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
        // Enable Kernel Timestamping (SO_TIMESTAMPNS)
        // Pass &udp_socket which implements AsFd
        match setsockopt(&udp_socket, sockopt::ReceiveTimestampns, &true) {
            Ok(_) => log::info!("Kernel timestamping (SO_TIMESTAMPNS) enabled."),
            Err(e) => log::warn!("Failed to enable kernel timestamping: {}", e),
        }
    }

    Ok(udp_socket)
}