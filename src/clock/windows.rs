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
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::core::PCWSTR;
use std::time::Duration;
use log::{info, warn, debug, error};

pub struct WindowsClock {
    original_adjustment: u64,
    original_increment: u64,
    original_disabled: BOOL,
    nominal_frequency: u64,
}

impl WindowsClock {
    pub fn new() -> Result<Self> {
        Self::enable_privilege("SeSystemtimePrivilege")?;

        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);

        unsafe {
            if let Err(e) = GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled) {
                error!("GetSystemTimeAdjustmentPrecise failed: {}", e);
                return Err(anyhow!("GetSystemTimeAdjustmentPrecise failed: {}", e));
            }
        }
        
        info!("Windows Clock Initial State: Adj={}, Inc={}, Disabled={}", adj, inc, disabled.as_bool());

        // The reported Inc value from broken HALs (10,000,000) represents the actual
        // clock divisor the Windows kernel uses. We must use this value for our adjustments
        // to maintain correct time rate.
        let nominal = inc;
        info!("Using HAL-reported increment {} as nominal frequency", nominal);

        // If adjustment is currently disabled, enable it with the nominal value
        if disabled.as_bool() {
            info!("Setting initial clock adjustment to nominal={}", nominal);
            unsafe {
                if let Err(e) = SetSystemTimeAdjustmentPrecise(nominal, false) {
                    error!("Failed to set initial adjustment: {}", e);
                    return Err(anyhow!("Failed to set initial adjustment: {}", e));
                }
            }
            info!("Initial clock adjustment set successfully.");
        }

        Ok(WindowsClock {
            original_adjustment: adj,
            original_increment: inc,
            original_disabled: disabled,
            nominal_frequency: nominal,
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
        // Adjustment = Nominal * factor
        let new_adj = (self.nominal_frequency as f64 * factor).round() as u64;

        // Convert factor to PPM for logging
        let ppm = (factor - 1.0) * 1_000_000.0;

        unsafe {
            debug!("Adjusting frequency (Precise): Factor={:.9}, PPM={:.3}, Base={}, NewAdj={}",
                   factor, ppm, self.nominal_frequency, new_adj);

            if let Err(e) = SetSystemTimeAdjustmentPrecise(new_adj, false) {
                error!("SetSystemTimeAdjustmentPrecise failed: {}", e);
                return Err(anyhow!("SetSystemTimeAdjustmentPrecise failed: {}", e));
            }
        }
        Ok(())
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
            if let Err(e) = FileTimeToSystemTime(&ft_new, &mut st) {
                 return Err(anyhow!("FileTimeToSystemTime failed: {}", e));
            }
            if let Err(e) = SetSystemTime(&st) {
                 return Err(anyhow!("SetSystemTime failed: {}", e));
            }
        }

        Ok(())
    }
}

impl Drop for WindowsClock {
    fn drop(&mut self) {
        unsafe {
            // On exit, set clock to run at 1x speed using the corrected nominal frequency
            // DO NOT restore the original (potentially broken) HAL settings
            // This ensures the system clock runs correctly even after the service stops
            let _ = SetSystemTimeAdjustmentPrecise(self.nominal_frequency, false);
        }
    }
}