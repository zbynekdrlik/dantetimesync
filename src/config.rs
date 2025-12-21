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
            SystemConfig {
                servo: ServoConfig {
                    kp: 0.005, // Reduced from 0.1 to prevent oscillation (8Hz limit ~0.008)
                    ki: 0.0005,
                    // Windows SetSystemTimeAdjustmentPrecise may not support extreme adjustments.
                    // Use stepping for large offsets, frequency adjustment for fine-tuning.
                    max_freq_adj_ppm: 10_000.0,  // 1% max adjustment (reasonable for Windows)
                    max_integral_ppm: 5_000.0,
                },
                filters: FilterConfig {
                    step_threshold_ns: 1_000_000, // 1ms - step if offset exceeds this
                    panic_threshold_ns: 10_000_000, // 10ms - initial step threshold
                    sample_window_size: 32, // Larger window to filter Windows jitter
                    min_delta_ns: 0,
                    calibration_samples: 0,
                    ptp_stepping_enabled: true, // Enable stepping to achieve <50us offset
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
