use super::SystemClock;
use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{BOOL, HANDLE, LUID, CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, SYSTEMTIME, FILETIME};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
    TOKEN_PRIVILEGES, SE_PRIVILEGE_ENABLED
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAdjustmentPrecise, SetSystemTimeAdjustmentPrecise, SetSystemTimeAdjustment,
    GetSystemTimeAsFileTime, SetSystemTime
};
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::core::PCWSTR;
use std::time::Duration;
use log::info;

pub struct WindowsClock {
    original_adjustment: u64,
    original_increment: u64,
    original_disabled: BOOL,
    nominal_frequency: u64,
}

impl WindowsClock {
    pub fn new() -> Result<Self> {
        Self::enable_privilege("SeSystemtimePrivilege")?;

        // Reset any existing adjustments to ensure a clean state
        unsafe {
            // Try to disable Precise first (if available)
            // We ignore errors here as it might not be active or available
            let _ = SetSystemTimeAdjustmentPrecise(0, true);
            // Disable Legacy
            let _ = SetSystemTimeAdjustment(0, true);
        }

        // Use Precise API if available (Windows 10+)
        // Fallback to legacy if needed? 
        // We assume modern Windows for now.
        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);

        unsafe {
            // GetSystemTimeAdjustmentPrecise is preferred to get the exact base increment
            if let Err(e) = GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled) {
                log::warn!("GetSystemTimeAdjustmentPrecise failed, trying legacy. Error: {}", e);
                // Fallback to legacy
                // But we define struct with u64.
                // Legacy returns u32.
                // For now, fail if Precise is missing (Win10+ required).
                return Err(anyhow!("GetSystemTimeAdjustmentPrecise failed (Win10+ required): {}", e));
            }
        }
        
        info!("Windows Clock Initial State: Adj={}, Inc={}, Disabled={}", adj, inc, disabled.as_bool());

        // Sanity check: If Inc is unreasonably large (e.g., 10,000,000 = 1s), it implies 
        // the OS reports a value inconsistent with the actual interrupt rate (typically 64Hz/15.6ms).
        // Using 1s Inc with 64Hz interrupts causes 64x time acceleration.
        if inc > 200_000 {
            log::warn!("Reported Time Increment {} is too large (>20ms). Suspect timer mismatch. Forcing standard 156,250 (15.625ms).", inc);
            inc = 156_250;
        }

        Ok(WindowsClock {
            original_adjustment: adj,
            original_increment: inc,
            original_disabled: disabled,
            nominal_frequency: inc, 
        })
    }

    fn enable_privilege(name: &str) -> Result<()> {
        unsafe {
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token)?;
            
            let mut luid = LUID::default();
            let mut name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
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
    fn adjust_frequency(&mut self, ppm: f64) -> Result<()> {
        // Calculate new adjustment based on nominal frequency (increment)
        // Adj = Inc + (Inc * ppm / 1e6)
        let adj_delta = (self.nominal_frequency as f64 * ppm / 1_000_000.0) as i32;
        let val = self.nominal_frequency as i32 + adj_delta;
        // Clamp to u32 range and ensure positive
        let new_adj = if val < 0 { 0 } else { val } as u32;

        unsafe {
            // Log the adjustment attempt for debugging
            log::debug!("Adjusting frequency (Legacy): PPM={:.3}, Base={}, NewAdj={}", ppm, self.nominal_frequency, new_adj);

            // Use Legacy API as Precise API seems to ignore the sanitized increment on this VM
            if SetSystemTimeAdjustment(new_adj, false).is_ok() {
                Ok(())
            } else {
                let err = GetLastError();
                log::error!("SetSystemTimeAdjustment failed! Error: {:?}", err);
                Err(anyhow::anyhow!("SetSystemTimeAdjustment failed: {:?}", err))
            }
        }
    }

    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        unsafe {
            let ft: FILETIME = GetSystemTimeAsFileTime();
            
            let mut u64_time = (ft.dwHighDateTime as u64) << 32 | (ft.dwLowDateTime as u64);
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
        }

        Ok(())
    }
}

impl Drop for WindowsClock {
    fn drop(&mut self) {
        unsafe {
            // Restore original settings
            // Use Precise here as we stored u64s
            let _ = SetSystemTimeAdjustmentPrecise(self.original_adjustment, self.original_disabled);
        }
    }
}