use log::debug;

pub struct PiServo {
    kp: f64,
    ki: f64,
    integral: f64,
    max_integral: f64,
}

impl PiServo {
    pub fn new(kp: f64, ki: f64) -> Self {
        PiServo {
            kp,
            ki,
            integral: 0.0,
            // 200 PPM is a safe upper bound for standard crystal drift.
            // Allowing more just invites instability (windup).
            max_integral: 200.0, 
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
        self.integral += error * self.ki;
        
        // Clamp integral
        if self.integral > self.max_integral { self.integral = self.max_integral; }
        if self.integral < -self.max_integral { self.integral = -self.max_integral; }

        // Proportional
        let proportional = error * self.kp;

        let adjustment_ppm = proportional + self.integral;
        
        // Clamp total adjustment to reasonable limits.
        // On systems with 10M increment (64x gain), we divide the input by 64.
        // To achieve 3% effective correction (30,000 ppm), we need 30,000 * 64 = 1,920,000 ppm input.
        // We allow up to 2,000,000 ppm (200%).
        let max_adj = 2000000.0;
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

    #[test]
    fn test_servo_proportional() {
        // Zero Ki, purely Proportional
        let mut servo = PiServo::new(0.001, 0.0);
        
        // Offset 1000ns (ahead) -> Error -1000 -> Adj -1.0 ppm
        let adj = servo.sample(1000);
        assert!((adj - -1.0).abs() < 0.0001);
    }

    #[test]
    fn test_servo_output_clamping() {
        let mut servo = PiServo::new(1.0, 0.0); // High Kp
        
        // Huge offset: 1s = 1,000,000,000ns.
        // P = -1e9.
        // Should clamp to -2,000,000.0
        let adj = servo.sample(1_000_000_000);
        assert_eq!(adj, -2000000.0);
    }

    #[test]
    fn test_servo_integral_accumulation() {
        let mut servo = PiServo::new(0.0, 0.001); // Pure Integral
        
        // Error -1000. I += -1.0. Adj -1.0
        let adj1 = servo.sample(1000);
        assert!((adj1 - -1.0).abs() < 0.0001);
        
        // Error -1000 again. I += -1.0 -> -2.0. Adj -2.0
        let adj2 = servo.sample(1000);
        assert!((adj2 - -2.0).abs() < 0.0001);
    }

    #[test]
    fn test_servo_reset() {
        let mut servo = PiServo::new(0.0, 0.001);
        servo.sample(1000); // I = -1.0
        assert!(servo.integral.abs() > 0.0);
        
        servo.reset();
        assert_eq!(servo.integral, 0.0);
        
        let adj = servo.sample(0);
        assert_eq!(adj, 0.0);
    }

    #[test]
    fn test_servo_integral_clamping() {
        let mut servo = PiServo::new(0.0, 1.0); // High Ki
        
        // Huge error to trigger clamp (Max 200)
        servo.sample(-300); // Error 300. I += 300 -> Clamped to 200.
        
        assert!((servo.integral - 200.0).abs() < 0.0001);
        
        let adj = servo.sample(0); // Error 0. Adj = I = 200.
        assert!((adj - 200.0).abs() < 0.0001);
    }
}