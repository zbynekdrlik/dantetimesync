//! PTP Controller - Core synchronization logic for Dante PTP time sync
//!
//! This controller implements a two-phase frequency synchronization approach:
//! 1. **Acquisition Phase**: Fast convergence using direct proportional control
//! 2. **Production Phase**: Precision maintenance using adaptive PI control with soft dead zones
//!
//! Key features:
//! - Lucky packet filtering (minimum offset selection) for jitter immunity
//! - Adaptive gain tuning based on oscillation detection
//! - Soft dead zones tuned for 96kHz audio (1 sample = 10.4µs)

use anyhow::Result;
use log::{info, warn, error, debug};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime};
use std::sync::{Arc, RwLock};
use crate::clock::SystemClock;
use crate::traits::{NtpSource, PtpNetwork};
use crate::ptp::{PtpV1Header, PtpV1Control, PtpV1FollowUpBody, PtpV1SyncMessageBody};
use crate::status::SyncStatus;
use crate::config::SystemConfig;

// ============================================================================
// CONSTANTS - Organized by functional area
// ============================================================================

// Safety limits
const MAX_DELTA_NS: i64 = 2_000_000_000;  // 2s - reject obviously invalid deltas

// ==========================================================================
// SELF-TUNING SERVO ALGORITHM
// ==========================================================================
// The key insight: when offset oscillates around zero, the AVERAGE correction
// needed to maintain that equals the natural drift compensation.
//
// Algorithm:
// 1. Strong P-term responds to offset → creates oscillation around zero
// 2. Track running average of total correction when offset is small
// 3. This average becomes our "drift baseline" - the steady-state correction
// 4. The drift baseline is the auto-learned natural clock drift
//
// This is self-tuning because:
// - P-term immediately responds to any offset
// - Drift baseline slowly converges to the correct value
// - No manual tuning needed - it learns from the oscillation pattern
// ==========================================================================

// ==========================================================================
// TWO-PHASE CONTROL: ACQUISITION vs PRODUCTION
// ==========================================================================
// ACQUISITION: Fast convergence to lock (offset > 50µs)
//   - Aggressive P-term: P_GAIN_ACQ = 1.0 (10x production)
//   - Target: reach <50µs within 1 minute
//
// PRODUCTION: Gentle stability (offset < 50µs)
//   - Gentle P-term: P_GAIN_PROD = 0.1
//   - Auto-adaptive drift learning
// ==========================================================================

// Acquisition phase (FAST convergence)
const P_GAIN_ACQ: f64 = 0.8;              // Aggressive P-term for quick lock
const P_MAX_ACQ_PPM: f64 = 200.0;         // Limit to prevent wild swings

// Production phase (gentle stability)
const P_GAIN_PROD: f64 = 0.1;             // Gentle P-term in production
const P_MAX_PROD_PPM: f64 = 100.0;        // Allow enough for high drift rates

// Phase transition threshold (with hysteresis to prevent hunting)
const PRODUCTION_ENTER_US: f64 = 40.0;   // Enter production below 40µs
const PRODUCTION_EXIT_US: f64 = 100.0;   // Exit production above 100µs

// Drift learning - only in production phase
const DRIFT_LEARN_RATE: f64 = 0.1;        // Learn rate in production (faster for high drift)
const DRIFT_MAX_PPM: f64 = 500.0;

// Lock detection (aligned with production thresholds)
const LOCK_OFFSET_THRESHOLD_US: f64 = 100.0;  // Same as PRODUCTION_EXIT_US
const LOCK_STABLE_COUNT: usize = 5;

// Lucky packet filter - minimum time between samples (config override available)
const DEFAULT_MIN_T1_DELTA_NS: i64 = 100_000_000;  // 100ms default (Dante sends ~125ms)

// Periodic NTP UTC alignment (steps clock without changing frequency)
const NTP_CHECK_INTERVAL_SECS: u64 = 30;       // Check NTP every 30 seconds
const NTP_SAMPLE_COUNT: usize = 5;             // Samples needed for reliable median
const NTP_STEP_THRESHOLD_US: i64 = 2_000;      // Only step if offset > 2ms (ignore small drifts)

// ============================================================================
// DATA STRUCTURES
// ============================================================================

/// Main PTP synchronization controller
pub struct PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    // Core components
    clock: C,
    network: N,
    ntp: S,
    config: SystemConfig,

    // PTP state
    pending_syncs: HashMap<u16, PendingSync>,
    prev_t1_ns: i64,
    prev_t2_ns: i64,
    current_gm_uuid: Option<[u8; 6]>,

    // Sample filtering
    sample_window: Vec<i64>,

    // Metrics (for status display)
    last_phase_offset_ns: i64,
    last_adj_ppm: f64,

    // Epoch tracking
    initial_epoch_offset_ns: i64,
    epoch_aligned: bool,

    // Settling state
    valid_count: usize,
    clock_settled: bool,
    settling_threshold: usize,

    // Shared status for IPC
    status_shared: Arc<RwLock<SyncStatus>>,

    // Calibration (Windows pcap offset compensation)
    calibration_samples: Vec<i64>,
    calibration_offset_ns: i64,
    calibration_complete: bool,

    // Frequency control state
    applied_freq_ppm: f64,

    // Warmup tracking
    warmup_start: Instant,
    warmup_complete: bool,

    // ==========================================================================
    // SELF-TUNING SERVO STATE
    // ==========================================================================
    // P-term creates oscillation, drift baseline is learned from average correction
    // ==========================================================================

    /// Learned drift baseline (auto-tuned from average correction when stable)
    drift_baseline_ppm: f64,

    /// Lock state - true when synchronized and stable
    is_locked: bool,
    lock_stable_count: usize,

    /// Production mode state (with hysteresis)
    in_production_mode: bool,

    // Rate-of-change tracking for Dante servo
    last_offset_us: Option<f64>,
    last_offset_time: Option<Instant>,
    smoothed_rate_ppm: f64,  // Exponential moving average of rate

    // Periodic NTP UTC tracking state
    last_ntp_check: Instant,
    ntp_offset_samples: VecDeque<i64>,  // in microseconds
    ntp_tracking_enabled: bool,
    last_ntp_step: Option<Instant>,  // Grace period after NTP stepping
}

struct PendingSync {
    rx_time_sys: SystemTime,
    source_uuid: [u8; 6],
}

// ============================================================================
// IMPLEMENTATION
// ============================================================================

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource,
{
    pub fn new(
        clock: C,
        network: N,
        ntp: S,
        status_shared: Arc<RwLock<SyncStatus>>,
        config: SystemConfig,
    ) -> Self {
        let window_size = config.filters.sample_window_size;
        let calibration_count = config.filters.calibration_samples;
        let calibration_complete = calibration_count == 0;

        info!("=== PTP Controller Initialization ===");
        info!("Mode: AUTO-ADAPTIVE DIRECT DRIFT MEASUREMENT");
        info!("  - Directly measures drift rate from offset samples");
        info!("  - No manual tuning required - works on any hardware");
        info!("Filter: window={}, min_delta={}ns", window_size, config.filters.min_delta_ns);
        info!("Calibration: {} ({})", calibration_count,
              if calibration_count > 0 { "enabled" } else { "disabled" });
        info!("=== Ready ===");

        let now = Instant::now();

        PtpController {
            clock,
            network,
            ntp,
            config,
            pending_syncs: HashMap::new(),
            prev_t1_ns: 0,
            prev_t2_ns: 0,
            current_gm_uuid: None,
            sample_window: Vec::with_capacity(window_size),
            last_phase_offset_ns: 0,
            last_adj_ppm: 0.0,
            initial_epoch_offset_ns: 0,
            epoch_aligned: false,
            valid_count: 0,
            clock_settled: false,
            settling_threshold: 1,
            status_shared,
            calibration_samples: Vec::with_capacity(calibration_count),
            calibration_offset_ns: 0,
            calibration_complete,
            applied_freq_ppm: 0.0,
            warmup_start: now,
            warmup_complete: false,
            // Self-tuning servo state
            drift_baseline_ppm: 0.0,
            is_locked: false,
            lock_stable_count: 0,
            in_production_mode: false,
            last_offset_us: None,
            last_offset_time: None,
            smoothed_rate_ppm: 0.0,
            // NTP UTC tracking - enabled on BOTH platforms
            // PTP (Dante) controls frequency only, NTP maintains UTC alignment
            // Dante provides device uptime, NOT UTC - so NTP is needed for real time
            last_ntp_check: now,
            ntp_offset_samples: VecDeque::with_capacity(NTP_SAMPLE_COUNT + 2),
            ntp_tracking_enabled: true,  // Always enabled - NTP is the UTC time source
            last_ntp_step: None,
        }
    }

    // ========================================================================
    // PUBLIC API
    // ========================================================================

    pub fn get_status_shared(&self) -> Arc<RwLock<SyncStatus>> {
        self.status_shared.clone()
    }

    pub fn run_ntp_sync(&mut self, skip: bool) {
        if skip { return; }

        match self.ntp.get_offset() {
            Ok((offset, sign)) => {
                let sign_str = if sign > 0 { "+" } else { "-" };
                info!("NTP Sync: Offset {}{:?}", sign_str, offset);

                if offset.as_millis() > 50 {
                    info!("Stepping clock (NTP)...");
                    if let Err(e) = self.clock.step_clock(offset, sign) {
                        error!("Failed to step clock: {}", e);
                    } else {
                        info!("Clock stepped successfully.");
                    }
                } else {
                    info!("Offset small, skipping step.");
                }
            }
            Err(e) => warn!("NTP Sync failed: {}", e),
        }
    }

    /// Periodic NTP UTC alignment - steps clock to maintain UTC sync
    ///
    /// This keeps all computers aligned to real UTC time by:
    /// - Checking NTP offset every 30 seconds (only in production mode)
    /// - Stepping clock if offset exceeds 500µs threshold
    /// - ONLY sets time value - does NOT change frequency (Dante stays locked)
    ///
    /// Key insight: step_clock() and adjust_frequency() are independent:
    /// - step_clock() = SetSystemTime() - sets absolute time value
    /// - adjust_frequency() = SetSystemTimeAdjustmentPrecise() - sets tick rate
    /// Stepping time does NOT affect the Dante-tuned frequency!
    pub fn check_ntp_utc_tracking(&mut self) {
        // Only check NTP once locked (tracking is stable) and tracking is enabled
        if !self.is_locked || !self.ntp_tracking_enabled {
            return;
        }

        // Check if enough time has passed since last NTP query
        if self.last_ntp_check.elapsed() < Duration::from_secs(NTP_CHECK_INTERVAL_SECS) {
            return;
        }

        self.last_ntp_check = Instant::now();

        // Query NTP and record offset
        match self.ntp.get_offset() {
            Ok((offset, sign)) => {
                let offset_us = (offset.as_nanos() as i64 / 1000) * sign as i64;

                // Add sample to buffer
                self.ntp_offset_samples.push_back(offset_us);
                if self.ntp_offset_samples.len() > NTP_SAMPLE_COUNT + 2 {
                    self.ntp_offset_samples.pop_front();
                }

                // Update shared status with NTP offset for tray app display
                if let Ok(mut status) = self.status_shared.write() {
                    status.ntp_offset_us = offset_us;
                }

                // Log current offset
                info!("[NTP] offset:{:+}us", offset_us);

                // Step clock if offset exceeds threshold
                if offset_us.abs() > NTP_STEP_THRESHOLD_US {
                    let step_us = offset_us;

                    // Apply the step (sets time, does NOT change frequency)
                    let step_dur = Duration::from_micros(step_us.abs() as u64);
                    let step_sign = if step_us > 0 { 1 } else { -1 };

                    if let Err(e) = self.clock.step_clock(step_dur, step_sign) {
                        warn!("[NTP] Step failed: {}", e);
                    } else {
                        // Clear NTP samples after step to start fresh measurement
                        self.ntp_offset_samples.clear();
                        // Clear PTP sample window to discard post-step transient samples
                        self.sample_window.clear();
                        // Set grace period to skip PTP samples for 2s after step
                        self.last_ntp_step = Some(Instant::now());
                        // Reset drift tracking to avoid false spike from step
                        self.last_offset_us = None;
                        self.last_offset_time = None;
                        info!("[NTP] Stepped {:+}us", step_us);
                    }
                }
            }
            Err(e) => {
                warn!("[NTP] Failed: {}", e);
            }
        }
    }

    /// Enable or disable periodic NTP UTC tracking
    pub fn set_ntp_tracking(&mut self, enabled: bool) {
        self.ntp_tracking_enabled = enabled;
        info!("[NTP-UTC] Tracking {}", if enabled { "enabled" } else { "disabled" });
    }

    pub fn log_status(&self) {
        // Just update shared status for IPC - no redundant logging
        self.update_shared_status();
    }

    pub fn process_loop_iteration(&mut self) -> Result<()> {
        let (buf, size, t2) = match self.network.recv_packet()? {
            Some(res) => res,
            None => return Ok(()),
        };

        if size < PtpV1Header::SIZE {
            return Ok(());
        }

        let header = match PtpV1Header::parse(&buf[..size]) {
            Ok(h) => h,
            Err(_) => return Ok(()),
        };

        match header.message_type {
            PtpV1Control::Sync => self.handle_sync_message(&header, &buf[..size], t2),
            PtpV1Control::FollowUp => self.handle_followup_message(&header, &buf[..size]),
            _ => {}
        }

        // Cleanup stale pending syncs
        if self.pending_syncs.len() > 100 {
            let now = SystemTime::now();
            self.pending_syncs.retain(|_, v| {
                now.duration_since(v.rx_time_sys).unwrap_or(Duration::ZERO) < Duration::from_secs(5)
            });
        }

        // Periodic NTP UTC tracking (every 30s in production mode)
        self.check_ntp_utc_tracking();

        Ok(())
    }

    // ========================================================================
    // PACKET HANDLING
    // ========================================================================

    fn handle_sync_message(&mut self, header: &PtpV1Header, buf: &[u8], t2: SystemTime) {
        self.pending_syncs.insert(header.sequence_id, PendingSync {
            rx_time_sys: t2,
            source_uuid: header.source_uuid,
        });

        if let Ok(body) = PtpV1SyncMessageBody::parse(&buf[PtpV1Header::SIZE..]) {
            let new_uuid = body.grandmaster_clock_uuid;
            match self.current_gm_uuid {
                Some(current) if current != new_uuid => {
                    info!("Grandmaster changed: {:02X?} -> {:02X?}", current, new_uuid);
                    self.current_gm_uuid = Some(new_uuid);
                    self.reset_filter();
                }
                None => {
                    info!("Locked to Grandmaster: {:02X?}", new_uuid);
                    self.current_gm_uuid = Some(new_uuid);
                }
                _ => {}
            }
        }
    }

    fn handle_followup_message(&mut self, header: &PtpV1Header, buf: &[u8]) {
        if let Ok(body) = PtpV1FollowUpBody::parse(&buf[PtpV1Header::SIZE..]) {
            if let Some(sync_info) = self.pending_syncs.remove(&body.associated_sequence_id) {
                if sync_info.source_uuid == header.source_uuid {
                    self.process_sync_pair(body.precise_origin_timestamp.to_nanos(), sync_info.rx_time_sys);
                }
            }
        }
    }

    // ========================================================================
    // SYNC PAIR PROCESSING - Main synchronization logic
    // ========================================================================

    fn process_sync_pair(&mut self, t1_ns: i64, t2_sys: SystemTime) {
        let t2_ns = t2_sys.duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;

        // Calculate display phase offset (modulo-based for readability)
        let phase_offset_ns = self.calculate_phase_offset(t1_ns, t2_ns);

        // Handle calibration if needed
        if self.process_calibration(phase_offset_ns) {
            return;
        }

        // Apply calibration offset
        let phase_offset_ns = phase_offset_ns - self.calibration_offset_ns;

        // Handle warmup period
        if !self.process_warmup() {
            return;
        }

        // Establish baseline
        self.update_baseline(t1_ns, phase_offset_ns);

        // Log delta sanity check
        self.log_delta_sanity(t1_ns, t2_ns);

        // Process sync once settled
        self.valid_count += 1;
        if self.valid_count >= self.settling_threshold {
            self.process_settled_sync(t1_ns, t2_ns, phase_offset_ns);
        }

        self.prev_t1_ns = t1_ns;
        self.prev_t2_ns = t2_ns;
    }

    fn calculate_phase_offset(&self, t1_ns: i64, t2_ns: i64) -> i64 {
        let time_diff_ns = t2_ns - t1_ns;
        let mut display_phase = (t2_ns % 1_000_000_000) - (t1_ns % 1_000_000_000);
        if display_phase > 500_000_000 { display_phase -= 1_000_000_000; }
        else if display_phase < -500_000_000 { display_phase += 1_000_000_000; }

        debug!("T1={:.3}s T2={:.3}s diff={:.3}s phase={}us",
               t1_ns as f64 / 1e9, t2_ns as f64 / 1e9, time_diff_ns as f64 / 1e9, display_phase / 1000);
        display_phase
    }

    fn process_calibration(&mut self, phase_offset_ns: i64) -> bool {
        let count = self.config.filters.calibration_samples;
        if self.calibration_complete || count == 0 {
            return false;
        }

        self.calibration_samples.push(phase_offset_ns);
        if self.calibration_samples.len() >= count {
            let mut sorted = self.calibration_samples.clone();
            sorted.sort();
            self.calibration_offset_ns = sorted[sorted.len() / 2];
            self.calibration_complete = true;
            info!("Calibration complete: offset={:.3}ms ({} samples)",
                  self.calibration_offset_ns as f64 / 1_000_000.0, count);
        }
        true
    }

    fn process_warmup(&mut self) -> bool {
        if self.warmup_complete {
            return true;
        }

        let warmup_secs = self.config.filters.warmup_secs;
        if warmup_secs <= 0.0 || self.warmup_start.elapsed().as_secs_f64() >= warmup_secs {
            self.warmup_complete = true;
            if warmup_secs > 0.0 {
                info!("[Warmup] Complete after {:.1}s", warmup_secs);
            }
            true
        } else {
            false
        }
    }

    fn update_baseline(&mut self, _t1_ns: i64, _phase_offset_ns: i64) {
        // First sample logging removed - not useful in production
    }

    fn log_delta_sanity(&self, t1_ns: i64, t2_ns: i64) {
        if self.prev_t1_ns > 0 && self.prev_t2_ns > 0 {
            let delta_master = t1_ns - self.prev_t1_ns;
            let delta_slave = t2_ns - self.prev_t2_ns;

            if delta_master > 0 && delta_master < MAX_DELTA_NS {
                let ratio = delta_slave as f64 / delta_master as f64;
                if ratio < 0.5 || ratio > 2.0 {
                    debug!("[Jitter] master={}ms slave={}ms ratio={:.2}x",
                           delta_master / 1_000_000, delta_slave / 1_000_000, ratio);
                }
            }
        }
    }

    fn process_settled_sync(&mut self, t1_ns: i64, t2_ns: i64, phase_offset_ns: i64) {
        if !self.clock_settled {
            self.clock_settled = true;
            self.initial_epoch_offset_ns = t2_ns - t1_ns;
            self.epoch_aligned = true;
            info!("Sync established.");
        }

        // Collect sample if enough time has passed
        if self.should_add_sample(t1_ns) {
            self.sample_window.push(phase_offset_ns);
        }

        // Process window when full - pass master time for drift calculation
        if self.sample_window.len() >= self.config.filters.sample_window_size {
            self.process_sample_window(t1_ns);
        }
    }

    // NOTE: PTP stepping removed - Dante provides device uptime, not UTC.
    // NTP handles all time stepping via check_ntp_utc_tracking().

    fn should_add_sample(&self, t1_ns: i64) -> bool {
        // Skip samples during 2s grace period after NTP step (prevents transient from corrupting servo)
        if let Some(step_time) = self.last_ntp_step {
            if step_time.elapsed() < Duration::from_secs(2) {
                debug!("[NTP-Grace] Skipping sample during post-step grace period");
                return false;
            }
        }
        if self.prev_t1_ns == 0 {
            return true;
        }
        // Use config value if > 0, otherwise default (Dante sends packets every ~125ms)
        let min_delta = if self.config.filters.min_delta_ns > 0 {
            self.config.filters.min_delta_ns
        } else {
            DEFAULT_MIN_T1_DELTA_NS
        };
        (t1_ns - self.prev_t1_ns).abs() >= min_delta
    }

    // ========================================================================
    // SAMPLE WINDOW PROCESSING - SELF-TUNING SERVO
    // ========================================================================
    //
    // The algorithm:
    // 1. Strong P-term responds to offset → creates oscillation around zero
    // 2. When offset is small, we learn that the current correction = drift
    // 3. Drift baseline slowly converges to the natural clock drift
    // 4. No manual tuning needed - it auto-learns from the oscillation
    //
    // ========================================================================

    fn process_sample_window(&mut self, _master_time_ns: i64) {
        let mut sorted = self.sample_window.clone();
        sorted.sort();

        let median = sorted[sorted.len() / 2];

        // UNIFIED: Use median for both platforms (robust against outliers)
        let offset_ns = median;
        let offset_us = offset_ns as f64 / 1000.0;

        debug!("[Filter] min={:.1}us max={:.1}us median={:.1}us",
               sorted.first().map(|&x| x as f64 / 1000.0).unwrap_or(0.0),
               sorted.last().map(|&x| x as f64 / 1000.0).unwrap_or(0.0),
               offset_us);

        self.last_phase_offset_ns = offset_ns;

        // Apply self-tuning servo
        self.apply_self_tuning_servo(offset_us);

        self.sample_window.clear();
    }

    /// Self-tuning servo algorithm
    ///
    /// Key insight: When offset oscillates around zero, the average correction
    /// needed to maintain that IS the drift compensation we need.
    ///
    /// So we:
    /// 1. Use P-term to respond to offset (creates oscillation)
    /// 2. When offset is small, learn drift from the total correction
    /// 3. This naturally converges to the right drift baseline
    fn apply_self_tuning_servo(&mut self, offset_us: f64) {
        // DANTE PTP FREQUENCY SYNC - Rate-of-Change Based Servo
        //
        // Key insight: Dante PTP timestamps are device uptime, NOT UTC.
        // The absolute offset (e.g., 182ms) is meaningless for time accuracy.
        // What matters is the RATE OF CHANGE of offset:
        // - If offset is stable → frequencies are matched ✓
        // - If offset is growing → local clock is too fast
        // - If offset is shrinking → local clock is too slow
        //
        // NTP handles UTC alignment separately. PTP only matches frequency.

        // Skip correction during post-step grace period
        if let Some(step_time) = self.last_ntp_step {
            if step_time.elapsed() < Duration::from_secs(2) {
                debug!("[Servo] In grace period, skipping correction");
                return;
            }
        }

        // Track offset for rate calculation
        let now = Instant::now();
        let dt_secs = self.last_offset_time.map(|t| now.duration_since(t).as_secs_f64())
                                           .unwrap_or(1.0);

        // Calculate instantaneous rate of change (drift rate in ppm)
        // delta_offset / delta_time gives us the frequency error
        let raw_rate_ppm = if let Some(prev_offset) = self.last_offset_us {
            if dt_secs > 0.1 {  // Need meaningful time delta
                let delta_offset = offset_us - prev_offset;
                // Convert: us/s = ppm
                (delta_offset / dt_secs).clamp(-500.0, 500.0)
            } else {
                self.smoothed_rate_ppm  // Keep previous
            }
        } else {
            0.0
        };

        // Store for next iteration
        self.last_offset_us = Some(offset_us);
        self.last_offset_time = Some(now);

        // Smooth rate with exponential moving average
        const RATE_SMOOTH_ALPHA: f64 = 0.3;  // Higher = more responsive
        self.smoothed_rate_ppm = self.smoothed_rate_ppm * (1.0 - RATE_SMOOTH_ALPHA)
                                + raw_rate_ppm * RATE_SMOOTH_ALPHA;
        let rate_ppm = self.smoothed_rate_ppm;

        // TWO-PHASE CONTROL based on rate stability, not absolute offset
        let abs_rate = rate_ppm.abs();
        if abs_rate < 5.0 {  // Rate stable within 5ppm
            self.in_production_mode = true;
        } else if abs_rate > 20.0 {  // Rate unstable above 20ppm
            self.in_production_mode = false;
        }

        let (p_gain, p_max, phase_name) = if self.in_production_mode {
            (P_GAIN_PROD, P_MAX_PROD_PPM, "PROD")
        } else {
            (P_GAIN_ACQ, P_MAX_ACQ_PPM, "ACQ")
        };

        // P-term: responds to rate of change (not absolute offset!)
        // Negative rate = clock too slow, need positive adjustment
        let p_term = (-rate_ppm * p_gain).clamp(-p_max, p_max);

        // I-term: Integrate rate error to learn true drift
        // This continuously adjusts drift_baseline to eliminate rate error
        const I_GAIN: f64 = 0.05;  // Slow integration to avoid oscillation
        let i_term = -rate_ppm * I_GAIN;  // Integrate rate error
        self.drift_baseline_ppm = (self.drift_baseline_ppm + i_term).clamp(-DRIFT_MAX_PPM, DRIFT_MAX_PPM);

        // Total correction = drift baseline + P-term
        let total_correction = (self.drift_baseline_ppm + p_term).clamp(-DRIFT_MAX_PPM, DRIFT_MAX_PPM);

        // Lock state: based on rate stability, not absolute offset
        let rate_stable = abs_rate < 5.0;  // Within 5ppm
        if rate_stable {
            self.lock_stable_count += 1;
            if self.lock_stable_count >= LOCK_STABLE_COUNT && !self.is_locked {
                self.is_locked = true;
                info!("[PTP] === LOCKED === Adj:{:+.1}ppm", self.drift_baseline_ppm);
            }
        } else {
            if self.lock_stable_count > 0 {
                self.lock_stable_count -= 1;  // Gradual unlock
            }
            if self.lock_stable_count == 0 && self.is_locked {
                self.is_locked = false;
                info!("[PTP] === UNLOCKED === Drift:{:+.1}us/s", rate_ppm);
            }
        }

        // Apply correction
        self.last_adj_ppm = total_correction;
        self.applied_freq_ppm = total_correction;
        let factor = 1.0 + (total_correction / 1_000_000.0);

        let status = if self.is_locked { "LOCK" } else { phase_name };

        // User-friendly log: drift rate (stability) and frequency adjustment
        info!("[PTP] {:4}  Drift:{:+6.1}us/s  Adj:{:+6.1}ppm",
              status, rate_ppm, total_correction);

        if let Err(e) = self.clock.adjust_frequency(factor) {
            warn!("Clock adjustment failed: {}", e);
        }

        self.update_shared_status();
    }

    // ========================================================================
    // UTILITY METHODS
    // ========================================================================

    fn update_shared_status(&self) {
        if let Ok(mut status) = self.status_shared.write() {
            // Core fields
            status.offset_ns = self.last_phase_offset_ns;
            status.drift_ppm = self.last_adj_ppm;
            status.gm_uuid = self.current_gm_uuid;
            status.settled = self.clock_settled;
            status.updated_ts = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            // Extended fields for tray app
            status.is_locked = self.is_locked;
            status.smoothed_rate_ppm = self.smoothed_rate_ppm;
            status.mode = if self.is_locked {
                "LOCK".to_string()
            } else if self.in_production_mode {
                "PROD".to_string()
            } else {
                "ACQ".to_string()
            };
            // NTP offset is updated separately via check_ntp_utc_tracking()
        }
    }

    fn reset_filter(&mut self) {
        self.valid_count = 0;
        self.clock_settled = false;
        self.prev_t1_ns = 0;
        self.prev_t2_ns = 0;
        self.sample_window.clear();
        self.warmup_start = Instant::now();
        self.warmup_complete = false;

        // Reset self-tuning servo state
        self.drift_baseline_ppm = 0.0;
        self.is_locked = false;
        self.lock_stable_count = 0;
        self.in_production_mode = false;
        self.applied_freq_ppm = 0.0;
        self.last_offset_us = None;
        self.last_offset_time = None;
        self.smoothed_rate_ppm = 0.0;

        // Reset NTP tracking
        self.ntp_offset_samples.clear();
        self.last_ntp_check = Instant::now();
        self.last_ntp_step = None;

        if let Err(e) = self.network.reset() {
            warn!("Network reset failed: {}", e);
        }

        self.update_shared_status();
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockSystemClock;
    use crate::traits::{MockNtpSource, MockPtpNetwork};
    use mockall::predicate::*;

    #[test]
    fn test_ntp_sync_trigger() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();

        mock_ntp.expect_get_offset()
            .times(1)
            .returning(|| Ok((Duration::from_millis(100), 1)));

        mock_clock.expect_step_clock()
            .with(eq(Duration::from_millis(100)), eq(1))
            .times(1)
            .returning(|_, _| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp, status, SystemConfig::default());
        controller.run_ntp_sync(false);
    }

    #[test]
    fn test_ptp_locking_flow() {
        use byteorder::{BigEndian, WriteBytesExt};

        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mut mock_net = MockPtpNetwork::new();
        let mock_ntp = MockNtpSource::new();

        let gm_uuid = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

        let make_sync = move |seq: u16| -> Vec<u8> {
            let mut buf = vec![0u8; 60];
            buf[0] = 0x10;
            buf[32] = 0x00;
            buf[22..28].copy_from_slice(&gm_uuid);
            let mut w = &mut buf[30..32];
            w.write_u16::<BigEndian>(seq).unwrap();
            buf[49..55].copy_from_slice(&gm_uuid);
            buf
        };

        let make_followup = move |seq: u16, t1_ns: u64| -> Vec<u8> {
            let mut buf = vec![0u8; 60];
            buf[0] = 0x10;
            buf[32] = 0x02;
            buf[22..28].copy_from_slice(&gm_uuid);
            let mut w = &mut buf[30..32];
            w.write_u16::<BigEndian>(seq).unwrap();
            let mut w = &mut buf[42..44];
            w.write_u16::<BigEndian>(seq).unwrap();
            let mut w = &mut buf[44..52];
            let s = (t1_ns / 1_000_000_000) as u32;
            let n = (t1_ns % 1_000_000_000) as u32;
            w.write_u32::<BigEndian>(s).unwrap();
            w.write_u32::<BigEndian>(n).unwrap();
            buf
        };

        for i in 0..8 {
            let t1 = 1_000_000_000 + i as u64 * 1_000_000_000;
            let t2 = SystemTime::UNIX_EPOCH + Duration::from_nanos(t1 + 1000);

            let sync_pkt = make_sync(i as u16);
            let follow_pkt = make_followup(i as u16, t1);

            mock_net.expect_recv_packet()
                .times(1)
                .returning(move || Ok(Some((sync_pkt.clone(), 60, t2))));

            mock_net.expect_recv_packet()
                .times(1)
                .returning(move || Ok(Some((follow_pkt.clone(), 60, t2))));
        }

        mock_net.expect_recv_packet().returning(|| Ok(None));
        mock_clock.expect_adjust_frequency().times(2).returning(|_| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.sample_window_size = 4;
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp, status, config);

        for _ in 0..16 {
            let _ = controller.process_loop_iteration();
        }

        assert!(controller.get_status_shared().read().unwrap().settled);
    }
}
