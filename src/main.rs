use anyhow::{Result, anyhow};
use clap::Parser;
use log::{info, warn, error};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use std::process::Command;
use std::fs::File;
use std::io::ErrorKind;

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

// Service Imports
#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use windows_service::
    {
        define_windows_service,
        service::
            {
                ServiceControl,
                ServiceControlAccept,
                ServiceExitCode,
                ServiceState,
                ServiceStatus,
                ServiceType,
            },
        service_control_handler::{self, ServiceControlHandlerResult},
        service_dispatcher,
    };

mod ptp;
mod net;
mod clock;
mod ntp;
mod traits;
mod controller;
mod servo;
mod status;
#[cfg(unix)]
mod rtc;

use traits::{NtpSource, PtpNetwork};
use controller::PtpController;
use status::SyncStatus;

#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    interface: Option<String>,

    #[arg(long, default_value = "10.77.8.2")]
    ntp_server: String,

    #[arg(long, default_value_t = false)]
    skip_ntp: bool,

    #[arg(long, default_value_t = false)]
    service: bool,
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
            Err(nix::errno::Errno::EAGAIN) => Ok(None),
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
                    // Ignore errors if already stopped
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
            if SetPriorityClass(GetCurrentProcess(), REALTIME_PRIORITY_CLASS).is_ok() {
                info!("Windows Realtime Priority enabled.");
            } else {
                warn!("Failed to set Windows Realtime Priority. Trying High...");
                if SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS).is_ok() {
                    info!("Windows High Priority enabled.");
                } else {
                    warn!("Failed to set Windows priority.");
                }
            }
            if timeBeginPeriod(1) == 0 { 
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
            Err(nix::errno::Errno::EAGAIN) => {
                Err(anyhow!("Another instance of dantetimesync is already running! (Lockfile: {})", lock_path))
            }
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(not(unix))]
    {
        // On Windows, file locking prevents deletion but not necessarily running if logic differs.
        // But File::create opens/truncates.
        // We want shared read, exclusive write?
        // Simple create is fine for now if we hold the handle.
        let file = File::create("dantetimesync.lock")?;
        Ok(file)
    }
}

// --- IPC Server (Windows) ---
#[cfg(windows)]
fn start_ipc_server(status: Arc<RwLock<SyncStatus>>) {
    thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build tokio runtime for IPC");
        
        rt.block_on(async move {
            use tokio::io::AsyncWriteExt;
            use tokio::net::windows::named_pipe::ServerOptions;
            
            // Named pipe server loop
            loop {
                // Create new instance for each connection
                // First instance should set first_pipe_instance(true) but if we just loop it might fail if one exists?
                // Actually tokio ServerOptions handles creating instances.
                // We create one, connect, handle, loop.
                let mut server = match ServerOptions::new()
                    .create(r"\\.\pipe\dantetimesync") 
                {
                    Ok(s) => s,
                    Err(_) => {
                        // Maybe pipe exists? Wait a bit.
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };

                if server.connect().await.is_ok() {
                    let s = { status.read().unwrap().clone() };
                    if let Ok(bytes) = serde_json::to_vec(&s) {
                        let len = (bytes.len() as u32).to_le_bytes();
                        let _ = server.write_all(&len).await;
                        let _ = server.write_all(&bytes).await;
                    }
                }
            }
        });
    });
}

#[cfg(not(windows))]
fn start_ipc_server(_status: Arc<RwLock<SyncStatus>>) {
    // No-op on Linux for now (or implement Unix Domain Socket)
}

// --- Sync Loop ---
fn run_sync_loop(args: Args, running: Arc<AtomicBool>) -> Result<()> {
    // Notify systemd (Linux) that we are starting
    #[cfg(unix)]
    {
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Status(format!("v{} | Starting...", env!("CARGO_PKG_VERSION")).as_str())]);
    }

    stop_conflicting_services();
    enable_realtime_priority();

    let sys_clock = match clock::PlatformClock::new() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to initialize system clock adjustment: {}", e);
            return Err(e);
        }
    };
    info!("System clock control initialized.");

    let (_, iface_ip) = net::get_default_interface()?;
    // info!("Selected Interface IP: {}", iface_ip);
    
    let sock_event = net::create_multicast_socket(ptp::PTP_EVENT_PORT, iface_ip)?;
    let sock_general = net::create_multicast_socket(ptp::PTP_GENERAL_PORT, iface_ip)?;
    info!("Listening on 224.0.1.129 ports 319/320");

    let network = RealPtpNetwork {
        sock_event,
        sock_general,
    };
    
    let ntp_source = RealNtpSource {
        client: ntp::NtpClient::new(&args.ntp_server),
    };

    let mut controller = PtpController::new(sys_clock, network, ntp_source);
    
    // Start IPC Server with shared status
    start_ipc_server(controller.get_status_shared());

    controller.run_ntp_sync(args.skip_ntp);

    info!("Starting PTP Loop...");
    
    // Notify systemd we are ready and loop is running
    #[cfg(unix)]
    {
        let _ = sd_notify::notify(false, &[
            sd_notify::NotifyState::Ready, 
            sd_notify::NotifyState::Status(format!("v{} | PTP Loop Running", env!("CARGO_PKG_VERSION")).as_str())
        ]);
    }

    let mut last_log = Instant::now();
    
    while running.load(Ordering::SeqCst) {
        if last_log.elapsed() >= Duration::from_secs(10) {
            controller.log_status();
            
            // Update systemd status with latest metrics
            #[cfg(unix)]
            {
                let s = controller.get_status_shared();
                if let Ok(status) = s.read() {
                    let status_str = if status.settled {
                        format!("v{} | Locked | Offset: {:.3} µs", env!("CARGO_PKG_VERSION"), status.offset_ns as f64 / 1000.0)
                    } else {
                        format!("v{} | Settling...", env!("CARGO_PKG_VERSION"))
                    };
                    let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Status(&status_str)]);
                }
            }
            
            last_log = Instant::now();
        }

        if let Err(e) = controller.process_loop_iteration() {
            warn!("Error in loop: {}", e);
        }
        
        thread::sleep(Duration::from_millis(1));
    }

    info!("Sync Loop Exiting.");
    #[cfg(unix)]
    {
        let _ = sd_notify::notify(false, &[sd_notify::NotifyState::Stopping]);
    }
    Ok(())
}

// --- Windows Service Entry ---
#[cfg(windows)]
const SERVICE_NAME: &str = "dantetimesync";

#[cfg(windows)]
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

#[cfg(windows)]
fn run_service_logic(args: Args) -> Result<()> {
    define_windows_service!(ffi_service_main, my_service_main);

    fn my_service_main(_arguments: Vec<OsString>) {
        let (shutdown_tx, shutdown_rx) = std::sync::mpsc::channel();

        let event_handler = move |control_event| -> ServiceControlHandlerResult {
            match control_event {
                ServiceControl::Stop => {
                    let _ = shutdown_tx.send(());
                    ServiceControlHandlerResult::NoError
                }
                ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
                _ => ServiceControlHandlerResult::NotImplemented,
            }
        };

        let status_handle = match service_control_handler::register(SERVICE_NAME, event_handler) {
            Ok(h) => h,
            Err(_) => return, // Can't log easily if logger not set up?
        };

        // Set Running
        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Running,
            controls_accepted: ServiceControlAccept::STOP,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });

        // Parse args again or use global? 
        // We can't pass args into service_main easily.
        // We'll parse defaults or basic args.
        let args = Args::parse(); // Should work from command line params passed to service?
        // Service args are passed in `arguments`.
        
        let running = Arc::new(AtomicBool::new(true));
        let r = running.clone();
        
        // Spawn the sync loop in a thread
        let handle = thread::spawn(move || {
            if let Err(e) = run_sync_loop(args, r) {
                error!("Service loop failed: {}", e);
            }
        });

        // Wait for stop signal
        let _ = shutdown_rx.recv();
        
        // Stop
        running.store(false, Ordering::SeqCst);
        let _ = handle.join();

        let _ = status_handle.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: ServiceState::Stopped,
            controls_accepted: ServiceControlAccept::empty(),
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });
    }

    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn main() -> Result<()> {
    env_logger::builder()
        .format_timestamp(None)
        .filter_level(log::LevelFilter::Info)
        .init();

    // Log Version immediately
    info!("Dante Time Sync v{}", env!("CARGO_PKG_VERSION"));

    let args = Args::parse();

    #[cfg(windows)]
    if args.service {
        return run_service_logic(args);
    }

    // Console Mode
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

    run_sync_loop(args, running)
}
