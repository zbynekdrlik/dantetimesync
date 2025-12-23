//! Windows Winsock-based PTP network implementation with SO_TIMESTAMP.
//!
//! Uses standard Winsock2 APIs with kernel-level timestamping for precise
//! packet arrival times. This approach captures timestamps at the network
//! stack level (not application level), achieving <100µs precision.
//!
//! Key APIs:
//! - WSAIoctl with SIO_TIMESTAMPING to enable timestamping
//! - WSARecvMsg to receive packets with control messages
//! - SO_TIMESTAMP control message contains QPC timestamp

use anyhow::{Result, anyhow};
use log::{info, warn, debug};
use std::net::Ipv4Addr;
use std::time::SystemTime;
use std::mem;
use std::ptr;

use windows::Win32::Networking::WinSock::{
    AF_INET, SOCK_DGRAM, IPPROTO_UDP, IPPROTO_IP,
    SOCKET, SOCKADDR_IN, IN_ADDR, WSADATA,
    WSAStartup, WSACleanup, WSAGetLastError, WSAIoctl,
    socket, bind, closesocket, setsockopt, ioctlsocket, FIONBIO,
    IP_ADD_MEMBERSHIP, IP_MULTICAST_LOOP,
    SOL_SOCKET, SO_REUSEADDR, SO_TIMESTAMP,
    SOCKET_ERROR, INVALID_SOCKET,
    WSAMSG, WSABUF, SEND_RECV_FLAGS,
    SIO_GET_EXTENSION_FUNCTION_POINTER, recv,
};
use windows::Win32::System::IO::OVERLAPPED;
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::GUID;

const PTP_EVENT_PORT: u16 = 319;
const PTP_GENERAL_PORT: u16 = 320;
const PTP_MULTICAST: Ipv4Addr = Ipv4Addr::new(224, 0, 1, 129);

// SIO_TIMESTAMPING constants (not in windows crate, defined per MS docs)
const SIO_TIMESTAMPING: u32 = 0x88000025;
const TIMESTAMPING_FLAG_RX: u32 = 0x1;

// GUID for WSARecvMsg extension function
const WSAID_WSARECVMSG: GUID = GUID::from_u128(0xf689d7c8_6f1f_436b_8a53_e54fe351c322);

/// Control message header - matches WSACMSGHDR/CMSGHDR structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct CmsgHdr {
    cmsg_len: usize,   // Length including header
    cmsg_level: i32,   // Protocol level
    cmsg_type: i32,    // Protocol-specific type
}

/// IP multicast membership request
#[repr(C)]
struct IpMreq {
    imr_multiaddr: u32,
    imr_interface: u32,
}

/// Timestamping configuration structure
#[repr(C)]
struct TimestampingConfig {
    flags: u32,
    tx_timestamp_id: u16,
    reserved: u16,
}

/// WSARecvMsg function type
type WsaRecvMsgFn = unsafe extern "system" fn(
    SOCKET,
    *mut WSAMSG,
    *mut u32,
    *mut OVERLAPPED,
    *mut std::ffi::c_void,  // LPWSAOVERLAPPED_COMPLETION_ROUTINE
) -> i32;

/// PTP network using Winsock with SO_TIMESTAMP for precise timestamps
pub struct WinsockPtpNetwork {
    socket_319: SOCKET,
    socket_320: SOCKET,
    recv_msg_fn: Option<WsaRecvMsgFn>,
    qpc_frequency: i64,
    timestamping_enabled: bool,
}

impl WinsockPtpNetwork {
    pub fn new(interface_ip: Ipv4Addr) -> Result<Self> {
        info!("Initializing Winsock PTP network with SO_TIMESTAMP");

        // Initialize Winsock
        unsafe {
            let mut wsa_data: WSADATA = mem::zeroed();
            let result = WSAStartup(0x0202, &mut wsa_data);
            if result != 0 {
                return Err(anyhow!("WSAStartup failed: {}", result));
            }
        }

        // Get QPC frequency for timestamp conversion
        let qpc_frequency = unsafe {
            let mut freq: i64 = 0;
            let _ = QueryPerformanceFrequency(&mut freq);
            info!("QPC frequency: {} Hz", freq);
            freq
        };

        // Create and configure sockets
        let socket_319 = Self::create_ptp_socket(PTP_EVENT_PORT, interface_ip)?;
        let socket_320 = Self::create_ptp_socket(PTP_GENERAL_PORT, interface_ip)?;

        // Get WSARecvMsg function pointer
        let recv_msg_fn = Self::get_wsarecvmsg_fn(socket_319)?;

        // Enable timestamping on both sockets
        let ts_enabled_319 = Self::enable_timestamping(socket_319);
        let ts_enabled_320 = Self::enable_timestamping(socket_320);

        let timestamping_enabled = ts_enabled_319 && ts_enabled_320;
        if timestamping_enabled {
            info!("SO_TIMESTAMP enabled on both PTP sockets");
        } else {
            warn!("SO_TIMESTAMP not available - falling back to application timestamps");
        }

        info!("Winsock PTP network initialized on {} (ports 319, 320)", interface_ip);

        Ok(WinsockPtpNetwork {
            socket_319,
            socket_320,
            recv_msg_fn,
            qpc_frequency,
            timestamping_enabled,
        })
    }

    fn create_ptp_socket(port: u16, interface_ip: Ipv4Addr) -> Result<SOCKET> {
        unsafe {
            // Create UDP socket
            let sock = socket(AF_INET.0 as i32, SOCK_DGRAM, IPPROTO_UDP.0 as i32);
            if sock == INVALID_SOCKET {
                return Err(anyhow!("Failed to create socket: {}", WSAGetLastError().0));
            }

            // Enable address reuse
            let reuse: i32 = 1;
            if setsockopt(sock, SOL_SOCKET as i32, SO_REUSEADDR as i32,
                         Some(&reuse.to_ne_bytes())) == SOCKET_ERROR {
                warn!("Failed to set SO_REUSEADDR: {}", WSAGetLastError().0);
            }

            // Bind to port
            let addr = SOCKADDR_IN {
                sin_family: AF_INET,
                sin_port: port.to_be(),
                sin_addr: IN_ADDR { S_un: std::mem::zeroed() },
                sin_zero: [0; 8],
            };

            if bind(sock, &addr as *const SOCKADDR_IN as *const _,
                   mem::size_of::<SOCKADDR_IN>() as i32) == SOCKET_ERROR {
                closesocket(sock);
                return Err(anyhow!("Failed to bind port {}: {}", port, WSAGetLastError().0));
            }

            // Join PTP multicast group
            let mreq = IpMreq {
                imr_multiaddr: u32::from_ne_bytes(PTP_MULTICAST.octets()),
                imr_interface: u32::from_ne_bytes(interface_ip.octets()),
            };

            if setsockopt(sock, IPPROTO_IP.0 as i32, IP_ADD_MEMBERSHIP as i32,
                         Some(std::slice::from_raw_parts(
                             &mreq as *const IpMreq as *const u8,
                             mem::size_of::<IpMreq>()
                         ))) == SOCKET_ERROR {
                closesocket(sock);
                return Err(anyhow!("Failed to join multicast: {}", WSAGetLastError().0));
            }

            // Disable multicast loopback
            let loopback: u8 = 0;
            setsockopt(sock, IPPROTO_IP.0 as i32, IP_MULTICAST_LOOP as i32,
                      Some(&[loopback]));

            // Set socket to non-blocking mode
            let mut mode: u32 = 1;
            if ioctlsocket(sock, FIONBIO, &mut mode) == SOCKET_ERROR {
                warn!("Failed to set non-blocking mode: {}", WSAGetLastError().0);
            }

            info!("PTP socket created on port {} (joined 224.0.1.129)", port);
            Ok(sock)
        }
    }

    fn get_wsarecvmsg_fn(sock: SOCKET) -> Result<Option<WsaRecvMsgFn>> {
        unsafe {
            let mut recv_msg_fn: Option<WsaRecvMsgFn> = None;
            let mut bytes_returned: u32 = 0;

            let result = WSAIoctl(
                sock,
                SIO_GET_EXTENSION_FUNCTION_POINTER,
                Some(&WSAID_WSARECVMSG as *const GUID as *const _),
                mem::size_of::<GUID>() as u32,
                Some(&mut recv_msg_fn as *mut _ as *mut _),
                mem::size_of_val(&recv_msg_fn) as u32,
                &mut bytes_returned,
                None,
                None,
            );

            if result == SOCKET_ERROR {
                warn!("WSARecvMsg not available: {}", WSAGetLastError().0);
                return Ok(None);
            }

            info!("WSARecvMsg function pointer obtained");
            Ok(recv_msg_fn)
        }
    }

    fn enable_timestamping(sock: SOCKET) -> bool {
        unsafe {
            // SIO_TIMESTAMPING is the only way to enable timestamping (Windows 10 1809+)
            // SO_TIMESTAMP is NOT a setsockopt option - it's only a cmsg_type for control messages
            let config = TimestampingConfig {
                flags: TIMESTAMPING_FLAG_RX,
                tx_timestamp_id: 0,
                reserved: 0,
            };

            let mut bytes_returned: u32 = 0;

            info!("[TS-Init] Attempting SIO_TIMESTAMPING (ioctl=0x{:08X}, flags=0x{:X})",
                  SIO_TIMESTAMPING, TIMESTAMPING_FLAG_RX);

            let result = WSAIoctl(
                sock,
                SIO_TIMESTAMPING,
                Some(&config as *const TimestampingConfig as *const _),
                mem::size_of::<TimestampingConfig>() as u32,
                None,
                0,
                &mut bytes_returned,
                None,
                None,
            );

            if result != SOCKET_ERROR {
                info!("[TS-Init] SIO_TIMESTAMPING enabled successfully");
                return true;
            }

            let err = WSAGetLastError().0;
            // Common error codes:
            // 10022 = WSAEINVAL (invalid argument or not supported)
            // 10045 = WSAEOPNOTSUPP (operation not supported)
            // 10014 = WSAEFAULT (bad address)
            warn!("[TS-Init] SIO_TIMESTAMPING failed with error {}", err);
            match err {
                10022 => warn!("[TS-Init] WSAEINVAL - ioctl not supported or invalid config. NIC driver may not support timestamping."),
                10045 => warn!("[TS-Init] WSAEOPNOTSUPP - operation not supported on this socket type"),
                _ => warn!("[TS-Init] Check if NIC driver supports Winsock timestamping"),
            }

            false
        }
    }

    /// Receive packet with timestamp using WSARecvMsg
    fn recv_with_timestamp(&mut self, sock: SOCKET) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
        const BUFFER_SIZE: usize = 512;
        const CONTROL_SIZE: usize = 128; // Increased for control messages

        let mut data = vec![0u8; BUFFER_SIZE];
        let mut control = vec![0u8; CONTROL_SIZE];

        unsafe {
            let mut data_buf = WSABUF {
                len: BUFFER_SIZE as u32,
                buf: windows::core::PSTR(data.as_mut_ptr()),
            };

            let mut msg = WSAMSG {
                name: ptr::null_mut(),
                namelen: 0,
                lpBuffers: &mut data_buf,
                dwBufferCount: 1,
                Control: WSABUF {
                    len: CONTROL_SIZE as u32,
                    buf: windows::core::PSTR(control.as_mut_ptr()),
                },
                dwFlags: 0,
            };

            let mut bytes_received: u32 = 0;

            // Use WSARecvMsg if available, otherwise fall back
            let result = if let Some(recv_fn) = self.recv_msg_fn {
                recv_fn(sock, &mut msg, &mut bytes_received, ptr::null_mut(), ptr::null_mut())
            } else {
                debug!("[Recv] WSARecvMsg not available, using fallback");
                return self.recv_fallback(sock);
            };

            if result == SOCKET_ERROR {
                let err = WSAGetLastError().0;
                if err == 10035 { // WSAEWOULDBLOCK
                    return Ok(None);
                }
                return Err(anyhow!("WSARecvMsg failed: {}", err));
            }

            if bytes_received == 0 {
                return Ok(None);
            }

            // Log receive details
            let control_len = msg.Control.len as usize;
            debug!("[Recv] Got {} bytes, control_len={}", bytes_received, control_len);

            // Extract timestamp from control message
            let timestamp = self.extract_timestamp(&control, control_len);

            data.truncate(bytes_received as usize);
            Ok(Some((data, bytes_received as usize, timestamp)))
        }
    }

    /// Extract SO_TIMESTAMP from control message buffer
    fn extract_timestamp(&self, control: &[u8], control_len: usize) -> SystemTime {
        if !self.timestamping_enabled || control_len == 0 {
            debug!("[TS] Timestamping disabled or no control data, using SystemTime::now()");
            return SystemTime::now();
        }

        debug!("[TS] Parsing control message: {} bytes", control_len);

        // Parse CmsgHdr to find SO_TIMESTAMP
        let mut offset = 0;
        let mut msg_count = 0;
        while offset + mem::size_of::<CmsgHdr>() <= control_len {
            let cmsg: &CmsgHdr = unsafe {
                &*(control.as_ptr().add(offset) as *const CmsgHdr)
            };

            if cmsg.cmsg_len == 0 {
                debug!("[TS] Control message {} has zero length, stopping", msg_count);
                break;
            }

            debug!("[TS] Control msg {}: level={} type={} len={}",
                   msg_count, cmsg.cmsg_level, cmsg.cmsg_type, cmsg.cmsg_len);

            // Check for SO_TIMESTAMP (level=SOL_SOCKET, type=SO_TIMESTAMP)
            if cmsg.cmsg_level == SOL_SOCKET as i32 && cmsg.cmsg_type == SO_TIMESTAMP as i32 {
                // Data follows the header
                let data_offset = offset + mem::size_of::<CmsgHdr>();
                if data_offset + 8 <= control_len {
                    let qpc_timestamp: u64 = unsafe {
                        *(control.as_ptr().add(data_offset) as *const u64)
                    };

                    // Get current QPC for comparison
                    let current_qpc = unsafe {
                        let mut qpc: i64 = 0;
                        let _ = QueryPerformanceCounter(&mut qpc);
                        qpc as u64
                    };

                    let latency_qpc = current_qpc.saturating_sub(qpc_timestamp);
                    let latency_us = (latency_qpc as f64 / self.qpc_frequency as f64) * 1_000_000.0;

                    info!("[TS] SO_TIMESTAMP found! QPC={} current={} latency={:.1}us",
                          qpc_timestamp, current_qpc, latency_us);

                    // Convert QPC to SystemTime
                    return self.qpc_to_systemtime(qpc_timestamp);
                }
            }

            // Move to next control message (aligned)
            let aligned_len = (cmsg.cmsg_len + 7) & !7;
            offset += aligned_len;
            msg_count += 1;
        }

        // No timestamp found, fall back
        warn!("[TS] No SO_TIMESTAMP in {} control messages, using SystemTime::now()", msg_count);
        SystemTime::now()
    }

    /// Convert QPC timestamp to SystemTime
    fn qpc_to_systemtime(&self, qpc: u64) -> SystemTime {
        // Get current QPC and SystemTime for reference
        let (current_qpc, current_time) = unsafe {
            let mut qpc_now: i64 = 0;
            let _ = QueryPerformanceCounter(&mut qpc_now);
            (qpc_now as u64, SystemTime::now())
        };

        // Calculate offset in nanoseconds
        let qpc_diff = current_qpc as i64 - qpc as i64;
        let ns_diff = (qpc_diff * 1_000_000_000) / self.qpc_frequency;

        // Subtract from current time (packet arrived before now)
        if ns_diff > 0 {
            current_time - std::time::Duration::from_nanos(ns_diff as u64)
        } else {
            current_time + std::time::Duration::from_nanos((-ns_diff) as u64)
        }
    }

    /// Fallback receive without timestamp
    fn recv_fallback(&self, sock: SOCKET) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
        let mut buffer = vec![0u8; 512];

        unsafe {
            let result = recv(sock, &mut buffer, SEND_RECV_FLAGS(0));

            if result == SOCKET_ERROR {
                let err = WSAGetLastError().0;
                if err == 10035 { // WSAEWOULDBLOCK
                    return Ok(None);
                }
                return Err(anyhow!("recv failed: {}", err));
            }

            if result == 0 {
                return Ok(None);
            }

            let timestamp = SystemTime::now();
            buffer.truncate(result as usize);
            Ok(Some((buffer, result as usize, timestamp)))
        }
    }
}

impl Drop for WinsockPtpNetwork {
    fn drop(&mut self) {
        unsafe {
            closesocket(self.socket_319);
            closesocket(self.socket_320);
            WSACleanup();
        }
        info!("Winsock PTP network closed");
    }
}

impl crate::traits::PtpNetwork for WinsockPtpNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
        // Try event port first (319), then general port (320)
        if let Some(packet) = self.recv_with_timestamp(self.socket_319)? {
            return Ok(Some(packet));
        }

        self.recv_with_timestamp(self.socket_320)
    }

    fn reset(&mut self) -> Result<()> {
        // No state to reset for Winsock sockets
        Ok(())
    }
}
