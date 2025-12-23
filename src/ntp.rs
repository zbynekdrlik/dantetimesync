use anyhow::Result;
use rsntp::SntpClient;
use std::time::Duration;

pub struct NtpClient {
    server: String,
}

impl NtpClient {
    pub fn new(server: &str) -> Self {
        NtpClient {
            server: server.to_string(),
        }
    }

    /// Fetches the current time from the NTP server.
    /// Returns the offset required to apply to the local system time (Local + Offset = True Time).
    /// Positive offset means local clock is behind (needs to step forward).
    pub fn get_offset(&self) -> Result<(Duration, i8)> {
        let client = SntpClient::new();
        let result = client.synchronize(&self.server)?;

        let offset = result.clock_offset();
        let offset_secs = offset.as_secs_f64();

        let sign = if offset_secs < 0.0 { -1 } else { 1 };
        let abs_secs = offset_secs.abs();

        // Convert abs_secs to Duration
        let secs = abs_secs.trunc() as u64;
        let nanos = (abs_secs.fract() * 1_000_000_000.0) as u32;

        Ok((Duration::new(secs, nanos), sign))
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use std::time::Duration;

    /// Test offset to Duration+sign conversion logic
    /// This mirrors the logic in get_offset() but with controlled inputs
    #[test]
    fn test_offset_to_duration_positive() {
        // Simulate positive offset (local clock behind, needs step forward)
        let offset_secs: f64 = 1.5;  // +1.5 seconds

        let sign = if offset_secs < 0.0 { -1 } else { 1 };
        let abs_secs = offset_secs.abs();
        let secs = abs_secs.trunc() as u64;
        let nanos = (abs_secs.fract() * 1_000_000_000.0) as u32;
        let duration = Duration::new(secs, nanos);

        assert_eq!(sign, 1);
        assert_eq!(duration.as_secs(), 1);
        assert_eq!(duration.subsec_nanos(), 500_000_000);
    }

    #[test]
    fn test_offset_to_duration_negative() {
        // Simulate negative offset (local clock ahead, needs step backward)
        let offset_secs: f64 = -2.25;  // -2.25 seconds

        let sign = if offset_secs < 0.0 { -1 } else { 1 };
        let abs_secs = offset_secs.abs();
        let secs = abs_secs.trunc() as u64;
        let nanos = (abs_secs.fract() * 1_000_000_000.0) as u32;
        let duration = Duration::new(secs, nanos);

        assert_eq!(sign, -1);
        assert_eq!(duration.as_secs(), 2);
        assert_eq!(duration.subsec_nanos(), 250_000_000);
    }

    #[test]
    fn test_offset_to_duration_zero() {
        // Simulate zero offset
        let offset_secs: f64 = 0.0;

        let sign = if offset_secs < 0.0 { -1 } else { 1 };
        let abs_secs = offset_secs.abs();
        let secs = abs_secs.trunc() as u64;
        let nanos = (abs_secs.fract() * 1_000_000_000.0) as u32;
        let duration = Duration::new(secs, nanos);

        assert_eq!(sign, 1);  // 0 is considered positive
        assert_eq!(duration.as_secs(), 0);
        assert_eq!(duration.subsec_nanos(), 0);
    }

    #[test]
    fn test_offset_small_values() {
        // Test sub-second offset (common case)
        let offset_secs: f64 = 0.002;  // 2ms

        let abs_secs = offset_secs.abs();
        let secs = abs_secs.trunc() as u64;
        let nanos = (abs_secs.fract() * 1_000_000_000.0) as u32;
        let duration = Duration::new(secs, nanos);

        assert_eq!(duration.as_secs(), 0);
        assert_eq!(duration.subsec_millis(), 2);
    }

    #[test]
    fn test_offset_microsecond_precision() {
        // Test microsecond precision
        let offset_secs: f64 = 0.000500;  // 500us

        let abs_secs = offset_secs.abs();
        let secs = abs_secs.trunc() as u64;
        let nanos = (abs_secs.fract() * 1_000_000_000.0) as u32;
        let duration = Duration::new(secs, nanos);

        assert_eq!(duration.as_secs(), 0);
        assert_eq!(duration.subsec_micros(), 500);
    }

    #[test]
    fn test_ntp_client_new() {
        let client = super::NtpClient::new("pool.ntp.org");
        assert_eq!(client.server, "pool.ntp.org");
    }
}