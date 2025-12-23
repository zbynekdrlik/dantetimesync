//! Windows clock control using SetSystemTimeAdjustmentPrecise (64-bit Precise API).
//!
//! This module includes comprehensive diagnostics to verify that frequency
//! adjustment actually affects clock speed.

use super::SystemClock;
use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{BOOL, HANDLE, LUID, CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, SYSTEMTIME, FILETIME};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
    TOKEN_PRIVILEGES, SE_PRIVILEGE_ENABLED
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAdjustmentPrecise, SetSystemTimeAdjustmentPrecise,
    GetSystemTimeAsFileTime, SetSystemTime
};
use windows::Win32::System::Performance::{QueryPerformanceFrequency, QueryPerformanceCounter};
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::core::PCWSTR;
use std::time::{Duration, Instant};
use log::{info, warn, debug, error};

pub struct WindowsClock {
    original_increment: u64,
    perf_frequency: i64,

    // Diagnostic tracking
    adjustment_count: u64,
    last_adjustment: u64,
    last_requested_ppm: f64,

    // High-precision measurement baseline (for diagnostics)
    baseline_perf_counter: i64,
    baseline_filetime: u64,
    last_measurement_time: Instant,
}

impl WindowsClock {
    pub fn new() -> Result<Self> {
        Self::enable_privilege("SeSystemtimePrivilege")?;

        // Get performance counter frequency
        let mut perf_freq: i64 = 0;
        unsafe {
            QueryPerformanceFrequency(&mut perf_freq)?;
        }

        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);

        unsafe {
            GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled)?;
        }

        info!("╔══════════════════════════════════════════════════════════════════╗");
        info!("║           WINDOWS CLOCK INITIALIZATION                           ║");
        info!("╠══════════════════════════════════════════════════════════════════╣");
        info!("║ API: SetSystemTimeAdjustmentPrecise (64-bit)                     ║");
        info!("╚══════════════════════════════════════════════════════════════════╝");

        info!("Performance Counter Frequency: {} Hz ({:.3} MHz)", perf_freq, perf_freq as f64 / 1_000_000.0);
        info!("Initial Adjustment: {} (Increment: {}, Disabled: {})", adj, inc, disabled.as_bool());

        // Calculate PPM sensitivity
        let ppm_per_unit = 1_000_000.0 / inc as f64;
        info!("Sensitivity: 1 adjustment unit = {:.6} PPM", ppm_per_unit);

        // Current PPM offset from nominal
        let current_ppm = ((adj as f64 - inc as f64) / inc as f64) * 1_000_000.0;
        info!("Current PPM offset: {:+.3} PPM (Adj {} vs Nominal {})", current_ppm, adj, inc);

        // Enable adjustment if disabled
        if disabled.as_bool() {
            warn!("Time adjustment was DISABLED! Enabling...");
            unsafe {
                SetSystemTimeAdjustmentPrecise(inc, false)?;
            }
            info!("Time adjustment ENABLED with nominal value.");
        }

        // Get baseline measurements
        let (baseline_pc, baseline_ft) = unsafe {
            let mut pc: i64 = 0;
            QueryPerformanceCounter(&mut pc)?;
            let ft = GetSystemTimeAsFileTime();
            let ft_u64 = (ft.dwHighDateTime as u64) << 32 | (ft.dwLowDateTime as u64);
            (pc, ft_u64)
        };

        let clock = WindowsClock {
            original_increment: inc,
            perf_frequency: perf_freq,
            adjustment_count: 0,
            last_adjustment: inc,
            last_requested_ppm: 0.0,
            baseline_perf_counter: baseline_pc,
            baseline_filetime: baseline_ft,
            last_measurement_time: Instant::now(),
        };

        // Check for interfering processes
        clock.check_for_interference();

        info!("Frequency adjustment API initialized (inverted sign correction applied).");

        Ok(clock)
    }

    /// Check for processes that might interfere with time adjustment
    fn check_for_interference(&self) {
        info!("");
        info!("Checking for interfering processes...");

        // Check if W32Time service is running
        let w32time_check = std::process::Command::new("sc")
            .args(["query", "w32time"])
            .output();

        match w32time_check {
            Ok(output) => {
                let output_str = String::from_utf8_lossy(&output.stdout);
                if output_str.contains("RUNNING") {
                    error!("⚠ W32Time service is RUNNING! This will interfere with time adjustment.");
                    error!("  Run: net stop w32time");
                } else if output_str.contains("STOPPED") {
                    info!("✓ W32Time service is stopped.");
                } else {
                    info!("  W32Time status: {}", output_str.lines().next().unwrap_or("unknown"));
                }
            }
            Err(e) => {
                warn!("Could not check W32Time status: {}", e);
            }
        }

        // Check current adjustment state
        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);
        unsafe {
            if GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled).is_ok() {
                if disabled.as_bool() {
                    error!("⚠ Time adjustment is DISABLED! Another process may have disabled it.");
                } else {
                    info!("✓ Time adjustment is enabled.");
                }
            }
        }
        info!("");
    }

    fn enable_privilege(name: &str) -> Result<()> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)?;

            let mut luid = LUID::default();
            let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            LookupPrivilegeValueW(PCWSTR::null(), PCWSTR(name_wide.as_ptr()), &mut luid)?;

            let mut tp = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                ..Default::default()
            };
            tp.Privileges[0].Luid = luid;
            tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED;

            AdjustTokenPrivileges(token, BOOL(0), Some(&tp), 0, None, None)?;

            if let Err(e) = GetLastError() {
                if e.code() == ERROR_NOT_ALL_ASSIGNED.to_hresult() {
                     return Err(anyhow!("Failed to adjust privilege: ERROR_NOT_ALL_ASSIGNED. Run as Administrator!"));
                }
            }

            CloseHandle(token)?;
        }
        Ok(())
    }

    /// Measure current clock rate vs wall clock and log detailed diagnostics
    fn measure_and_log_effectiveness(&mut self, _requested_ppm: f64) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_measurement_time);

        // Only measure if enough time has passed (at least 1 second for accuracy)
        if elapsed.as_secs() < 1 {
            return;
        }

        unsafe {
            // Get current measurements
            let mut current_pc: i64 = 0;
            if QueryPerformanceCounter(&mut current_pc).is_err() {
                return;
            }

            let current_ft = GetSystemTimeAsFileTime();
            let current_ft_u64 = (current_ft.dwHighDateTime as u64) << 32 | (current_ft.dwLowDateTime as u64);

            // Calculate elapsed times
            let pc_elapsed = current_pc - self.baseline_perf_counter;
            let wall_time_ns = (pc_elapsed as f64 / self.perf_frequency as f64) * 1_000_000_000.0;

            let ft_elapsed = current_ft_u64 - self.baseline_filetime;
            let system_time_ns = ft_elapsed as f64 * 100.0;

            // Calculate observed PPM since baseline
            let time_diff_ns = system_time_ns - wall_time_ns;
            let observed_ppm = (time_diff_ns / wall_time_ns) * 1_000_000.0;

            // Calculate effectiveness
            let effectiveness = if self.last_requested_ppm.abs() > 0.1 {
                observed_ppm / self.last_requested_ppm
            } else {
                if observed_ppm.abs() < 10.0 { 1.0 } else { 0.0 }
            };

            // Log every 10 seconds worth of measurements (debug level - matches Linux simplicity)
            if elapsed.as_secs() >= 10 {
                debug!("[FreqMeasure] Elapsed: {:.1}s | Requested: {:+.1} PPM | Observed: {:+.1} PPM | Effectiveness: {:.0}%",
                      wall_time_ns / 1_000_000_000.0, self.last_requested_ppm, observed_ppm, effectiveness * 100.0);

                if effectiveness.abs() < 0.3 && self.last_requested_ppm.abs() > 10.0 {
                    debug!("[FreqMeasure] LOW EFFECTIVENESS! Frequency adjustment may not be working.");
                }

                // Reset baseline for next measurement period
                self.baseline_perf_counter = current_pc;
                self.baseline_filetime = current_ft_u64;
                self.last_measurement_time = now;
            }
        }
    }
}

impl SystemClock for WindowsClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        let ppm = (factor - 1.0) * 1_000_000.0;

        // Calculate adjustment delta
        // NOTE: Windows has INVERTED behavior - increasing adjustment slows the clock!
        // To speed up (positive PPM), we must DECREASE the adjustment value.
        let adjustment_delta = (-ppm * self.perf_frequency as f64 / 1_000_000.0).round() as i64;
        let new_adj = (self.original_increment as i64 + adjustment_delta) as u64;

        self.adjustment_count += 1;

        // Calculate delta for logging
        let delta_from_nominal = new_adj as i64 - self.original_increment as i64;

        // Log adjustments at debug level (matches Linux simplicity)
        debug!("[FreqAdj #{}] {:+.3} PPM | Adj: {} → {} (Δ{:+} from nominal)",
               self.adjustment_count, ppm, self.last_adjustment, new_adj, delta_from_nominal);

        unsafe {
            // Apply adjustment
            SetSystemTimeAdjustmentPrecise(new_adj, false)?;

            // Verify
            let mut verify_adj = 0u64;
            let mut verify_inc = 0u64;
            let mut verify_disabled = BOOL(0);

            if GetSystemTimeAdjustmentPrecise(&mut verify_adj, &mut verify_inc, &mut verify_disabled).is_ok() {
                if verify_adj != new_adj {
                    error!("[FreqAdj] MISMATCH! Requested={}, Actual={}", new_adj, verify_adj);
                }
                if verify_disabled.as_bool() {
                    error!("[FreqAdj] TIME ADJUSTMENT DISABLED! Interference detected!");
                    // Try to re-enable
                    let _ = SetSystemTimeAdjustmentPrecise(new_adj, false);
                }
            }
        }

        self.last_adjustment = new_adj;
        self.last_requested_ppm = ppm;

        // Periodic effectiveness measurement
        self.measure_and_log_effectiveness(ppm);

        Ok(())
    }

    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        let sign_str = if sign > 0 { "+" } else { "-" };
        info!("[StepClock] Stepping by {}{:.3}ms", sign_str, offset.as_secs_f64() * 1000.0);

        unsafe {
            let ft = GetSystemTimeAsFileTime();
            let before_u64 = (ft.dwHighDateTime as u64) << 32 | (ft.dwLowDateTime as u64);

            let mut u64_time = before_u64;
            let offset_100ns = offset.as_nanos() as u64 / 100;

            if sign > 0 {
                u64_time += offset_100ns;
            } else {
                if u64_time > offset_100ns {
                    u64_time -= offset_100ns;
                } else {
                    return Err(anyhow!("Clock step would result in negative time"));
                }
            }

            let ft_new = FILETIME {
                dwLowDateTime: (u64_time & 0xFFFFFFFF) as u32,
                dwHighDateTime: (u64_time >> 32) as u32,
            };

            let mut st = SYSTEMTIME::default();
            FileTimeToSystemTime(&ft_new, &mut st)?;
            SetSystemTime(&st)?;

            // Verify
            let ft_after = GetSystemTimeAsFileTime();
            let after_u64 = (ft_after.dwHighDateTime as u64) << 32 | (ft_after.dwLowDateTime as u64);
            let actual_step = after_u64 as i64 - before_u64 as i64;
            let expected_step = if sign > 0 { offset_100ns as i64 } else { -(offset_100ns as i64) };

            info!("[StepClock] Actual step: {} (expected: {})", actual_step, expected_step);

            // Reset measurement baseline after step
            let mut pc: i64 = 0;
            let _ = QueryPerformanceCounter(&mut pc);
            self.baseline_perf_counter = pc;
            self.baseline_filetime = after_u64;
            self.last_measurement_time = Instant::now();
        }

        Ok(())
    }
}

impl Drop for WindowsClock {
    fn drop(&mut self) {
        info!("╔══════════════════════════════════════════════════════════════════╗");
        info!("║           WINDOWS CLOCK SHUTDOWN                                 ║");
        info!("╚══════════════════════════════════════════════════════════════════╝");
        info!("Total frequency adjustments: {}", self.adjustment_count);
        info!("Resetting to nominal: {}", self.original_increment);

        unsafe {
            match SetSystemTimeAdjustmentPrecise(self.original_increment, false) {
                Ok(_) => info!("Clock reset to nominal successfully."),
                Err(e) => error!("Failed to reset clock: {}", e),
            }
        }
    }
}
