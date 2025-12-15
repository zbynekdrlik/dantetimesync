use anyhow::{Result, anyhow};
use clap::Parser;
use log::{info, warn, error};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use std::process::Command;
use std::fs::File;
use std::os::unix::io::AsRawFd;

#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use nix::sys::socket::{recvmsg, MsgFlags, ControlMessageOwned, SockaddrStorage};
#[cfg(unix)]
use nix::sys::time::TimeSpec;
#[cfg(unix)]
use nix::fcntl::{flock, FlockArg}; // For Singleton Lock

mod ptp;
mod net;
mod clock;
mod ntp;
mod traits;
mod controller;
mod servo;
mod rtc;

use traits::{NtpSource, PtpNetwork};
use controller::PtpController;
use std::io::ErrorKind;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    interface: Option<String>,

    #[arg(long, default_value = "10.77.8.2")]
    ntp_server: String,

    #[arg(long, default_value_t = false)]
    skip_ntp: bool,
}

// Concrete Implementations for Traits
struct RealNtpSource {
    client: ntp::NtpClient,
}

impl NtpSource for RealNtpSource {
    fn get_offset(&self) -> Result<(Duration, i8)> {
        self.client.get_offset()
    }
}

struct RealPtpNetwork {
    sock_event: std::net::UdpSocket,
    sock_general: std::net::UdpSocket,
}

impl RealPtpNetwork {
    #[cfg(unix)]
    fn recv_with_timestamp(sock: &std::net::UdpSocket, buf: &mut [u8]) -> Result<Option<(usize, SystemTime)>> {
        let fd = sock.as_raw_fd();
        let mut iov = [std::io::IoSliceMut::new(buf)];
        // Space for CMSG (Timestamp)
        let mut cmsg_buf = nix::cmsg_space!(TimeSpec);
        
        match recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsg_buf), MsgFlags::empty()) {
            Ok(msg) => {
                // Check if we got a timestamp
                let timestamp = msg.cmsgs().find_map(|cmsg| {
                    if let ControlMessageOwned::ScmTimestampns(ts) = cmsg {
                        // ts is TimeSpec (tv_sec(), tv_nsec())
                        let duration = Duration::new(ts.tv_sec() as u64, ts.tv_nsec() as u32);
                        Some(SystemTime::UNIX_EPOCH + duration)
                    } else {
                        None
                    }
                }).unwrap_or_else(|| SystemTime::now());
                
                Ok(Some((msg.bytes, timestamp)))
            }
            Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EWOULDBLOCK) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    #[cfg(not(unix))]
    fn recv_with_timestamp(sock: &std::net::UdpSocket, buf: &mut [u8]) -> Result<Option<(usize, SystemTime)>> {
        match sock.recv_from(buf) {
            Ok((size, _)) => Ok(Some((size, SystemTime::now()))),
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

impl PtpNetwork for RealPtpNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
        let mut buf = [0u8; 2048];
        
        loop {
            // Check Event Socket
            match Self::recv_with_timestamp(&self.sock_event, &mut buf) {
                Ok(Some((size, ts))) => {
                    return Ok(Some((buf[..size].to_vec(), size, ts)));
                }
                Ok(None) => {} // Continue to check general
                Err(e) => return Err(e),
            }

            // Check General Socket
            match Self::recv_with_timestamp(&self.sock_general, &mut buf) {
                Ok(Some((size, ts))) => {
                    return Ok(Some((buf[..size].to_vec(), size, ts)));
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }

            return Ok(None);
        }
    }
}

fn stop_conflicting_services() {
    #[cfg(windows)]
    {
        info!("Attempting to stop W32Time service...");
        match Command::new("net").args(["stop", "w32time"]).output() {
            Ok(out) => {
                if out.status.success() {
                    info!("W32Time stopped successfully.");
                } else {
                    let err = String::from_utf8_lossy(&out.stderr);
                    warn!("Failed to stop W32Time (ignoring if already stopped): {}", err);
                }
            }
            Err(e) => warn!("Failed to execute 'net stop w32time': {}", e),
        }
    }

    #[cfg(unix)]
    {
        info!("Ensuring system NTP is disabled (timedatectl set-ntp false)...");
        match Command::new("timedatectl").args(["set-ntp", "false"]).output() {
             Ok(_) => info!("NTP service disabled via timedatectl."),
             Err(e) => warn!("Failed to disable NTP via timedatectl (ignoring): {}", e),
        }
    }
}

fn acquire_singleton_lock() -> Result<File> {
    #[cfg(unix)]
    {
        let lock_path = "/var/run/dantetimesync.lock";
        let file = File::create(lock_path).map_err(|e| anyhow!("Failed to create lock file {}: {}", lock_path, e))?;
        
        // Try to acquire exclusive non-blocking lock
        match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
            Ok(_) => Ok(file),
            Err(nix::errno::Errno::EAGAIN) | Err(nix::errno::Errno::EWOULDBLOCK) => {
                Err(anyhow!("Another instance of dantetimesync is already running! (Lockfile: {})", lock_path))
            }
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(not(unix))]
    {
        // Windows needs named mutex or similar. Skipping for now as requested context is Linux.
        Ok(File::create("dantetimesync.lock")?)
    }
}

fn main() -> Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));
    let args = Args::parse();

    // 0. Singleton Check
    // We hold the file handle. Lock is released when file is closed (process exit).
    let _lock_file = match acquire_singleton_lock() {
        Ok(f) => f,
        Err(e) => {
            error!("{}", e);
            std::process::exit(1);
        }
    };

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        info!("Ctrl+C received. Shutting down...");
        r.store(false, Ordering::SeqCst);
    })?;

    // 1. Stop Conflicting Services
    stop_conflicting_services();

    // 2. Initialize Clock
    let sys_clock = match clock::PlatformClock::new() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to initialize system clock adjustment: {}", e);
            error!("Ensure you are running as Administrator/Root.");
            return Err(e);
        }
    };
    info!("System clock control initialized.");

    // 3. Network Interface
    let (iface, iface_ip) = net::get_default_interface()?;
    info!("Selected Interface: {} ({})", iface.name, iface_ip);
    
    // 4. Sockets
    let sock_event = net::create_multicast_socket(ptp::PTP_EVENT_PORT, iface_ip)?;
    let sock_general = net::create_multicast_socket(ptp::PTP_GENERAL_PORT, iface_ip)?;
    info!("Listening on 224.0.1.129 ports 319 (Event) and 320 (General)");

    let network = RealPtpNetwork {
        sock_event,
        sock_general,
    };
    
    let ntp_source = RealNtpSource {
        client: ntp::NtpClient::new(&args.ntp_server),
    };

    let mut controller = PtpController::new(sys_clock, network, ntp_source);

    // 5. NTP Sync
    controller.run_ntp_sync(args.skip_ntp);

    // 6. Main Loop
    info!("Starting PTP Loop...");
    let mut last_log = Instant::now();
    
    while running.load(Ordering::SeqCst) {
        if last_log.elapsed() >= Duration::from_secs(10) {
            controller.log_status();
            last_log = Instant::now();
        }

        if let Err(e) = controller.process_loop_iteration() {
            warn!("Error in loop: {}", e);
        }
        
        thread::sleep(Duration::from_millis(1));
    }

    info!("Exiting.");
    Ok(())
}
