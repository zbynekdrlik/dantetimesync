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
}

impl Default for SystemConfig {
    fn default() -> Self {
        #[cfg(windows)]
        {
            SystemConfig {
                servo: ServoConfig {
                    kp: 0.1,
                    ki: 0.001,
                    max_freq_adj_ppm: 500_000.0,
                    max_integral_ppm: 100_000.0, // Windows needs massive integral range for VM drift
                },
                filters: FilterConfig {
                    step_threshold_ns: 10_000_000, // 10ms
                    panic_threshold_ns: 50_000_000, // 50ms
                    sample_window_size: 4,
                    min_delta_ns: 0,
                },
            }
        }

        #[cfg(not(windows))]
        {
            SystemConfig {
                servo: ServoConfig {
                    kp: 0.0005,
                    ki: 0.00005,
                    max_freq_adj_ppm: 500.0,    // Standard Crystal Limits
                    max_integral_ppm: 100.0,
                },
                filters: FilterConfig {
                    step_threshold_ns: 5_000_000,  // 5ms (Relaxed to catch initial drift)
                    panic_threshold_ns: 10_000_000, // 10ms
                    sample_window_size: 4,
                    min_delta_ns: 1_000_000, // 1ms
                },
            }
        }
    }
}
