use log::debug;
use crate::config::ServoConfig;

pub struct PiServo {
    config: ServoConfig,
    integral: f64,
}

impl PiServo {
    pub fn new(config: ServoConfig) -> Self {
        PiServo {
            config,
            integral: 0.0,
        }
    }

    pub fn reset(&mut self) {
        self.integral = 0.0;
    }

    /// Calculate frequency adjustment (in PPM) to correct the phase offset (in nanoseconds).
    /// `offset_ns`: Local - Master (positive if Local is ahead)
    pub fn sample(&mut self, offset_ns: i64) -> f64 {
        // We want to drive offset_ns to 0.
        // If offset_ns > 0 (ahead), we need to slow down (negative adj).
        // If offset_ns < 0 (behind), we need to speed up (positive adj).
        
        let error = -offset_ns as f64; 

        // Update Integral
        self.integral += error * self.config.ki;
        
        // Clamp integral
        if self.integral > self.config.max_integral_ppm { self.integral = self.config.max_integral_ppm; }
        if self.integral < -self.config.max_integral_ppm { self.integral = -self.config.max_integral_ppm; }

        // Proportional
        let proportional = error * self.config.kp;

        let adjustment_ppm = proportional + self.integral;
        
        // Clamp total adjustment
        let max_adj = self.config.max_freq_adj_ppm;
        let final_adj = if adjustment_ppm > max_adj { 
            max_adj 
        } else if adjustment_ppm < -max_adj { 
            -max_adj 
        } else { 
            adjustment_ppm 
        };
        
        debug!("Servo: Err={}ns, P={:.3}, I={:.3}, RawAdj={:.3}ppm, Final={:.3}ppm", 
            offset_ns, proportional, self.integral, adjustment_ppm, final_adj);
        
        final_adj
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