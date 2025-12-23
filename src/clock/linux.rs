use super::SystemClock;
use anyhow::{Result, anyhow};
use libc::{self, timex, adjtimex, ADJ_FREQUENCY, timeval, settimeofday};
use std::mem;
use std::time::Duration;

pub struct LinuxClock {
    original_freq: i64,
}

impl LinuxClock {
    pub fn new() -> Result<Self> {
        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = 0; // Query mode
        
        let ret = unsafe { adjtimex(&mut tx) };
        if ret < 0 {
            return Err(anyhow!("adjtimex failed (are you root?)"));
        }

        Ok(LinuxClock {
            original_freq: tx.freq,
        })
    }
}

impl SystemClock for LinuxClock {
    fn adjust_frequency(&mut self, factor: f64) -> Result<()> {
        let ppm = (factor - 1.0) * 1_000_000.0;
        let freq_val = (ppm * 65536.0) as i64;
        
        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = ADJ_FREQUENCY;
        tx.freq = freq_val;

        let ret = unsafe { adjtimex(&mut tx) };
        if ret < 0 {
             return Err(anyhow!("adjtimex failed to set frequency"));
        }
        
        Ok(())
    }

    fn step_clock(&mut self, offset: Duration, sign: i8) -> Result<()> {
        let mut tv: timeval = unsafe { mem::zeroed() };
        unsafe { libc::gettimeofday(&mut tv, std::ptr::null_mut()) };

        let offset_sec = offset.as_secs() as i64;
        let offset_usec = offset.subsec_micros() as i64;

        if sign > 0 {
            tv.tv_sec += offset_sec;
            tv.tv_usec += offset_usec;
        } else {
            tv.tv_sec -= offset_sec;
            tv.tv_usec -= offset_usec;
        }

        // Normalize
        while tv.tv_usec >= 1_000_000 {
            tv.tv_sec += 1;
            tv.tv_usec -= 1_000_000;
        }
        while tv.tv_usec < 0 {
            tv.tv_sec -= 1;
            tv.tv_usec += 1_000_000;
        }

        let ret = unsafe { settimeofday(&tv, std::ptr::null()) };
        if ret < 0 {
            return Err(anyhow!("settimeofday failed: errno={}", std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

impl Drop for LinuxClock {
    fn drop(&mut self) {
        let mut tx: timex = unsafe { mem::zeroed() };
        tx.modes = ADJ_FREQUENCY;
        tx.freq = self.original_freq;
        unsafe { adjtimex(&mut tx) };
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    /// Test PPM to freq_val conversion math
    /// The kernel uses freq_val = ppm * 65536 (16-bit fixed point)
    #[test]
    fn test_ppm_to_freq_val_conversion() {
        // Helper to compute freq_val from factor (same logic as adjust_frequency)
        fn factor_to_freq_val(factor: f64) -> i64 {
            let ppm = (factor - 1.0) * 1_000_000.0;
            (ppm * 65536.0) as i64
        }

        // No adjustment: factor = 1.0 → ppm = 0 → freq_val = 0
        assert_eq!(factor_to_freq_val(1.0), 0);

        // +100ppm: factor = 1.0001 → ppm = 100 → freq_val ≈ 6553600
        // Allow ±1 for floating point rounding
        let freq_100ppm = factor_to_freq_val(1.0001);
        assert!((freq_100ppm - 6553600).abs() <= 1,
                "Expected ~6553600, got {}", freq_100ppm);

        // -100ppm: factor = 0.9999 → ppm = -100 → freq_val ≈ -6553600
        let freq_neg100ppm = factor_to_freq_val(0.9999);
        assert!((freq_neg100ppm + 6553600).abs() <= 1,
                "Expected ~-6553600, got {}", freq_neg100ppm);

        // Test exact integer PPM values using direct calculation
        // +1ppm exactly: ppm * 65536 = 65536
        let freq_1ppm_direct = (1.0_f64 * 65536.0) as i64;
        assert_eq!(freq_1ppm_direct, 65536);

        // -1ppm exactly
        let freq_neg1ppm_direct = (-1.0_f64 * 65536.0) as i64;
        assert_eq!(freq_neg1ppm_direct, -65536);

        // Verify the conversion formula is correct for boundary values
        // At 500ppm (max adjustment): freq_val = 500 * 65536 = 32768000
        let freq_500ppm = (500.0_f64 * 65536.0) as i64;
        assert_eq!(freq_500ppm, 32768000);
    }

    /// Test tv_usec normalization logic
    #[test]
    fn test_tv_usec_normalization() {
        // Helper to normalize tv_usec (same logic as step_clock)
        fn normalize_timeval(tv_sec: &mut i64, tv_usec: &mut i64) {
            while *tv_usec >= 1_000_000 {
                *tv_sec += 1;
                *tv_usec -= 1_000_000;
            }
            while *tv_usec < 0 {
                *tv_sec -= 1;
                *tv_usec += 1_000_000;
            }
        }

        // Overflow case: tv_usec = 1,500,000 → should normalize to sec+1, usec=500,000
        let (mut sec, mut usec) = (10, 1_500_000);
        normalize_timeval(&mut sec, &mut usec);
        assert_eq!(sec, 11);
        assert_eq!(usec, 500_000);

        // Double overflow: tv_usec = 2,500,000
        let (mut sec, mut usec) = (10, 2_500_000);
        normalize_timeval(&mut sec, &mut usec);
        assert_eq!(sec, 12);
        assert_eq!(usec, 500_000);

        // Underflow case: tv_usec = -500,000 → should normalize to sec-1, usec=500,000
        let (mut sec, mut usec) = (10, -500_000);
        normalize_timeval(&mut sec, &mut usec);
        assert_eq!(sec, 9);
        assert_eq!(usec, 500_000);

        // Double underflow: tv_usec = -1,500,000
        let (mut sec, mut usec) = (10, -1_500_000);
        normalize_timeval(&mut sec, &mut usec);
        assert_eq!(sec, 8);
        assert_eq!(usec, 500_000);

        // No change needed
        let (mut sec, mut usec) = (10, 500_000);
        normalize_timeval(&mut sec, &mut usec);
        assert_eq!(sec, 10);
        assert_eq!(usec, 500_000);
    }

    /// Test step_clock offset calculation
    #[test]
    fn test_step_offset_calculation() {
        use std::time::Duration;

        // Helper to compute new timeval from base + offset (same logic as step_clock)
        fn apply_step(base_sec: i64, base_usec: i64, offset: Duration, sign: i8) -> (i64, i64) {
            let offset_sec = offset.as_secs() as i64;
            let offset_usec = offset.subsec_micros() as i64;

            let (mut tv_sec, mut tv_usec) = (base_sec, base_usec);

            if sign > 0 {
                tv_sec += offset_sec;
                tv_usec += offset_usec;
            } else {
                tv_sec -= offset_sec;
                tv_usec -= offset_usec;
            }

            // Normalize
            while tv_usec >= 1_000_000 {
                tv_sec += 1;
                tv_usec -= 1_000_000;
            }
            while tv_usec < 0 {
                tv_sec -= 1;
                tv_usec += 1_000_000;
            }

            (tv_sec, tv_usec)
        }

        // Step forward by 1.5 seconds
        let (sec, usec) = apply_step(100, 250_000, Duration::from_micros(1_500_000), 1);
        assert_eq!(sec, 101);
        assert_eq!(usec, 750_000);

        // Step backward by 1.5 seconds
        let (sec, usec) = apply_step(100, 250_000, Duration::from_micros(1_500_000), -1);
        assert_eq!(sec, 98);
        assert_eq!(usec, 750_000);

        // Small step forward (500us)
        let (sec, usec) = apply_step(100, 999_000, Duration::from_micros(500), 1);
        assert_eq!(sec, 100);
        assert_eq!(usec, 999_500);

        // Small step causing overflow
        let (sec, usec) = apply_step(100, 999_000, Duration::from_micros(2000), 1);
        assert_eq!(sec, 101);
        assert_eq!(usec, 1_000);
    }
}