#![cfg(unix)]

use anyhow::{Result, anyhow};
use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::time::SystemTime;
use nix::ioctl_write_ptr;
use chrono::{DateTime, Datelike, Timelike, Local};

// Linux RTC Time Struct
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct RtcTime {
    pub tm_sec: i32,
    pub tm_min: i32,
    pub tm_hour: i32,
    pub tm_mday: i32,
    pub tm_mon: i32,
    pub tm_year: i32,
    pub tm_wday: i32,
    pub tm_yday: i32,
    pub tm_isdst: i32,
}

// Magic 'p' (0x70), number 0x0a
const RTC_MAGIC: u8 = b'p';
const RTC_SET_TIME_CMD: u8 = 0x0a;

ioctl_write_ptr!(rtc_set_time, RTC_MAGIC, RTC_SET_TIME_CMD, RtcTime);

pub fn update_rtc(time: SystemTime) -> Result<()> {
    let dt: DateTime<Local> = time.into();
    
    let rtc_val = RtcTime {
        tm_sec: dt.second() as i32,
        tm_min: dt.minute() as i32,
        tm_hour: dt.hour() as i32,
        tm_mday: dt.day() as i32,
        tm_mon: dt.month0() as i32, // rtc_time tm_mon is 0-11
        tm_year: dt.year() as i32 - 1900, // rtc_time tm_year is years since 1900
        tm_wday: 0, // Ignored by RTC_SET_TIME usually
        tm_yday: 0, // Ignored
        tm_isdst: 0, // Best guess
    };

    let file = OpenOptions::new().write(true).open("/dev/rtc0")?;
    
    unsafe {
        rtc_set_time(file.as_raw_fd(), &rtc_val)?;
    }
    
    Ok(())
}