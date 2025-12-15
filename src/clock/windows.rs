use super::SystemClock;
use anyhow::{Result, anyhow};
use windows::Win32::Foundation::{BOOL, HANDLE, LUID, CloseHandle, GetLastError, ERROR_NOT_ALL_ASSIGNED};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, TOKEN_ADJUST_PRIVILEGES, TOKEN_QUERY,
    TOKEN_PRIVILEGES, SE_PRIVILEGE_ENABLED
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::SystemInformation::{
    GetSystemTimeAdjustmentPrecise, SetSystemTimeAdjustmentPrecise
};
use windows::Win32::System::Time::{GetSystemTimeAsFileTime, FileTimeToSystemTime, SetSystemTime, SYSTEMTIME, FILETIME};
use windows::core::PCWSTR;
use std::time::Duration;

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
            
            if GetLastError() == ERROR_NOT_ALL_ASSIGNED {
                 return Err(anyhow!("Failed to adjust privilege: ERROR_NOT_ALL_ASSIGNED"));
            }
            
            CloseHandle(token)?;
        }
        Ok(())
    }
}

impl SystemClock for WindowsClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        let new_adj = (self.nominal_frequency as f64 * factor).round() as u64;
        unsafe {
            SetSystemTimeAdjustmentPrecise(new_adj, BOOL(0))?;
        }
        Ok(())
    }

    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        unsafe {
            let mut ft = FILETIME::default();
            GetSystemTimeAsFileTime(&mut ft);
            
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
