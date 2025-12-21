use log::{debug, info, warn};
use crate::config::ServoConfig;

pub struct PiServo {
    config: ServoConfig,
    integral: f64,
    sample_count: u64,
    integral_clamped: bool,
    output_clamped: bool,
}

impl PiServo {
    pub fn new(config: ServoConfig) -> Self {
        info!("=== PI Servo Initialization ===");
        info!("Kp={}, Ki={}", config.kp, config.ki);
        info!("Max Frequency Adjustment: ±{:.0} PPM", config.max_freq_adj_ppm);
        info!("Max Integral: ±{:.0} PPM", config.max_integral_ppm);
        info!("=== PI Servo Ready ===");

        PiServo {
            config,
            integral: 0.0,
            sample_count: 0,
            integral_clamped: false,
            output_clamped: false,
        }
    }

    pub fn reset(&mut self) {
        info!("[Servo] RESET - Integral cleared (was {:.3} PPM)", self.integral);
        self.integral = 0.0;
        self.integral_clamped = false;
        self.output_clamped = false;
    }

    /// Calculate frequency adjustment (in PPM) to correct the phase offset (in nanoseconds).
    /// `offset_ns`: Local - Master (positive if Local is ahead)
    pub fn sample(&mut self, offset_ns: i64) -> f64 {
        self.sample_count += 1;

        // We want to drive offset_ns to 0.
        // If offset_ns > 0 (ahead), we need to slow down (negative adj).
        // If offset_ns < 0 (behind), we need to speed up (positive adj).

        let error = -offset_ns as f64;
        let error_us = offset_ns as f64 / 1000.0;

        // Calculate integral update
        let integral_update = error * self.config.ki;
        let integral_before = self.integral;
        self.integral += integral_update;

        // Clamp integral
        let integral_clamped_now;
        if self.integral > self.config.max_integral_ppm {
            self.integral = self.config.max_integral_ppm;
            integral_clamped_now = true;
        } else if self.integral < -self.config.max_integral_ppm {
            self.integral = -self.config.max_integral_ppm;
            integral_clamped_now = true;
        } else {
            integral_clamped_now = false;
        }

        // Log warning if integral just became clamped
        if integral_clamped_now && !self.integral_clamped {
            warn!("[Servo] Integral CLAMPED at {:.3} PPM (max={:.0})", self.integral, self.config.max_integral_ppm);
        }
        self.integral_clamped = integral_clamped_now;

        // Proportional
        let proportional = error * self.config.kp;

        let adjustment_ppm = proportional + self.integral;

        // Clamp total adjustment
        let max_adj = self.config.max_freq_adj_ppm;
        let output_clamped_now;
        let final_adj = if adjustment_ppm > max_adj {
            output_clamped_now = true;
            max_adj
        } else if adjustment_ppm < -max_adj {
            output_clamped_now = true;
            -max_adj
        } else {
            output_clamped_now = false;
            adjustment_ppm
        };

        // Log warning if output just became clamped
        if output_clamped_now && !self.output_clamped {
            warn!("[Servo] Output CLAMPED at {:.3} PPM (max={:.0})", final_adj, max_adj);
        }
        self.output_clamped = output_clamped_now;

        // Detailed debug logging
        debug!("[Servo #{}] Offset={:+.1}µs → Error={:.0}ns | P={:+.3}ppm (Kp*err) | I={:+.3}ppm (was {:+.3}) | Raw={:+.3}ppm | Final={:+.3}ppm{}",
               self.sample_count,
               error_us,
               offset_ns,
               proportional,
               self.integral,
               integral_before,
               adjustment_ppm,
               final_adj,
               if output_clamped_now { " [CLAMPED]" } else { "" });

        final_adj
    }

    /// Get current integral value for diagnostics
    pub fn get_integral(&self) -> f64 {
        self.integral
    }

    /// Get sample count for diagnostics
    pub fn get_sample_count(&self) -> u64 {
        self.sample_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServoConfig;

    fn get_test_config(kp: f64, ki: f64) -> ServoConfig {
        ServoConfig {
            kp,
            ki,
            max_freq_adj_ppm: 2_000_000.0,
            max_integral_ppm: 15_000.0,
        }
    }

    #[test]
    fn test_servo_proportional() {
        // Zero Ki, purely Proportional
        let mut servo = PiServo::new(get_test_config(0.001, 0.0));
        
        // Offset 1000ns (ahead) -> Error -1000 -> Adj -1.0 ppm
        let adj = servo.sample(1000);
        assert!((adj - -1.0).abs() < 0.0001);
    }

    #[test]
    fn test_servo_output_clamping() {
        let mut servo = PiServo::new(get_test_config(1.0, 0.0)); // High Kp
        
        // Huge offset: 1s = 1,000,000,000ns.
        // P = -1e9.
        // Should clamp to -2,000,000.0
        let adj = servo.sample(1_000_000_000);
        assert_eq!(adj, -2000000.0);
    }

    #[test]
    fn test_servo_integral_accumulation() {
        let mut servo = PiServo::new(get_test_config(0.0, 0.001)); // Pure Integral
        
        // Error -1000. I += -1.0. Adj -1.0
        let adj1 = servo.sample(1000);
        assert!((adj1 - -1.0).abs() < 0.0001);
        
        // Error -1000 again. I += -1.0 -> -2.0. Adj -2.0
        let adj2 = servo.sample(1000);
        assert!((adj2 - -2.0).abs() < 0.0001);
    }

    #[test]
    fn test_servo_reset() {
        let mut servo = PiServo::new(get_test_config(0.0, 0.001));
        servo.sample(1000); // I = -1.0
        assert!(servo.integral.abs() > 0.0);
        
        servo.reset();
        assert_eq!(servo.integral, 0.0);
        
        let adj = servo.sample(0);
        assert_eq!(adj, 0.0);
    }

    #[test]
    fn test_servo_integral_clamping() {
        let mut config = get_test_config(0.0, 1.0);
        config.max_integral_ppm = 200.0;
        let mut servo = PiServo::new(config); 
        
        // Huge error to trigger clamp
        servo.sample(-20000); 
        
        assert!((servo.integral - 200.0).abs() < 0.0001);
        
        let adj = servo.sample(0); 
        assert!((adj - 200.0).abs() < 0.0001);
    }
}