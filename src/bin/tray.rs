#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    println!("This utility is for Windows only.");
}

#[cfg(windows)]
mod app {
    use serde::Deserialize;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::windows::named_pipe::ClientOptions;
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem},
        Icon, TrayIconBuilder,
    };
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows::Win32::System::Threading::CreateMutexW;
    use winit::event::Event;
    use winit::event_loop::{ControlFlow, EventLoopBuilder};
    use winrt_notification::{Sound, Toast};

    // ========================================================================
    // SINGLE INSTANCE CHECK - Prevent multiple tray apps
    // ========================================================================

    struct SingleInstanceGuard {
        _handle: HANDLE,
    }

    impl SingleInstanceGuard {
        /// Try to acquire single-instance lock. Returns None if another instance is running.
        fn try_acquire() -> Option<Self> {
            unsafe {
                let mutex_name: Vec<u16> = "Global\\DanteTrayMutex\0".encode_utf16().collect();
                let handle = CreateMutexW(None, false, PCWSTR(mutex_name.as_ptr()));

                match handle {
                    Ok(h) => {
                        // Check if mutex already existed
                        if let Err(e) = GetLastError() {
                            if e.code() == ERROR_ALREADY_EXISTS.to_hresult() {
                                // Another instance is running - close handle and return None
                                let _ = CloseHandle(h);
                                return None;
                            }
                        }
                        Some(SingleInstanceGuard { _handle: h })
                    }
                    Err(_) => None,
                }
            }
        }
    }

    impl Drop for SingleInstanceGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self._handle);
            }
        }
    }

    // ========================================================================
    // SYNC STATUS - Extended struct matching service
    // ========================================================================

    #[derive(Deserialize, Debug, Clone, Default)]
    struct SyncStatus {
        // Core fields
        pub offset_ns: i64,
        pub drift_ppm: f64,
        #[serde(rename = "gm_uuid")]
        pub _gm_uuid: Option<[u8; 6]>,
        pub settled: bool,
        #[serde(rename = "updated_ts")]
        pub _updated_ts: u64,

        // Extended fields for tray app
        #[serde(default)]
        pub is_locked: bool,
        #[serde(default)]
        pub smoothed_rate_ppm: f64,
        #[serde(default)]
        pub ntp_offset_us: i64,
        #[serde(default)]
        pub mode: String,
        #[serde(default)]
        pub ntp_failed: bool,
    }

    // ========================================================================
    // GITHUB RELEASE - For version check
    // ========================================================================

    #[derive(Deserialize, Debug)]
    struct GitHubRelease {
        tag_name: String,
    }

    /// Parse version string (e.g., "v1.6.4" or "1.6.4") into comparable tuple
    fn parse_version(version: &str) -> Option<(u32, u32, u32)> {
        let v = version.trim_start_matches('v');
        let parts: Vec<&str> = v.split('.').collect();
        if parts.len() >= 3 {
            Some((
                parts[0].parse().ok()?,
                parts[1].parse().ok()?,
                parts[2].parse().ok()?,
            ))
        } else {
            None
        }
    }

    /// Compare versions, returns true if remote is newer than local
    fn is_newer_version(local: &str, remote: &str) -> bool {
        match (parse_version(local), parse_version(remote)) {
            (Some(l), Some(r)) => r > l,
            _ => false,
        }
    }

    #[derive(Debug)]
    enum AppEvent {
        Update(SyncStatus),
        Offline,
        NewVersionAvailable(String),
    }

    // ========================================================================
    // TOAST NOTIFICATIONS
    // ========================================================================

    /// Show a Windows toast notification
    fn show_notification(title: &str, message: &str) {
        let _ = Toast::new(Toast::POWERSHELL_APP_ID)
            .title(title)
            .text1(message)
            .sound(Some(Sound::Default))
            .show();
    }

    // ========================================================================
    // VERSION CHECK - GitHub API
    // ========================================================================

    const GITHUB_API_URL: &str =
        "https://api.github.com/repos/zbynekdrlik/dantetimesync/releases/latest";

    /// Fetch the latest version from GitHub releases
    async fn check_latest_version() -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let client = reqwest::Client::builder()
            .user_agent("DanteTimeSync-Tray")
            .timeout(Duration::from_secs(10))
            .build()?;

        let response = client.get(GITHUB_API_URL).send().await?;
        let release: GitHubRelease = response.json().await?;
        Ok(release.tag_name)
    }

    /// Track previous state for detecting transitions
    #[derive(Default)]
    struct NotificationState {
        was_locked: bool,
        was_nano: bool,
        was_online: bool,
        was_ptp_offline: bool,
        was_ntp_failed: bool,
        first_update: bool,
    }

    // ========================================================================
    // ICON GENERATION - Dynamic with pulsing ring and update badge support
    // ========================================================================

    /// Generate an icon with optional pulsing ring and update badge
    /// pulse_intensity: 0.0 = no ring, 1.0 = full ring (based on drift rate)
    /// show_update_badge: if true, shows orange dot in top-right corner
    fn generate_icon_full(
        r: u8,
        g: u8,
        b: u8,
        pulse_intensity: f32,
        show_update_badge: bool,
    ) -> Icon {
        let width = 32;
        let height = 32;
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);

        let cx = 16.0;
        let cy = 16.0;
        let inner_radius = 11.0;
        let outer_radius = 15.0;
        let ring_width = outer_radius - inner_radius;

        // Update badge: small orange dot in top-right corner
        let badge_cx = 25.0;
        let badge_cy = 7.0;
        let badge_radius = 5.0;

        for y in 0..height {
            for x in 0..width {
                let dx = x as f32 - cx + 0.5;
                let dy = y as f32 - cy + 0.5;
                let dist = (dx * dx + dy * dy).sqrt();

                // Check if pixel is in update badge area (takes priority)
                let badge_dx = x as f32 - badge_cx + 0.5;
                let badge_dy = y as f32 - badge_cy + 0.5;
                let badge_dist = (badge_dx * badge_dx + badge_dy * badge_dy).sqrt();

                if show_update_badge && badge_dist <= badge_radius {
                    // Update badge - orange/amber color with anti-aliasing
                    let mut alpha = 255u8;
                    if badge_dist > badge_radius - 1.0 {
                        alpha = ((badge_radius - badge_dist) * 255.0).max(0.0) as u8;
                    }
                    rgba.push(255); // R - orange
                    rgba.push(152); // G
                    rgba.push(0); // B
                    rgba.push(alpha);
                } else if dist <= inner_radius {
                    // Main fill - solid color
                    let mut alpha = 255u8;
                    if dist > inner_radius - 1.0 {
                        alpha = ((inner_radius - dist) * 255.0).max(0.0) as u8;
                    }
                    rgba.push(r);
                    rgba.push(g);
                    rgba.push(b);
                    rgba.push(alpha);
                } else if dist <= outer_radius && pulse_intensity > 0.01 {
                    // Pulsing ring - intensity based on drift rate
                    let ring_pos = (dist - inner_radius) / ring_width;
                    // Fade out towards edge
                    let ring_alpha = ((1.0 - ring_pos) * pulse_intensity * 180.0) as u8;
                    // Ring color is brighter version of main color
                    let ring_r = (r as f32 * 1.3).min(255.0) as u8;
                    let ring_g = (g as f32 * 1.3).min(255.0) as u8;
                    let ring_b = (b as f32 * 1.3).min(255.0) as u8;
                    rgba.push(ring_r);
                    rgba.push(ring_g);
                    rgba.push(ring_b);
                    rgba.push(ring_alpha);
                } else {
                    // Transparent
                    rgba.push(0);
                    rgba.push(0);
                    rgba.push(0);
                    rgba.push(0);
                }
            }
        }
        Icon::from_rgba(rgba, width, height).unwrap()
    }

    /// Generate icon with optional ring (no badge)
    fn generate_icon_with_ring(r: u8, g: u8, b: u8, pulse_intensity: f32) -> Icon {
        generate_icon_full(r, g, b, pulse_intensity, false)
    }

    /// Generate static icon (no ring, no badge)
    fn generate_icon(r: u8, g: u8, b: u8) -> Icon {
        generate_icon_full(r, g, b, 0.0, false)
    }

    // ========================================================================
    // MAIN APPLICATION
    // ========================================================================

    pub fn main() {
        // Single-instance check - exit silently if another instance is running
        let _guard = match SingleInstanceGuard::try_acquire() {
            Some(guard) => guard,
            None => {
                // Another instance is already running - exit silently
                return;
            }
        };

        let event_loop = EventLoopBuilder::<AppEvent>::with_user_event()
            .build()
            .unwrap();
        let proxy = event_loop.create_proxy();

        // ====================================================================
        // MENU ITEMS
        // ====================================================================

        let status_i = MenuItem::new("Status: Connecting...", false, None);
        let mode_i = MenuItem::new("Mode: --", false, None);

        // Service control
        let restart_i = MenuItem::new("Restart Service", true, None);
        let start_stop_i = MenuItem::new("Stop Service", true, None);

        // Utilities
        let log_i = MenuItem::new("Open Log File", true, None);
        let live_log_i = MenuItem::new("View Live Log", true, None);
        let config_i = MenuItem::new("Edit Configuration", true, None);

        // Upgrade - disabled until new version detected
        let upgrade_i = MenuItem::new("Check for Updates...", true, None);

        let quit_i = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        menu.append(&status_i).unwrap();
        menu.append(&mode_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .unwrap();
        menu.append(&restart_i).unwrap();
        menu.append(&start_stop_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .unwrap();
        menu.append(&log_i).unwrap();
        menu.append(&live_log_i).unwrap();
        menu.append(&config_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .unwrap();
        menu.append(&upgrade_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .unwrap();
        menu.append(&quit_i).unwrap();

        // Colors (Flat UI / Bootstrap-style)
        let red_icon = generate_icon(220, 53, 69); // Danger Red - Offline
        let green_icon = generate_icon(40, 167, 69); // Success Green - Locked
        let yellow_icon = generate_icon(255, 193, 7); // Warning Yellow - Acquiring
        let orange_icon = generate_icon(255, 152, 0); // Orange - NTP-only mode (PTP offline)
        let cyan_icon = generate_icon(0, 188, 212); // Cyan - NANO mode (ultra-precise)

        // Wrap in RefCell so we can explicitly drop it on exit to clean up tray icon
        let tray_icon = RefCell::new(Some(
            TrayIconBuilder::new()
                .with_menu(Box::new(menu.clone()))
                .with_tooltip("Dante Time Sync - Connecting...")
                .with_icon(yellow_icon.clone())
                .build()
                .unwrap(),
        ));

        // Spawn status poller thread
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move {
                loop {
                    // Server pipe is outbound-only (Server -> Client).
                    // We must open as read-only, otherwise we get ACCESS_DENIED.
                    match ClientOptions::new()
                        .write(false)
                        .read(true)
                        .open(r"\\.\pipe\dantetimesync")
                    {
                        Ok(mut client) => {
                            loop {
                                let mut len_buf = [0u8; 4];
                                if client.read_exact(&mut len_buf).await.is_err() {
                                    break;
                                }
                                let len = u32::from_le_bytes(len_buf) as usize;
                                let mut buf = vec![0u8; len];
                                if client.read_exact(&mut buf).await.is_err() {
                                    break;
                                }

                                if let Ok(status) = serde_json::from_slice::<SyncStatus>(&buf) {
                                    let _ = proxy.send_event(AppEvent::Update(status));
                                }
                            }
                            // Connection closed by server (one-shot). Sleep before reconnecting to prevent UI freeze.
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                        Err(_) => {
                            let _ = proxy.send_event(AppEvent::Offline);
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            });
        });

        // ====================================================================
        // VERSION CHECK - Periodic check for updates
        // ====================================================================
        let version_proxy = event_loop.create_proxy();
        let current_version = env!("CARGO_PKG_VERSION").to_string();
        let update_available = Arc::new(AtomicBool::new(false));
        let update_available_clone = update_available.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            rt.block_on(async move {
                // Initial delay before first check (10 seconds after startup)
                tokio::time::sleep(Duration::from_secs(10)).await;

                loop {
                    // Check for new version
                    match check_latest_version().await {
                        Ok(latest) => {
                            if is_newer_version(&current_version, &latest) {
                                // Only notify if we haven't already
                                if !update_available_clone.load(Ordering::Relaxed) {
                                    update_available_clone.store(true, Ordering::Relaxed);
                                    let _ = version_proxy
                                        .send_event(AppEvent::NewVersionAvailable(latest));
                                }
                            }
                        }
                        Err(_) => {
                            // Silently ignore version check failures
                        }
                    }

                    // Check every 6 hours
                    tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
                }
            });
        });

        let menu_channel = MenuEvent::receiver();
        let version = env!("CARGO_PKG_VERSION");

        // Track state for notifications
        let notification_state = RefCell::new(NotificationState {
            was_locked: false,
            was_nano: false,
            was_online: false,
            was_ptp_offline: false,
            was_ntp_failed: false,
            first_update: true,
        });

        event_loop.run(move |event, elwt| {
            elwt.set_control_flow(ControlFlow::Wait);

            match event {
                Event::UserEvent(app_event) => {
                    match app_event {
                        AppEvent::Update(status) => {
                            // ================================================
                            // NOTIFICATIONS - Detect state transitions
                            // ================================================
                            {
                                let mut state = notification_state.borrow_mut();

                                let is_nano = status.mode == "NANO";
                                let is_ptp_offline = status.mode == "NTP-only";

                                // Check for state changes (skip first update)
                                if !state.first_update {
                                    // NTP failure transitions (critical)
                                    if status.ntp_failed && !state.was_ntp_failed {
                                        show_notification(
                                            "Dante Time Sync",
                                            "NTP server unreachable"
                                        );
                                    } else if !status.ntp_failed && state.was_ntp_failed {
                                        show_notification(
                                            "Dante Time Sync",
                                            "NTP connection restored"
                                        );
                                    }
                                    // PTP offline transitions (highest priority)
                                    else if is_ptp_offline && !state.was_ptp_offline {
                                        show_notification(
                                            "Dante Time Sync",
                                            "PTP offline - running NTP-only sync"
                                        );
                                    } else if !is_ptp_offline && state.was_ptp_offline {
                                        show_notification(
                                            "Dante Time Sync",
                                            "PTP restored - resuming PTP sync"
                                        );
                                    }
                                    // NANO mode transitions (only when PTP is online)
                                    else if is_nano && !state.was_nano {
                                        show_notification(
                                            "Dante Time Sync",
                                            "NANO mode - ultra-precise sync achieved"
                                        );
                                    } else if !is_nano && state.was_nano && !is_ptp_offline {
                                        show_notification(
                                            "Dante Time Sync",
                                            "Exited NANO mode"
                                        );
                                    }
                                    // Lock state changes (only if not NANO or PTP offline transition)
                                    else if status.is_locked && !state.was_locked && !is_ptp_offline {
                                        show_notification(
                                            "Dante Time Sync",
                                            "Frequency locked - sync achieved"
                                        );
                                    } else if !status.is_locked && state.was_locked && !is_nano && !is_ptp_offline {
                                        show_notification(
                                            "Dante Time Sync",
                                            "Lock lost - reacquiring..."
                                        );
                                    }

                                    // Service came online
                                    if !state.was_online {
                                        show_notification(
                                            "Dante Time Sync",
                                            "Service connected"
                                        );
                                    }
                                }

                                state.was_locked = status.is_locked;
                                state.was_nano = is_nano;
                                state.was_ptp_offline = is_ptp_offline;
                                state.was_ntp_failed = status.ntp_failed;
                                state.was_online = true;
                                state.first_update = false;
                            }

                            // ================================================
                            // ICON SELECTION - Based on mode and drift
                            // ================================================

                            // Calculate pulse intensity from drift rate (0-1 range)
                            // Higher drift rate = more visible ring
                            let pulse_intensity = (status.smoothed_rate_ppm.abs() / 20.0).min(1.0) as f32;

                            // Check if update is available for badge
                            let has_update = update_available.load(Ordering::Relaxed);

                            let is_nano = status.mode == "NANO";
                            let is_ptp_offline = status.mode == "NTP-only";
                            let icon = if is_ptp_offline {
                                // PTP offline: Orange - running NTP-only sync
                                generate_icon_full(255, 152, 0, 0.0, has_update)
                            } else if is_nano {
                                // NANO mode: Cyan - ultra-precise sync
                                let nano_pulse = (status.smoothed_rate_ppm.abs() / 5.0).min(1.0) as f32;
                                generate_icon_full(0, 188, 212, nano_pulse, has_update)
                            } else if status.is_locked {
                                // Locked: Green with optional ring if there's drift
                                generate_icon_full(40, 167, 69, pulse_intensity, has_update)
                            } else if status.settled {
                                // Settled but not locked: Yellow with ring
                                generate_icon_full(255, 193, 7, pulse_intensity.max(0.3), has_update)
                            } else {
                                // Not settled: Yellow (connecting)
                                generate_icon_full(255, 193, 7, 0.0, has_update)
                            };

                            // ================================================
                            // STATUS TEXT
                            // ================================================

                            let mode_str = if status.mode.is_empty() {
                                if status.is_locked { "LOCK" } else { "ACQ" }
                            } else {
                                &status.mode
                            };

                            // Drift rate display (rate of change, not absolute offset)
                            let drift_str = format!("{:+.1}us/s", status.smoothed_rate_ppm);

                            let tooltip = format!(
                                "Dante Time Sync v{}\nMode: {} | Drift: {}\nFreq Adj: {:+.1}ppm\nNTP Offset: {:+}us",
                                version, mode_str, drift_str, status.drift_ppm, status.ntp_offset_us
                            );

                            let status_text = format!("{} | Drift: {}", mode_str, drift_str);
                            let mode_text = format!("Mode: {} | Adj: {:+.1}ppm", mode_str, status.drift_ppm);

                            if let Some(ref ti) = *tray_icon.borrow() {
                                let _ = ti.set_icon(Some(icon));
                                let _ = ti.set_tooltip(Some(tooltip));
                            }
                            status_i.set_text(status_text);
                            mode_i.set_text(mode_text);
                            // Service is running - show Stop option
                            start_stop_i.set_text("Stop Service".to_string());
                            restart_i.set_enabled(true);
                        }
                        AppEvent::Offline => {
                            // Check if we were online before
                            {
                                let mut state = notification_state.borrow_mut();
                                if state.was_online {
                                    show_notification(
                                        "Dante Time Sync",
                                        "Service offline"
                                    );
                                }
                                state.was_online = false;
                                state.was_locked = false;
                            }

                            // Check if update is available for badge
                            let has_update = update_available.load(Ordering::Relaxed);
                            let offline_icon = generate_icon_full(220, 53, 69, 0.0, has_update);

                            if let Some(ref ti) = *tray_icon.borrow() {
                                let _ = ti.set_icon(Some(offline_icon));
                                let _ = ti.set_tooltip(Some(format!("Dante Time Sync v{}\nService Offline", version)));
                            }
                            status_i.set_text("Service Offline".to_string());
                            mode_i.set_text("--".to_string());
                            // Service is stopped - show Start option
                            start_stop_i.set_text("Start Service".to_string());
                            restart_i.set_enabled(false);
                        }
                        AppEvent::NewVersionAvailable(new_version) => {
                            // Update menu item text to show available version
                            upgrade_i.set_text(format!("Upgrade to {}", new_version));

                            // Show notification
                            show_notification(
                                "Dante Time Sync",
                                &format!("New version {} available", new_version)
                            );
                        }
                    }
                }
                _ => {
                    if let Ok(event) = menu_channel.try_recv() {
                        if event.id == quit_i.id() {
                            // Explicitly drop tray icon to clean up system tray
                            tray_icon.borrow_mut().take();
                            elwt.exit();
                        } else if event.id == restart_i.id() {
                            // Restart service using PowerShell (requires elevation)
                            let _ = std::process::Command::new("powershell.exe")
                                .args(["-Command", "Start-Process powershell -Verb RunAs -ArgumentList '-Command','Restart-Service dantetimesync -Force'"])
                                .spawn();
                        } else if event.id == start_stop_i.id() {
                            // Start or Stop service based on current state
                            let is_online = notification_state.borrow().was_online;
                            if is_online {
                                // Service running - stop it
                                let _ = std::process::Command::new("powershell.exe")
                                    .args(["-Command", "Start-Process powershell -Verb RunAs -ArgumentList '-Command','Stop-Service dantetimesync -Force'"])
                                    .spawn();
                            } else {
                                // Service stopped - start it
                                let _ = std::process::Command::new("powershell.exe")
                                    .args(["-Command", "Start-Process powershell -Verb RunAs -ArgumentList '-Command','Start-Service dantetimesync'"])
                                    .spawn();
                            }
                        } else if event.id == log_i.id() {
                            let _ = std::process::Command::new("notepad.exe")
                                .arg(r"C:\ProgramData\DanteTimeSync\dantetimesync.log")
                                .spawn();
                        } else if event.id == live_log_i.id() {
                            let _ = std::process::Command::new("powershell.exe")
                                .args(["-NoExit", "-Command", "Get-Content 'C:\\ProgramData\\DanteTimeSync\\dantetimesync.log' -Tail 20 -Wait"])
                                .spawn();
                        } else if event.id == config_i.id() {
                            let _ = std::process::Command::new("notepad.exe")
                                .arg(r"C:\ProgramData\DanteTimeSync\config.json")
                                .spawn();
                        } else if event.id == upgrade_i.id() {
                            // Run upgrade via PowerShell IRM (Invoke-RestMethod)
                            // This downloads and executes the install script from GitHub
                            let _ = std::process::Command::new("powershell.exe")
                                .args([
                                    "-Command",
                                    "Start-Process powershell -Verb RunAs -ArgumentList '-NoExit','-Command','irm https://raw.githubusercontent.com/zbynekdrlik/dantetimesync/master/install.ps1 | iex'"
                                ])
                                .spawn();
                        }
                    }
                }
            }
        }).unwrap();
    }
}

#[cfg(windows)]
fn main() {
    app::main();
}
