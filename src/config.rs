use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub servo: ServoConfig,
    pub filters: FilterConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServoConfig {
    pub kp: f64,
    pub ki: f64,
    pub max_freq_adj_ppm: f64,
    pub max_integral_ppm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    pub step_threshold_ns: i64,      // "MASSIVE_DRIFT_THRESHOLD_NS"
    pub panic_threshold_ns: i64,     // "MAX_PHASE_OFFSET_FOR_STEP_NS"
    pub sample_window_size: usize,
    pub min_delta_ns: i64,
    pub calibration_samples: usize,  // Number of samples for timestamp calibration (0 = disabled)
    pub ptp_stepping_enabled: bool,  // Enable stepping based on PTP offset (disable for frequency-only sync)
}

impl Default for SystemConfig {
    fn default() -> Self {
        #[cfg(windows)]
        {
            // WINDOWS FREQUENCY ADJUSTMENT LIMITATION:
            // Both SetSystemTimeAdjustment (legacy 32-bit) and SetSystemTimeAdjustmentPrecise (64-bit)
            // APIs accept frequency adjustment values but have ZERO effect on actual clock speed on
            // most Windows systems. Testing shows: requested +50 PPM often results in observed -50 PPM.
            // This is a known Windows limitation - the APIs exist but the kernel doesn't honor them.
            //
            // SOLUTION: Use PTP stepping for all convergence. Frequency adjustment serves only as
            // a "best effort" assist. The stepping mechanism maintains ±200µs accuracy between steps.
            SystemConfig {
                servo: ServoConfig {
                    kp: 0.005,   // Low gain - frequency adjustment has minimal effect
                    ki: 0.0005,  // Low integral - prevents runaway since freq adj doesn't work
                    max_freq_adj_ppm: 10_000.0,  // Allow large values (won't hurt, API ignores them)
                    max_integral_ppm: 5_000.0,
                },
                filters: FilterConfig {
                    // Stepping is the PRIMARY correction mechanism on Windows
                    step_threshold_ns: 1_500_000, // 1.5ms - step when offset exceeds this
                    panic_threshold_ns: 5_000_000, // 5ms - initial coarse step threshold
                    sample_window_size: 8, // 8 samples for lucky packet filter
                    min_delta_ns: 0,
                    calibration_samples: 0,
                    ptp_stepping_enabled: true, // CRITICAL: Must be true - only way to converge on Windows
                },
            }
        }

        #[cfg(not(windows))]
        {
            SystemConfig {
                servo: ServoConfig {
                    kp: 0.0005,
                    ki: 0.00005,
                    max_freq_adj_ppm: 500.0,    
                    max_integral_ppm: 100.0,
                },
                filters: FilterConfig {
                    step_threshold_ns: 5_000_000,  // 5ms
                    panic_threshold_ns: 10_000_000, // 10ms
                    sample_window_size: 4,
                    min_delta_ns: 1_000_000, // 1ms
                    calibration_samples: 0, // Linux uses kernel timestamping, no calibration needed
                    ptp_stepping_enabled: true, // Linux kernel timestamps are accurate, stepping works
                },
            }
        }
    }
}
