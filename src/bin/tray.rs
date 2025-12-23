#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
fn main() {
    println!("This utility is for Windows only.");
}

#[cfg(windows)]
mod app {
    use tray_icon::{TrayIconBuilder, menu::{Menu, MenuItem, MenuEvent}, Icon};
    use winit::event_loop::{ControlFlow, EventLoopBuilder};
    use winit::event::Event;
    use tokio::net::windows::named_pipe::ClientOptions;
    use tokio::io::AsyncReadExt;
    use serde::Deserialize;
    use std::time::Duration;
    use std::cell::RefCell;
    use winrt_notification::{Toast, Sound};

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
    }

    #[derive(Debug)]
    enum AppEvent {
        Update(SyncStatus),
        Offline,
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

    /// Track previous state for detecting transitions
    #[derive(Default)]
    struct NotificationState {
        was_locked: bool,
        was_online: bool,
        first_update: bool,
    }

    // ========================================================================
    // ICON GENERATION - Dynamic with pulsing ring support
    // ========================================================================

    /// Generate an icon with optional pulsing ring intensity
    /// pulse_intensity: 0.0 = no ring, 1.0 = full ring (based on drift rate)
    fn generate_icon_with_ring(r: u8, g: u8, b: u8, pulse_intensity: f32) -> Icon {
        let width = 32;
        let height = 32;
        let mut rgba = Vec::with_capacity((width * height * 4) as usize);

        let cx = 16.0;
        let cy = 16.0;
        let inner_radius = 11.0;
        let outer_radius = 15.0;
        let ring_width = outer_radius - inner_radius;

        for y in 0..height {
            for x in 0..width {
                let dx = x as f32 - cx + 0.5;
                let dy = y as f32 - cy + 0.5;
                let dist = (dx*dx + dy*dy).sqrt();

                if dist <= inner_radius {
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

    /// Generate static icon (no ring)
    fn generate_icon(r: u8, g: u8, b: u8) -> Icon {
        generate_icon_with_ring(r, g, b, 0.0)
    }

    // ========================================================================
    // MAIN APPLICATION
    // ========================================================================

    pub fn main() {
        let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build().unwrap();
        let proxy = event_loop.create_proxy();

        // ====================================================================
        // MENU ITEMS
        // ====================================================================

        let status_i = MenuItem::new("Status: Connecting...", false, None);
        let mode_i = MenuItem::new("Mode: --", false, None);

        // Service control
        let restart_i = MenuItem::new("Restart Service", true, None);
        let stop_i = MenuItem::new("Stop Service", true, None);

        // Utilities
        let log_i = MenuItem::new("Open Log File", true, None);
        let live_log_i = MenuItem::new("View Live Log", true, None);
        let config_i = MenuItem::new("Edit Configuration", true, None);

        let quit_i = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        menu.append(&status_i).unwrap();
        menu.append(&mode_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator()).unwrap();
        menu.append(&restart_i).unwrap();
        menu.append(&stop_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator()).unwrap();
        menu.append(&log_i).unwrap();
        menu.append(&live_log_i).unwrap();
        menu.append(&config_i).unwrap();
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator()).unwrap();
        menu.append(&quit_i).unwrap();

        // Colors (Flat UI / Bootstrap-style)
        let red_icon = generate_icon(220, 53, 69);    // Danger Red - Offline
        let green_icon = generate_icon(40, 167, 69);  // Success Green - Locked
        let yellow_icon = generate_icon(255, 193, 7); // Warning Yellow - Acquiring

        let tray_icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("Dante Time Sync - Connecting...")
            .with_icon(yellow_icon.clone())
            .build()
            .unwrap();

        // Spawn poller thread... (kept same)
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
                                if client.read_exact(&mut len_buf).await.is_err() { break; }
                                let len = u32::from_le_bytes(len_buf) as usize;
                                let mut buf = vec![0u8; len];
                                if client.read_exact(&mut buf).await.is_err() { break; }
                                
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

        let menu_channel = MenuEvent::receiver();
        let version = env!("CARGO_PKG_VERSION");

        // Track state for notifications
        let notification_state = RefCell::new(NotificationState {
            was_locked: false,
            was_online: false,
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

                                // Check for lock state changes (skip first update)
                                if !state.first_update {
                                    if status.is_locked && !state.was_locked {
                                        show_notification(
                                            "Dante Time Sync",
                                            "Frequency locked - sync achieved"
                                        );
                                    } else if !status.is_locked && state.was_locked {
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
                                state.was_online = true;
                                state.first_update = false;
                            }

                            // ================================================
                            // ICON SELECTION - Based on lock state and drift
                            // ================================================

                            // Calculate pulse intensity from drift rate (0-1 range)
                            // Higher drift rate = more visible ring
                            let pulse_intensity = (status.smoothed_rate_ppm.abs() / 20.0).min(1.0) as f32;

                            let icon = if status.is_locked {
                                // Locked: Green with optional ring if there's drift
                                if pulse_intensity > 0.1 {
                                    generate_icon_with_ring(40, 167, 69, pulse_intensity)
                                } else {
                                    green_icon.clone()
                                }
                            } else if status.settled {
                                // Settled but not locked: Yellow with ring
                                generate_icon_with_ring(255, 193, 7, pulse_intensity.max(0.3))
                            } else {
                                // Not settled: Yellow (connecting)
                                yellow_icon.clone()
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

                            let _ = tray_icon.set_icon(Some(icon));
                            let _ = tray_icon.set_tooltip(Some(tooltip));
                            status_i.set_text(status_text);
                            mode_i.set_text(mode_text);
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

                            let _ = tray_icon.set_icon(Some(red_icon.clone()));
                            let _ = tray_icon.set_tooltip(Some(format!("Dante Time Sync v{}\nService Offline", version)));
                            status_i.set_text("Service Offline".to_string());
                            mode_i.set_text("--".to_string());
                        }
                    }
                }
                _ => {
                    if let Ok(event) = menu_channel.try_recv() {
                        if event.id == quit_i.id() {
                            elwt.exit();
                        } else if event.id == restart_i.id() {
                            // Restart service using PowerShell (requires elevation)
                            let _ = std::process::Command::new("powershell.exe")
                                .args(["-Command", "Start-Process powershell -Verb RunAs -ArgumentList '-Command','Restart-Service \"Dante Time Sync\" -Force'"])
                                .spawn();
                        } else if event.id == stop_i.id() {
                            // Stop service
                            let _ = std::process::Command::new("powershell.exe")
                                .args(["-Command", "Start-Process powershell -Verb RunAs -ArgumentList '-Command','Stop-Service \"Dante Time Sync\" -Force'"])
                                .spawn();
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