//! Windows clock control using SetSystemTimeAdjustmentPrecise (64-bit Precise API).
//!
//! NOTE: Windows frequency adjustment APIs have a known limitation where they accept
//! values but don't actually change clock speed. Testing confirms both legacy 32-bit
//! and modern 64-bit APIs exhibit this behavior. Time synchronization relies primarily
//! on stepping (SetSystemTime) which works reliably with ~1ms precision.

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
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::core::PCWSTR;
use std::time::{Duration, Instant};
use log::{info, warn, debug, error};

pub struct WindowsClock {
    original_adjustment: u64,
    original_increment: u64,
    original_disabled: BOOL,
    perf_frequency: i64,
    // Diagnostic tracking
    adjustment_count: u64,
    last_adjustment: u64,
    last_adjustment_time: Instant,
    last_time_filetime: u64,
}

impl WindowsClock {
    pub fn new() -> Result<Self> {
        Self::enable_privilege("SeSystemtimePrivilege")?;

        // Get performance counter frequency first
        let mut perf_freq: i64 = 0;
        unsafe {
            QueryPerformanceFrequency(&mut perf_freq)?;
        }

        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);

        unsafe {
            // Use the PRECISE API - 64-bit values for higher precision
            GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled)?;
        }

        // Comprehensive startup diagnostics
        info!("=== Windows Clock Initialization ===");
        info!("API: SetSystemTimeAdjustmentPrecise (64-bit Precise API)");
        info!("Performance Counter Frequency: {} Hz ({:.1} MHz)", perf_freq, perf_freq as f64 / 1_000_000.0);
        info!("Initial State: Adjustment={}, Increment={}, Disabled={}", adj, inc, disabled.as_bool());

        // The Precise API uses performance counter frequency units
        let inc_ms = inc as f64 * 1000.0 / perf_freq as f64;
        let inc_us = inc as f64 * 1_000_000.0 / perf_freq as f64;
        info!("Tick Increment: {:.3} ms ({:.1} us, {} perf counter units)", inc_ms, inc_us, inc);

        // Calculate PPM per adjustment unit
        let ppm_per_unit = 1_000_000.0 / inc as f64;
        info!("Adjustment Sensitivity: 1 unit = {:.6} PPM ({:.8}%)", ppm_per_unit, ppm_per_unit / 10_000.0);

        // Calculate current offset from nominal
        let current_ppm = if adj != inc {
            ((adj as f64 - inc as f64) / inc as f64) * 1_000_000.0
        } else {
            0.0
        };
        info!("Current Offset: Adj={} vs Nominal={}, Delta={}, Current PPM={:.3}",
              adj, inc, adj as i64 - inc as i64, current_ppm);

        // The increment is the nominal frequency (in perf counter units)
        let nominal = inc;
        info!("Nominal Frequency: {} (perf counter units per tick)", nominal);

        // Get current system time for baseline
        let current_ft = unsafe { GetSystemTimeAsFileTime() };
        let current_ft_u64 = (current_ft.dwHighDateTime as u64) << 32 | (current_ft.dwLowDateTime as u64);
        info!("Current FILETIME: {} (100ns units since 1601)", current_ft_u64);

        // If adjustment is currently disabled, enable it with the nominal value
        if disabled.as_bool() {
            info!("Time adjustment is DISABLED. Enabling with nominal value...");
            unsafe {
                SetSystemTimeAdjustmentPrecise(nominal, false)?;
            }
            info!("Time adjustment ENABLED successfully.");

            // Verify it was enabled
            let mut verify_adj = 0u64;
            let mut verify_inc = 0u64;
            let mut verify_disabled = BOOL(0);
            unsafe {
                if GetSystemTimeAdjustmentPrecise(&mut verify_adj, &mut verify_inc, &mut verify_disabled).is_ok() {
                    info!("Verification: Adj={}, Inc={}, Disabled={}",
                          verify_adj, verify_inc, verify_disabled.as_bool());
                }
            }
        } else {
            info!("Time adjustment is already ENABLED.");
        }

        info!("=== Windows Clock Ready ===");

        Ok(WindowsClock {
            original_adjustment: adj,
            original_increment: inc,
            original_disabled: disabled,
            perf_frequency: perf_freq,
            adjustment_count: 0,
            last_adjustment: nominal,
            last_adjustment_time: Instant::now(),
            last_time_filetime: current_ft_u64,
        })
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
                     return Err(anyhow!("Failed to adjust privilege: ERROR_NOT_ALL_ASSIGNED"));
                }
            }

            CloseHandle(token)?;
        }
        Ok(())
    }
}

impl SystemClock for WindowsClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        // factor is the ratio: 1.0 = no change, 1.001 = speed up by 1000 ppm
        // Using Microsoft's formula: adjustment_delta = PPM * PerfFreq / 1_000_000
        // new_adjustment = nominal + adjustment_delta

        let ppm = (factor - 1.0) * 1_000_000.0;

        // Calculate adjustment delta using MS formula
        let adjustment_delta = (ppm * self.perf_frequency as f64 / 1_000_000.0).round() as i64;
        let new_adj = (self.original_increment as i64 + adjustment_delta) as u64;

        // Calculate the delta from nominal in units
        let delta_units = new_adj as i64 - self.original_increment as i64;
        let delta_from_last = new_adj as i64 - self.last_adjustment as i64;

        self.adjustment_count += 1;

        // Measure actual time progress since last adjustment
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_adjustment_time);

        unsafe {
            // Get current FILETIME to measure actual clock progress
            let current_ft = GetSystemTimeAsFileTime();
            let current_ft_u64 = (current_ft.dwHighDateTime as u64) << 32 | (current_ft.dwLowDateTime as u64);

            // Calculate how much the system clock advanced vs wall clock
            let filetime_delta = current_ft_u64.saturating_sub(self.last_time_filetime) as f64;
            let wall_delta_100ns = elapsed.as_nanos() as f64 / 100.0;

            // Ratio of system time progress to wall time progress
            let progress_ratio = if wall_delta_100ns > 0.0 && elapsed.as_millis() > 100 {
                filetime_delta / wall_delta_100ns
            } else {
                1.0
            };

            let observed_ppm = (progress_ratio - 1.0) * 1_000_000.0;

            // Log at INFO level every 10th adjustment, DEBUG otherwise
            let log_detailed = self.adjustment_count % 10 == 1 || delta_from_last.abs() > 10;

            if log_detailed {
                info!("[FreqAdj #{}] PPM={:+.3} | Adj: {}->{} (delta {:+}) | PerfFreq={}",
                      self.adjustment_count, ppm, self.last_adjustment, new_adj, delta_units, self.perf_frequency);

                if elapsed.as_millis() > 100 {
                    info!("  Progress Check: Wall={:.1}ms, FILETIME_delta={:.0}, Ratio={:.6}, Observed_PPM={:+.1}",
                          elapsed.as_secs_f64() * 1000.0, filetime_delta, progress_ratio, observed_ppm);

                    let last_ppm = ((self.last_adjustment as f64 / self.original_increment as f64) - 1.0) * 1_000_000.0;
                    let ppm_error = observed_ppm - last_ppm;
                    if ppm_error.abs() > 100.0 && elapsed.as_secs() > 1 {
                        warn!("  PPM Discrepancy: Expected ~{:.1} PPM, Observed {:.1} PPM, Error={:.1}",
                              last_ppm, observed_ppm, ppm_error);
                    }
                }
            } else {
                debug!("[FreqAdj #{}] PPM={:+.3} Adj={} (delta {:+})",
                       self.adjustment_count, ppm, new_adj, delta_units);
            }

            // Apply the adjustment using the PRECISE API
            SetSystemTimeAdjustmentPrecise(new_adj, false)?;

            // Verify the adjustment was actually applied
            let mut verify_adj = 0u64;
            let mut verify_inc = 0u64;
            let mut verify_disabled = BOOL(0);
            if GetSystemTimeAdjustmentPrecise(&mut verify_adj, &mut verify_inc, &mut verify_disabled).is_ok() {
                if verify_adj != new_adj {
                    error!("ADJUSTMENT MISMATCH! Requested={}, Actual={}, Disabled={}",
                           new_adj, verify_adj, verify_disabled.as_bool());
                } else if log_detailed {
                    debug!("  Verified: Adj={}, Disabled={}", verify_adj, verify_disabled.as_bool());
                }

                if verify_disabled.as_bool() {
                    error!("TIME ADJUSTMENT WAS DISABLED! Another process may be interfering.");
                }
            }

            // Update tracking state
            self.last_adjustment = new_adj;
            self.last_adjustment_time = now;
            self.last_time_filetime = current_ft_u64;
        }
        Ok(())
    }

    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        let sign_str = if sign > 0 { "+" } else { "-" };
        info!("[StepClock] Stepping clock by {}{:.3}ms", sign_str, offset.as_secs_f64() * 1000.0);

        unsafe {
            let ft: FILETIME = GetSystemTimeAsFileTime();
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
            if let Err(e) = FileTimeToSystemTime(&ft_new, &mut st) {
                 error!("[StepClock] FileTimeToSystemTime failed: {}", e);
                 return Err(anyhow!("FileTimeToSystemTime failed: {}", e));
            }
            if let Err(e) = SetSystemTime(&st) {
                 error!("[StepClock] SetSystemTime failed: {}", e);
                 return Err(anyhow!("SetSystemTime failed: {}", e));
            }

            // Verify the step was applied
            let ft_after: FILETIME = GetSystemTimeAsFileTime();
            let after_u64 = (ft_after.dwHighDateTime as u64) << 32 | (ft_after.dwLowDateTime as u64);
            let actual_step = after_u64 as i64 - before_u64 as i64;
            let expected_step = if sign > 0 { offset_100ns as i64 } else { -(offset_100ns as i64) };

            info!("[StepClock] Before={}, After={}, ActualStep={}, ExpectedStep={}",
                  before_u64, after_u64, actual_step, expected_step);

            // Update our tracking baseline after step
            self.last_time_filetime = after_u64;
            self.last_adjustment_time = Instant::now();
        }

        info!("[StepClock] Step complete.");
        Ok(())
    }
}

impl Drop for WindowsClock {
    fn drop(&mut self) {
        info!("=== Windows Clock Shutdown ===");
        info!("Resetting clock to nominal frequency: {}", self.original_increment);
        info!("Total frequency adjustments made: {}", self.adjustment_count);

        unsafe {
            // On exit, set clock to run at 1x speed using the nominal frequency
            match SetSystemTimeAdjustmentPrecise(self.original_increment, false) {
                Ok(_) => info!("Clock reset to nominal successfully."),
                Err(e) => error!("Failed to reset clock: {}", e),
            }
        }

        info!("=== Windows Clock Shutdown Complete ===");
    }
}
