use super::SystemClock;
use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{BOOL, HANDLE, LUID, CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED, SYSTEMTIME, FILETIME, WIN32_ERROR};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
    TOKEN_PRIVILEGES, SE_PRIVILEGE_ENABLED
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAdjustmentPrecise, SetSystemTimeAdjustmentPrecise,
    GetSystemTimeAsFileTime, SetSystemTime
};
use windows::Win32::System::Time::{FileTimeToSystemTime}; 
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

        let mut adj = 0u64;
        let mut inc = 0u64;
        let mut disabled = BOOL(0);

        unsafe {
            GetSystemTimeAdjustmentPrecise(&mut adj, &mut inc, &mut disabled)?;
        }
        
        info!("Windows Clock Initial State: Adj={}, Inc={}, Disabled={}", adj, inc, disabled.as_bool());

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
            
            // Compiler indicates GetLastError returns Result<()> in this environment.
            // We verify if the result is an error matching ERROR_NOT_ALL_ASSIGNED.
            if let Err(e) = GetLastError() {
                // ERROR_NOT_ALL_ASSIGNED is WIN32_ERROR. Convert to HRESULT for comparison if needed, 
                // or check if WIN32_ERROR impls PartialEq with Error code.
                // windows::core::Error.code() returns HRESULT.
                // WIN32_ERROR.to_hresult() returns HRESULT.
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
        let adj_delta = (self.base_increment as f64 * ppm / 1_000_000.0) as i32;
        let new_adj = (self.base_increment as i32 + adj_delta) as u32;

        unsafe {
            // Log the adjustment attempt for debugging
            log::debug!("Adjusting frequency: PPM={:.3}, Base={}, NewAdj={}", ppm, self.base_increment, new_adj);

            if SetSystemTimeAdjustment(new_adj, false).is_ok() {
                Ok(())
            } else {
                let err = GetLastError();
                log::error!("SetSystemTimeAdjustment failed! Error: {:?}", err);
                Err(anyhow::anyhow!("SetSystemTimeAdjustment failed: {:?}", err))
            }
        }
    }
}

    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        unsafe {
            let mut ft: FILETIME = GetSystemTimeAsFileTime();
            
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
            
            ft.dwLowDateTime = (u64_time & 0xFFFFFFFF) as u32;
            ft.dwHighDateTime = (u64_time >> 32) as u32;
            
            let mut st = SYSTEMTIME::default();
            FileTimeToSystemTime(&ft, &mut st)?;
            SetSystemTime(&st)?;
        }

        Ok(())
    }
}

impl Drop for WindowsClock {
    fn drop(&mut self) {
        unsafe {
            let _ = SetSystemTimeAdjustmentPrecise(self.original_adjustment, self.original_disabled);
        }
    }
}
