use anyhow::Result;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::cell::RefCell;
use dantetimesync::clock::SystemClock;
use dantetimesync::traits::{NtpSource, PtpNetwork};
use dantetimesync::config::{SystemConfig, ServoConfig, FilterConfig};
use dantetimesync::controller::PtpController;
use dantetimesync::status::SyncStatus;
use std::f64::consts::PI;

// --- Physics Engine ---

struct PhysicsEngine {
    time: f64, // seconds
    offset_ns: f64, // Local - Master
    
    natural_drift_ppm: f64,
    current_adj_ppm: f64,
    step_offset_ns: f64,
}

impl PhysicsEngine {
    fn new(drift: f64) -> Self {
        PhysicsEngine {
            time: 0.0,
            offset_ns: 0.0,
            natural_drift_ppm: drift,
            current_adj_ppm: 0.0,
            step_offset_ns: 0.0,
        }
    }
    
    fn advance(&mut self, dt: f64) {
        self.time += dt;
        let rate_ppm = self.natural_drift_ppm + self.current_adj_ppm;
        let rate_ns_per_sec = rate_ppm * 1000.0;
        self.offset_ns += rate_ns_per_sec * dt;
    }
}

struct SharedPhysics {
    engine: RefCell<PhysicsEngine>,
}

#[derive(Clone)]
struct SimClockRef(Arc<SharedPhysics>);

impl SystemClock for SimClockRef {
    fn adjust_frequency(&mut self, freq: f64) -> Result<()> {
        let ppm = (freq - 1.0) * 1_000_000.0;
        self.0.engine.borrow_mut().current_adj_ppm = ppm;
        Ok(())
    }

    fn step_clock(&mut self, step: Duration, sign: i8) -> Result<()> {
        let ns = step.as_nanos() as i64 * sign as i64;
        self.0.engine.borrow_mut().step_offset_ns += ns as f64;
        Ok(())
    }
}

struct StatefulNetwork {
    physics: Arc<SharedPhysics>,
    jitter_sigma_ns: f64,
    seq: u16,
    pending_followup: Option<(u16, u64)>, // (seq, t1)
}

impl PtpNetwork for StatefulNetwork {
    fn recv_packet(&mut self) -> Result<Option<(Vec<u8>, usize, SystemTime)>> {
        let mut phys = self.physics.engine.borrow_mut();
        
        if let Some((seq, t1)) = self.pending_followup {
            self.pending_followup = None;
            let t2_sys = SystemTime::UNIX_EPOCH;
            
            let mut buf = vec![0u8; 60];
            buf[0] = 0x10;
            buf[32] = 0x02; // FollowUp
            buf[30] = (seq >> 8) as u8;
            buf[31] = (seq & 0xFF) as u8;
            buf[42] = (seq >> 8) as u8;
            buf[43] = (seq & 0xFF) as u8;
            
            let s = (t1 / 1_000_000_000) as u32;
            let n = (t1 % 1_000_000_000) as u32;
            use byteorder::{BigEndian, ByteOrder};
            BigEndian::write_u32(&mut buf[44..48], s);
            BigEndian::write_u32(&mut buf[48..52], n);
            
            return Ok(Some((buf, 60, t2_sys)));
        }
        
        // Advance time (packet interval 125ms)
        phys.advance(0.125); 
        self.seq = self.seq.wrapping_add(1);
        
        let t1_ns = (phys.time * 1_000_000_000.0) as u64;
        
        // Calculate T2 (Local Receive Time)
        let offset = phys.offset_ns + phys.step_offset_ns;
        
        // Box-Muller Noise
        let u1: f64 = rand::random();
        let u2: f64 = rand::random();
        let z0 = (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos();
        let noise = z0 * self.jitter_sigma_ns;
        
        let t2_ns_val = (t1_ns as f64 + offset + noise) as u64;
        let t2_sys = SystemTime::UNIX_EPOCH + Duration::from_nanos(t2_ns_val);
        
        let mut buf = vec![0u8; 60];
        buf[0] = 0x10;
        buf[32] = 0x00; // Sync
        buf[30] = (self.seq >> 8) as u8;
        buf[31] = (self.seq & 0xFF) as u8;
        buf[49] = 1;
        
        self.pending_followup = Some((self.seq, t1_ns));
        
        Ok(Some((buf, 60, t2_sys)))
    }
    
    fn reset(&mut self) -> Result<()> { Ok(()) }
}

struct SimNtp {
    physics: Arc<SharedPhysics>,
}
impl NtpSource for SimNtp {
    fn get_offset(&self) -> Result<(Duration, i8)> {
        let phys = self.physics.engine.borrow();
        let off = phys.offset_ns + phys.step_offset_ns;
        let sign = if off >= 0.0 { 1 } else { -1 };
        Ok((Duration::from_nanos(off.abs() as u64), sign))
    }
}

// --- The Test Runner ---

fn run_simulation(
    config: SystemConfig, 
    jitter_ns: f64, 
    drift_ppm: f64,
    duration_secs: usize
) -> (f64, f64) { 
    let physics = Arc::new(SharedPhysics {
        engine: RefCell::new(PhysicsEngine::new(drift_ppm)),
    });
    
    let status = Arc::new(RwLock::new(SyncStatus::default()));
    
    let network = StatefulNetwork {
        physics: physics.clone(),
        jitter_sigma_ns: jitter_ns,
        seq: 0,
        pending_followup: None,
    };
    
    let ntp = SimNtp { physics: physics.clone() };
    let clock = SimClockRef(physics.clone());
    
    let mut controller = PtpController::new(clock, network, ntp, status, config);
    
    let steps = duration_secs * 16; // 8 packets/sec * 2 (Sync+FU)
    let mut max_offset_steady = 0.0;
    
    for i in 0..steps {
        controller.process_loop_iteration().unwrap();
        
        if i > steps / 2 {
            let phys = physics.engine.borrow();
            let current_off = (phys.offset_ns + phys.step_offset_ns).abs();
            if current_off > max_offset_steady {
                max_offset_steady = current_off;
            }
        }
    }
    
    let final_offset = {
        let phys = physics.engine.borrow();
        (phys.offset_ns + phys.step_offset_ns).abs()
    };
    
    (final_offset, max_offset_steady)
}

#[test]
fn test_linux_stability_low_jitter() {
    let mut config = SystemConfig::default(); 
    config.servo.kp = 0.0005;
    config.servo.ki = 0.00005;
    config.filters.step_threshold_ns = 5_000_000;
    
    // 50us jitter, 50ppm drift
    let (final_off, max_off) = run_simulation(config, 50_000.0, 50.0, 100);
    
    println!("Linux Stable: Final {:.3}us, Max {:.3}us", final_off/1000.0, max_off/1000.0);
    assert!(final_off < 100_000.0, "Final offset too high");
}

#[test]
fn test_windows_stability_high_jitter() {
    let mut config = SystemConfig::default();
    config.servo.kp = 0.1;
    config.servo.ki = 0.001;
    config.filters.step_threshold_ns = 10_000_000;
    
    // 2ms jitter, 500ppm drift
    let (final_off, max_off) = run_simulation(config, 2_000_000.0, 500.0, 100);
    
    println!("Windows Stable: Final {:.3}ms, Max {:.3}ms", final_off/1_000_000.0, max_off/1_000_000.0);
    assert!(final_off < 5_000_000.0, "Final offset too high");
}

#[test]
fn test_regression_high_gain_low_jitter() {
    let mut config = SystemConfig::default();
    config.servo.kp = 0.1; // HIGH GAIN
    config.servo.ki = 0.001;
    
    // 10us jitter (Very low, like Linux HW)
    let (final_off, max_off) = run_simulation(config, 10_000.0, 50.0, 100);
    
    println!("Regression Check: Final {:.3}us, Max {:.3}us", final_off/1000.0, max_off/1000.0);
    
    // With Kp=0.1 (100ppm/us), 10us noise = 1000ppm kick.
    // 1000ppm * 0.125s = 125us movement in next step.
    // It should oscillate violently or at least be noisy.
    
    // Let's see if it's worse than the "Stable" Linux config (which was <100us).
    // If max_off > 500us, it's unstable.
    if max_off > 500_000.0 {
        println!("Confirmed: High Gain on Low Jitter causes instability/oscillation.");
    } else {
        println!("Note: High Gain might handle Low Jitter okay if noise is really small.");
    }
}
