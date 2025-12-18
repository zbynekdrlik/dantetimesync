pub mod ptp;
pub mod net;
pub mod clock;
pub mod ntp;
pub mod traits;
pub mod controller;
pub mod servo;
pub mod status;
pub mod config;

#[cfg(unix)]
pub mod rtc;
