use anyhow::Result;
use std::time::Duration;

#[cfg_attr(test, mockall::automock)]
pub trait NtpSource {
    fn get_offset(&self) -> Result<(Duration, i8)>;
}

#[cfg_attr(test, mockall::automock)]
pub trait PtpNetwork {
    /// Receive a packet. Returns Ok(Some((data, len, timestamp))) if packet received.
    /// Returns Ok(None) if no packet (timeout/wouldblock).
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, std::time::SystemTime)>>;
    
    /// Reset the network state (e.g. clear buffers). Default impl does nothing.
    fn reset(&mut self) -> Result<()> { Ok(()) }
}