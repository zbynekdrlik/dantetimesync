use anyhow::Result;
use clap::Parser;
use log::{error, info, warn};
use std::fs::File;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use anyhow::anyhow;
#[cfg(unix)]
use nix::fcntl::{flock, FlockArg};
#[cfg(unix)]
use std::io::ErrorKind;
#[cfg(unix)]
use std::net::UdpSocket;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;
#[cfg(unix)]
use std::time::SystemTime;

#[cfg(windows)]
use windows::Win32::Media::timeBeginPeriod;
#[cfg(windows)]
use windows::Win32::System::Threading::{
    GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS, REALTIME_PRIORITY_CLASS,
};

#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(windows)]
use windows::core::PCWSTR;
#[cfg(windows)]
use windows::Win32::Foundation::HLOCAL;
#[cfg(windows)]
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
#[cfg(windows)]
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
#[cfg(windows)]
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
#[cfg(windows)]
use windows::Win32::System::Pipes::{CreateNamedPipeW, NAMED_PIPE_MODE};

// Constants for Pipe (Manual definition to avoid import issues)
#[cfg(windows)]
const PIPE_ACCESS_OUTBOUND: u32 = 0x00000002;
#[cfg(windows)]
const FILE_FLAG_OVERLAPPED: u32 = 0x40000000;
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
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

// Use library crate modules
#[cfg(windows)]
use dantesync::net_pcap;
#[cfg(unix)]
use dantesync::ptp;
use dantesync::{clock, config, controller, net, ntp, status, traits};

use config::SystemConfig;
use controller::PtpController;
use serde::{Deserialize, Serialize};
use status::SyncStatus;
use traits::NtpSource;
#[cfg(unix)]
use traits::PtpNetwork;

/// Simplified configuration - only NTP server needs to be managed
/// All other parameters auto-adjust based on platform defaults
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Config {
    ntp_server: String,

    /// Advanced system tuning (optional - uses auto-optimized defaults if omitted)
    #[serde(default)]
    system: SystemConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ntp_server: "10.77.8.2".to_string(),
            system: SystemConfig::default(),
        }
    }
}

fn load_config() -> Config {
    #[cfg(windows)]
    let path = r"C:\ProgramData\DanteSync\config.json";
    #[cfg(not(windows))]
    let path = "/etc/dantesync/config.json";

    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(cfg) = serde_json::from_str::<Config>(&content) {
            return cfg;
        }
    }

    // Create simple config with only ntp_server (system defaults auto-apply)
    let cfg = Config::default();
    let simple_config = r#"{
  "ntp_server": "10.77.8.2"
}"#;
    let _ = std::fs::write(path, simple_config);
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

// Legacy UDP-based PTP network (used on Linux with kernel timestamping)
#[cfg(unix)]
struct RealPtpNetwork {
    sock_event: UdpSocket,
    sock_general: UdpSocket,
}

#[cfg(unix)]
impl PtpNetwork for RealPtpNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
        let mut buf = [0u8; 2048];

        // Check Event Socket first
        match net::recv_with_timestamp(&self.sock_event, &mut buf) {
            Ok(Some((size, ts))) => {
                return Ok(Some((buf[..size].to_vec(), size, ts)));
            }
            Ok(None) => {} // Continue to check general
            Err(e) => return Err(e),
        }

        // Check General Socket
        match net::recv_with_timestamp(&self.sock_general, &mut buf) {
            Ok(Some((size, ts))) => {
                return Ok(Some((buf[..size].to_vec(), size, ts)));
            }
            Ok(None) => {} // No data on either socket
            Err(e) => return Err(e),
        }

        Ok(None)
    }

    fn reset(&mut self) -> Result<()> {
        // Drain buffers to prevent processing old packets after a clock step
        let mut buf = [0u8; 2048];
        loop {
            match self.sock_event.recv_from(&mut buf) {
                Ok(_) => continue,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        loop {
            match self.sock_general.recv_from(&mut buf) {
                Ok(_) => continue,
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
        Ok(())
    }
}

// Windows uses Npcap for precise packet timestamps with HostHighPrec mode
// See net_pcap::NpcapPtpNetwork - uses KeQuerySystemTimePrecise() for synchronized timestamps

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
        match Command::new("timedatectl")
            .args(["set-ntp", "false"])
            .output()
        {
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
                warn!(
                    "Failed to set realtime priority: {}. Latency might suffer.",
                    err
                );
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
        let lock_path = "/var/run/dantesync.lock";
        let file = File::create(lock_path)
            .map_err(|e| anyhow!("Failed to create lock file {}: {}", lock_path, e))?;

        match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
            Ok(_) => Ok(file),
            Err(nix::errno::Errno::EAGAIN) => Err(anyhow!(
                "Another instance of dantesync is already running! (Lockfile: {})",
                lock_path
            )),
            Err(e) => Err(e.into()),
        }
    }
    #[cfg(not(unix))]
    {
        // On Windows, file locking prevents deletion but not necessarily running if logic differs.
        // But File::create opens/truncates.
        // We want shared read, exclusive write?
        // Simple create is fine for now if we hold the handle.
        let file = File::create("dantesync.lock")?;
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

            // Pre-allocate UTF-16 strings outside loop for performance
            let pipe_name_wide: Vec<u16> = r"\\.\pipe\dantesync"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let sddl_wide: Vec<u16> = "D:(A;;GA;;;WD)"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();

            // Named pipe server loop
            loop {
                // Create pipe manually with Security Descriptor to allow Users to connect to Service
                // SDDL: D:(A;;GA;;;WD) -> DACL: Allow Generic All to World (Everyone)
                // This is needed because Service runs as SYSTEM and Tray runs as User.

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
                        None,
                    )
                    .is_ok()
                    {
                        sa.lpSecurityDescriptor = sd.0;

                        let h = CreateNamedPipeW(
                            PCWSTR(pipe_name_wide.as_ptr()),
                            FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_OUTBOUND | FILE_FLAG_OVERLAPPED),
                            NAMED_PIPE_MODE(0), // Byte mode (0) for Tokio compatibility
                            PIPE_UNLIMITED_INSTANCES,
                            1024,
                            1024,
                            0,
                            Some(&sa),
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
                    // Handle poisoned lock gracefully instead of panicking
                    let s = match status.read() {
                        Ok(guard) => guard.clone(),
                        Err(e) => {
                            error!("Status lock poisoned: {}. Skipping IPC write.", e);
                            continue;
                        }
                    };
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
fn run_sync_loop(args: Args, running: Arc<AtomicBool>, system_config: SystemConfig) -> Result<()> {
    // Notify systemd (Linux) that we are starting
    #[cfg(unix)]
    {
        let _ = sd_notify::notify(
            false,
            &[sd_notify::NotifyState::Status(
                format!("v{} | Starting...", env!("CARGO_PKG_VERSION")).as_str(),
            )],
        );
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

    // Platform-specific network setup
    #[cfg(unix)]
    let network = {
        // Create sockets to join multicast groups (IGMP) with kernel timestamping
        let sock_event = net::create_multicast_socket(ptp::PTP_EVENT_PORT, iface_ip)?;
        let sock_general = net::create_multicast_socket(ptp::PTP_GENERAL_PORT, iface_ip)?;
        info!(
            "Joined Multicast Groups on {} ({}) - Kernel timestamping",
            iface_name, iface_ip
        );

        RealPtpNetwork {
            sock_event,
            sock_general,
        }
    };

    #[cfg(windows)]
    let network = {
        // Use Npcap with HostHighPrec timestamps (KeQuerySystemTimePrecise)
        // This provides driver-level timestamps that are both precise AND synced with system time
        match net_pcap::NpcapPtpNetwork::new(&iface_name) {
            Ok(npcap_net) => {
                info!(
                    "Using Npcap HostHighPrec timestamps on {} ({})",
                    iface_name, iface_ip
                );
                npcap_net
            }
            Err(e) => {
                error!(
                    "Failed to initialize Npcap: {}. Npcap is required on Windows.",
                    e
                );
                return Err(e);
            }
        }
    };

    let ntp_source = RealNtpSource {
        client: ntp::NtpClient::new(&args.ntp_server),
    };

    let mut controller =
        PtpController::new(sys_clock, network, ntp_source, status_shared, system_config);

    if !args.skip_ntp {
        info!("Using NTP Server: {}", args.ntp_server);
    }
    controller.run_ntp_sync(args.skip_ntp);

    info!("Starting PTP Loop...");

    // Notify systemd we are ready and loop is running
    #[cfg(unix)]
    {
        let _ = sd_notify::notify(
            false,
            &[
                sd_notify::NotifyState::Ready,
                sd_notify::NotifyState::Status(
                    format!("v{} | PTP Loop Running", env!("CARGO_PKG_VERSION")).as_str(),
                ),
            ],
        );
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
                        format!(
                            "v{} | Locked | Offset: {:.3} µs",
                            env!("CARGO_PKG_VERSION"),
                            status.offset_ns as f64 / 1000.0
                        )
                    } else {
                        format!("v{} | Settling...", env!("CARGO_PKG_VERSION"))
                    };
                    let _ =
                        sd_notify::notify(false, &[sd_notify::NotifyState::Status(&status_str)]);
                };
            }

            last_log = Instant::now();
        }

        if let Err(e) = controller.process_loop_iteration() {
            warn!("Error in loop: {}", e);
        }

        // On Windows, use tight polling for lower jitter (50µs = ~5% CPU)
        // On Linux, 1ms is fine since we use kernel timestamps
        #[cfg(windows)]
        thread::sleep(Duration::from_micros(50));
        #[cfg(not(windows))]
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
const SERVICE_NAME: &str = "dantesync";

#[cfg(windows)]
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

#[cfg(windows)]
fn my_service_main(_arguments: Vec<OsString>) {
    // We need to reload config or pass it?
    // Windows Service entry doesn't allow easy closure capture without unsafe global.
    // But we can just reload it, it's cheap.
    let config = load_config();

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
        if let Err(e) = run_sync_loop(args, r, config.system) {
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

// Generate FFI wrapper for Windows service entry point
#[cfg(windows)]
define_windows_service!(ffi_service_main, my_service_main);

#[cfg(windows)]
fn run_service_logic(_args: Args, _config: Config) -> Result<()> {
    info!("Service logic starting...");
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)?;
    Ok(())
}

fn main() -> Result<()> {
    let mut args = Args::parse();
    let config = load_config();

    // Use config if arg is default
    if args.ntp_server == "10.77.8.2" {
        args.ntp_server = config.ntp_server.clone();
    }

    #[cfg(windows)]
    if args.service {
        // Initialize File Logging for Service
        let log_path = r"C:\ProgramData\DanteSync\dantesync.log";

        // Log Rotation (Simple: 1MB limit check on startup)
        if let Ok(metadata) = std::fs::metadata(log_path) {
            if metadata.len() > 1_000_000 {
                let old_path = format!("{}.old", log_path);
                let _ = std::fs::rename(log_path, old_path);
            }
        }

        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .write(true)
            .open(log_path)
        {
            let target = env_logger::Target::Pipe(Box::new(file));
            env_logger::builder()
                .target(target)
                .filter_level(log::LevelFilter::Info)
                .format_timestamp_millis()
                .format_target(false) // Remove module path from logs
                .format_level(false) // Remove INFO/WARN prefix
                .init();
        } else {
            // Fallback
            env_logger::builder()
                .filter_level(log::LevelFilter::Info)
                .init();
        }

        info!("Service Started: v{}", env!("CARGO_PKG_VERSION"));
        return run_service_logic(args, config);
    }

    // Console Mode Logging (clean format)
    env_logger::builder()
        .format_timestamp(None)
        .format_target(false) // Remove module path
        .format_level(false) // Remove INFO/WARN prefix
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

    run_sync_loop(args, running, config.system)
}
