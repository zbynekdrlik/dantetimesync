use anyhow::{Result, anyhow};
use clap::Parser;
use log::{info, warn, error};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use std::process::Command;
use std::fs::File;

#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(unix)]
use nix::sys::socket::{recvmsg, MsgFlags, ControlMessageOwned, SockaddrStorage};
#[cfg(unix)]
use nix::sys::time::TimeSpec;
#[cfg(unix)]
use nix::fcntl::{flock, FlockArg};

#[cfg(windows)]
use windows::Win32::System::Threading::{SetPriorityClass, GetCurrentProcess, REALTIME_PRIORITY_CLASS, HIGH_PRIORITY_CLASS};
#[cfg(windows)]
use windows::Win32::Media::timeBeginPeriod;

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
        // Windows Fallback: User-space timestamping.
        // Precision is limited by scheduler jitter, but mitigated by Realtime Priority and timeBeginPeriod(1).
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
                    // Ignore if already stopped (code 2)
                    let err = String::from_utf8_lossy(&out.stderr);
                    if !err.contains("The service has not been started") {
                         warn!("Failed to stop W32Time: {}", err);
                    }
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

fn enable_realtime_priority() {
    #[cfg(unix)]
    {
        unsafe {
            let policy = libc::SCHED_FIFO;
            let param = libc::sched_param { sched_priority: 50 };
            
            if libc::sched_setscheduler(0, policy, &param) == 0 {
                info!("Realtime priority (SCHED_FIFO, 50) enabled successfully.");
            } else {
                let err = std::io::Error::last_os_error();
                warn!("Failed to set realtime priority: {}. Latency might suffer.", err);
            }
        }
    }
    #[cfg(windows)]
    {
        unsafe {
            // Set REALTIME_PRIORITY_CLASS (Highest possible)
            // If this is too dangerous (hangs UI), HIGH_PRIORITY_CLASS is fallback.
            // But for SOTA sync, Realtime is preferred.
            if SetPriorityClass(GetCurrentProcess(), REALTIME_PRIORITY_CLASS).as_bool() {
                info!("Windows Realtime Priority enabled.");
            } else {
                warn!("Failed to set Windows Realtime Priority. Trying High...");
                if SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS).as_bool() {
                    info!("Windows High Priority enabled.");
                } else {
                    warn!("Failed to set Windows priority.");
                }
            }
            
            // Force 1ms Timer Resolution
            if timeBeginPeriod(1) == 0 { // TIMERR_NOERROR = 0
                info!("Windows High-Res Timer (1ms) enabled.");
            } else {
                warn!("Failed to set Windows High-Res Timer.");
            }
        }
    }
}

fn acquire_singleton_lock() -> Result<File> {
    #[cfg(unix)]
    {
        let lock_path = "/var/run/dantetimesync.lock";
        let file = File::create(lock_path).map_err(|e| anyhow!("Failed to create lock file {}: {}", lock_path, e))?;
        
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
        // Simple file check for Windows (flock not available in std)
        // A robust Windows mutex would use NamedMutex, but File locking is harder to make non-blocking/exclusive cross-process without winapi.
        // For now, we rely on the Service Manager (SCM) single instance behavior.
        // Or create a dummy file and keep it open (Windows file sharing rules default to exclusive write).
        let file = File::create("dantetimesync.lock")?;
        Ok(file)
    }
}

fn main() -> Result<()> {
    env_logger::builder()
        .format_timestamp(None)
        .filter_level(log::LevelFilter::Info)
        .init();

    let args = Args::parse();

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

    stop_conflicting_services();
    
    // Enable Priority/Timers BEFORE clock/net init
    enable_realtime_priority();

    let sys_clock = match clock::PlatformClock::new() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to initialize system clock adjustment: {}", e);
            error!("Ensure you are running as Administrator/Root.");
            return Err(e);
        }
    };
    info!("System clock control initialized.");

    let (iface, iface_ip) = net::get_default_interface()?;
    info!("Selected Interface: {} ({})", iface.name, iface_ip);
    
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

    controller.run_ntp_sync(args.skip_ntp);

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
