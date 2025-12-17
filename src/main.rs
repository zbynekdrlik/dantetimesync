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
use nix::sys::socket::{recvmsg, MsgFlags, ControlMessageOwned, SockaddrStorage};
#[cfg(unix)]
use nix::sys::time::TimeSpec;
#[cfg(unix)]
use nix::fcntl::{flock, FlockArg};

#[cfg(windows)]
use windows::Win32::System::Threading::{SetPriorityClass, GetCurrentProcess, REALTIME_PRIORITY_CLASS, HIGH_PRIORITY_CLASS};
#[cfg(windows)]
use windows::Win32::Media::timeBeginPeriod;

#[cfg(windows)]
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
#[cfg(windows)]
use windows::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
#[cfg(windows)]
use windows::Win32::System::Pipes::{CreateNamedPipeW, NAMED_PIPE_MODE};
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::{FILE_FLAGS_AND_ATTRIBUTES};
#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::{INVALID_HANDLE_VALUE, HANDLE, HLOCAL};
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(windows)]
use std::os::windows::io::FromRawHandle;

// Constants for Pipe (Manual definition to avoid import issues)
#[cfg(windows)]
const PIPE_ACCESS_OUTBOUND: u32 = 0x00000002;
#[cfg(windows)]
const FILE_FLAG_OVERLAPPED: u32 = 0x40000000;
#[cfg(windows)]
const PIPE_TYPE_MESSAGE: u32 = 0x00000004;
#[cfg(windows)]
const PIPE_READMODE_MESSAGE: u32 = 0x00000002;
#[cfg(windows)]
const PIPE_WAIT: u32 = 0x00000000;
#[cfg(windows)]
const PIPE_UNLIMITED_INSTANCES: u32 = 255;

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn LocalFree(hMem: HLOCAL) -> HLOCAL;
}

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

#[cfg(windows)]
struct PcapPtpNetwork {
    capture: pcap::Capture<pcap::Active>,
}

#[cfg(windows)]
impl PcapPtpNetwork {
    fn new(device_name: &str) -> Result<Self> {
        let mut cap = pcap::Capture::from_device(device_name)?
            .promisc(true)
            .snaplen(2048)
            .timeout(10) // ms
            .open()?;
            
        cap.filter("udp port 319 or udp port 320", true)?;
        
        Ok(Self { capture: cap })
    }
}

#[cfg(windows)]
impl PtpNetwork for PcapPtpNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
        match self.capture.next_packet() {
            Ok(packet) => {
                let ts_sec = packet.header.ts.tv_sec as u64;
                let ts_usec = packet.header.ts.tv_usec as u32;
                let timestamp = SystemTime::UNIX_EPOCH + Duration::new(ts_sec, ts_usec * 1000);
                
                let data = packet.data;
                // Basic IPv4 parsing (Ethernet 14 + IP 20 + UDP 8 = 42)
                if data.len() < 42 { return Ok(None); }
                
                // EtherType 0x0800 (IPv4)
                if data[12] == 0x08 && data[13] == 0x00 {
                    let ip_header_len = (data[14] & 0x0F) * 4;
                    let udp_offset = 14 + ip_header_len as usize;
                    let payload_offset = udp_offset + 8;
                    
                    if data.len() > payload_offset {
                        let payload = data[payload_offset..].to_vec();
                        return Ok(Some((payload, payload.len(), timestamp)));
                    }
                }
                Ok(None)
            }
            Err(pcap::Error::TimeoutExpired) => Ok(None),
            Err(e) => Err(anyhow::Error::from(e)),
        }
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct Config {
    ntp_server: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ntp_server: "10.77.8.2".to_string(),
        }
    }
}

fn load_config() -> Config {
    #[cfg(windows)]
    let path = r"C:\ProgramData\DanteTimeSync\config.json";
    #[cfg(not(windows))]
    let path = "/etc/dantetimesync/config.json"; 

    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(cfg) = serde_json::from_str(&content) {
            return cfg;
        }
    }
    
    let cfg = Config::default();
    if let Ok(bytes) = serde_json::to_string_pretty(&cfg) {
        let _ = std::fs::write(path, bytes);
    }
    cfg
}

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
            
            // Named pipe server loop
            loop {
                // Create pipe manually with Security Descriptor to allow Users to connect to Service
                // SDDL: D:(A;;GA;;;WD) -> DACL: Allow Generic All to World (Everyone)
                // This is needed because Service runs as SYSTEM and Tray runs as User.
                let pipe_name_wide: Vec<u16> = r"\\.\pipe\dantetimesync".encode_utf16().chain(std::iter::once(0)).collect();
                let sddl_wide: Vec<u16> = "D:(A;;GA;;;WD)".encode_utf16().chain(std::iter::once(0)).collect();
                
                let mut sd = PSECURITY_DESCRIPTOR::default();
                let mut sa = SECURITY_ATTRIBUTES {
                    nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                    lpSecurityDescriptor: std::ptr::null_mut(),
                    bInheritHandle: false.into(),
                };

                let handle = unsafe {
                    if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                        PCWSTR(sddl_wide.as_ptr()),
                        SDDL_REVISION_1, 
                        &mut sd, 
                        None
                    ).is_ok() {
                        sa.lpSecurityDescriptor = sd.0;
                        
                        let h = CreateNamedPipeW(
                            PCWSTR(pipe_name_wide.as_ptr()),
                            FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED), 
                            NAMED_PIPE_MODE(0), // Byte mode (0) for Tokio compatibility
                            PIPE_UNLIMITED_INSTANCES,
                            1024,
                            1024,
                            0,
                            Some(&sa)
                        );
                        
                        let _ = LocalFree(std::mem::transmute(sd));
                        h
                    } else {
                        // Fallback if SDDL fails (shouldn't happen)
                        windows::Win32::Foundation::INVALID_HANDLE_VALUE
                    }
                };

                if handle == windows::Win32::Foundation::INVALID_HANDLE_VALUE {
                    error!("Failed to create named pipe with SDDL. Retrying...");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }

                // Wrap in Tokio
                let mut server = unsafe { 
                    match NamedPipeServer::from_raw_handle(handle.0 as *mut std::ffi::c_void) {
                        Ok(s) => s,
                        Err(e) => {
                            error!("Failed to wrap named pipe handle: {}", e);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
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

    // Initialize Shared Status
    let status_shared = Arc::new(RwLock::new(SyncStatus::default()));

    // Start IPC Server immediately (so Tray App can connect even if network is down)
    start_ipc_server(status_shared.clone());

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

    // Network Interface Selection (Retry Loop)
    let (iface_name, iface_ip) = loop {
        match net::get_default_interface() {
            Ok(res) => break res,
            Err(e) => {
                if !running.load(Ordering::SeqCst) {
                    return Ok(());
                }
                warn!("Waiting for network interface... ({})", e);
                thread::sleep(Duration::from_secs(5));
            }
        }
    };
    
    // Create sockets to join multicast groups (IGMP)
    let sock_event = net::create_multicast_socket(ptp::PTP_EVENT_PORT, iface_ip)?;
    let sock_general = net::create_multicast_socket(ptp::PTP_GENERAL_PORT, iface_ip)?;
    info!("Joined Multicast Groups on {} ({})", iface_name, iface_ip);

    #[cfg(unix)]
    let network = RealPtpNetwork {
        sock_event,
        sock_general,
    };

    #[cfg(windows)]
    let network = {
        // On Windows, use Pcap for kernel timestamps
        info!("Opening Npcap capture on device: {}", iface_name);
        PcapPtpNetwork::new(&iface_name)?
    };
    
    let ntp_source = RealNtpSource {
        client: ntp::NtpClient::new(&args.ntp_server),
    };

    let mut controller = PtpController::new(sys_clock, network, ntp_source, status_shared);
    
    if !args.skip_ntp {
        info!("Using NTP Server: {}", args.ntp_server);
    }
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
                };
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
fn run_service_logic(_args: Args) -> Result<()> {
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
            Err(_) => return, 
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

        // Use global args via simple parse if needed, but here we just need default or what logic needs
        let args = Args::parse();
        
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
    let mut args = Args::parse();
    let config = load_config();

    // Use config if arg is default
    if args.ntp_server == "10.77.8.2" {
        args.ntp_server = config.ntp_server;
    }

    #[cfg(windows)]
    if args.service {
        // Initialize File Logging for Service
        let log_path = r"C:\ProgramData\DanteTimeSync\dantetimesync.log";
        
        // Log Rotation (Simple: 1MB limit check on startup)
        if let Ok(metadata) = std::fs::metadata(log_path) {
            if metadata.len() > 1_000_000 {
                let old_path = format!("{}.old", log_path);
                let _ = std::fs::rename(log_path, old_path);
            }
        }

        if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).write(true).open(log_path) {
             let target = env_logger::Target::Pipe(Box::new(file));
             env_logger::builder()
                .target(target)
                .filter_level(log::LevelFilter::Info)
                .format_timestamp_millis()
                .init();
        } else {
             // Fallback
             env_logger::builder().filter_level(log::LevelFilter::Info).init();
        }
        
        info!("Service Started: v{}", env!("CARGO_PKG_VERSION"));
        return run_service_logic(args);
    }

    // Console Mode Logging
    env_logger::builder()
        .format_timestamp(None)
        .filter_level(log::LevelFilter::Info)
        .init();

    // Log Version immediately
    info!("Dante Time Sync v{}", env!("CARGO_PKG_VERSION"));

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