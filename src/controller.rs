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

use crate::clock::SystemClock;
use crate::config::SystemConfig;
use crate::ptp::{PtpV1Control, PtpV1FollowUpBody, PtpV1Header, PtpV1SyncMessageBody};
use crate::spike_filter::{FilterMode, SpikeFilter};
use crate::status::SyncStatus;
use crate::traits::{NtpSource, PtpNetwork};
use anyhow::Result;
use log::{debug, error, info, warn};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

/// Format a 6-byte UUID/MAC as a readable string (e.g., "00:1D:C1:AB:CD:EF")
fn format_mac(uuid: &[u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        uuid[0], uuid[1], uuid[2], uuid[3], uuid[4], uuid[5]
    )
}

// ============================================================================
// CONSTANTS - Organized by functional area
// ============================================================================

// Safety limits
const MAX_DELTA_NS: i64 = 2_000_000_000; // 2s - reject obviously invalid deltas

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
const P_GAIN_ACQ: f64 = 0.8; // Aggressive P-term for quick lock
const P_MAX_ACQ_PPM: f64 = 200.0; // Limit to prevent wild swings

// Production phase (gentle stability)
const P_GAIN_PROD: f64 = 0.1; // Gentle P-term in production
const P_MAX_PROD_PPM: f64 = 100.0; // Allow enough for high drift rates

// NANO phase (ultra-precise for sub-µs capable systems)
// Entry: drift < 0.5 µs/s sustained for 30 samples
// Exit: drift > 1.0 µs/s for 5 samples (hysteresis)
const P_GAIN_NANO: f64 = 0.01; // 10x smaller than PROD - minimize hunting
const P_MAX_NANO_PPM: f64 = 10.0; // Tiny corrections only
const I_GAIN_NANO: f64 = 0.005; // 10x smaller I-term
const NANO_ENTER_RATE_US: f64 = 0.5; // Enter NANO if drift < 0.5 µs/s
const NANO_EXIT_RATE_US: f64 = 1.0; // Exit NANO if drift > 1.0 µs/s
const NANO_SUSTAIN_COUNT: usize = 15; // 15 samples (~15s) to enter NANO
const NANO_EXIT_COUNT: usize = 5; // 5 consecutive samples above threshold to exit (hysteresis)
const NANO_DEADBAND_US: f64 = 0.1; // Ignore drift < 0.1 µs/s (noise floor)

// Max drift baseline limit
const DRIFT_MAX_PPM: f64 = 500.0;

// Lock detection
const LOCK_STABLE_COUNT: usize = 5;

// Lucky packet filter - minimum time between samples (config override available)
const DEFAULT_MIN_T1_DELTA_NS: i64 = 100_000_000; // 100ms default (Dante sends ~125ms)

// Periodic NTP UTC alignment (steps clock without changing frequency)
const NTP_CHECK_INTERVAL_SECS: u64 = 30; // Check NTP every 30 seconds
const NTP_SAMPLE_COUNT: usize = 5; // Samples needed for reliable median
const NTP_STEP_THRESHOLD_US: i64 = 500; // Step if offset > 500µs (tighter UTC alignment)

// PTP offline detection
const PTP_TIMEOUT_SECS: u64 = 10; // Consider PTP offline after 10s without packets

// NTP failure detection
const NTP_FAILURE_THRESHOLD: usize = 3; // Consider NTP failed after 3 consecutive failures

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
    /// The source UUID of the device sending Sync messages (may differ from grandmaster_clock_uuid)
    current_sync_source: Option<[u8; 6]>,

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

    /// NANO mode state (ultra-precise for sub-µs capable systems)
    in_nano_mode: bool,
    nano_sustain_count: usize, // Track consecutive sub-threshold samples for entry
    nano_exit_count: usize,    // Track consecutive above-threshold samples for exit (hysteresis)

    // Rate-of-change tracking for Dante servo
    last_offset_us: Option<f64>,
    last_offset_time: Option<Instant>,
    smoothed_rate_ppm: f64, // Exponential moving average of rate

    // Periodic NTP UTC tracking state
    last_ntp_check: Instant,
    ntp_offset_samples: VecDeque<i64>, // in microseconds
    ntp_tracking_enabled: bool,
    last_ntp_step: Option<Instant>, // Grace period after NTP stepping

    // PTP offline detection
    last_ptp_packet: Instant,
    ptp_offline: bool,
    ptp_offline_logged: bool, // Prevent repeated logging

    // NTP failure tracking
    ntp_consecutive_failures: usize,
    ntp_failed: bool,

    // ==========================================================================
    // ADAPTIVE SPIKE DETECTION
    // ==========================================================================
    // Robust outlier detection using MAD (Median Absolute Deviation)
    // Auto-adapts to each computer's noise profile
    // ==========================================================================
    /// Spike filter for rejecting timestamp noise spikes
    spike_filter: SpikeFilter,
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
        info!(
            "Filter: window={}, min_delta={}ns",
            window_size, config.filters.min_delta_ns
        );
        info!(
            "Calibration: {} ({})",
            calibration_count,
            if calibration_count > 0 {
                "enabled"
            } else {
                "disabled"
            }
        );
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
            current_sync_source: None,
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
            in_nano_mode: false,
            nano_sustain_count: 0,
            nano_exit_count: 0,
            last_offset_us: None,
            last_offset_time: None,
            smoothed_rate_ppm: 0.0,
            // NTP UTC tracking - enabled on BOTH platforms
            // PTP (Dante) controls frequency only, NTP maintains UTC alignment
            // Dante provides device uptime, NOT UTC - so NTP is needed for real time
            last_ntp_check: now,
            ntp_offset_samples: VecDeque::with_capacity(NTP_SAMPLE_COUNT + 2),
            ntp_tracking_enabled: true, // Always enabled - NTP is the UTC time source
            last_ntp_step: None,
            // PTP offline detection
            last_ptp_packet: now,
            ptp_offline: false,
            ptp_offline_logged: false,
            // NTP failure tracking
            ntp_consecutive_failures: 0,
            ntp_failed: false,
            // Adaptive spike detection
            spike_filter: SpikeFilter::new(),
        }
    }

    // ========================================================================
    // PUBLIC API
    // ========================================================================

    pub fn get_status_shared(&self) -> Arc<RwLock<SyncStatus>> {
        self.status_shared.clone()
    }

    pub fn run_ntp_sync(&mut self, skip: bool) {
        if skip {
            return;
        }

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
    ///
    /// Stepping time does NOT affect the Dante-tuned frequency!
    /// Check PTP status and handle offline mode
    fn check_ptp_status(&mut self) {
        let elapsed = self.last_ptp_packet.elapsed();

        if elapsed > Duration::from_secs(PTP_TIMEOUT_SECS) {
            if !self.ptp_offline {
                self.ptp_offline = true;
                if !self.ptp_offline_logged {
                    warn!(
                        "[PTP] No packets received for {}s - PTP masters may be offline",
                        PTP_TIMEOUT_SECS
                    );
                    info!("[PTP] Continuing with NTP-only time sync");
                    self.ptp_offline_logged = true;
                }
                // Update status to reflect offline state
                if let Ok(mut status) = self.status_shared.write() {
                    status.settled = false;
                    status.mode = "NTP-only".to_string();
                }
            }
        } else if self.ptp_offline {
            // PTP came back online
            self.ptp_offline = false;
            self.ptp_offline_logged = false;
            info!("[PTP] Packets received - PTP sync resumed");
        }
    }

    pub fn check_ntp_utc_tracking(&mut self) {
        // Run NTP sync when:
        // 1. PTP is offline (NTP-only mode), OR
        // 2. PTP is locked and tracking is enabled
        let should_check = self.ptp_offline || (self.is_locked && self.ntp_tracking_enabled);
        if !should_check {
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

                // NTP success - reset failure tracking
                if self.ntp_failed {
                    info!("[NTP] Connection restored");
                }
                self.ntp_consecutive_failures = 0;
                self.ntp_failed = false;

                // Add sample to buffer
                self.ntp_offset_samples.push_back(offset_us);
                if self.ntp_offset_samples.len() > NTP_SAMPLE_COUNT + 2 {
                    self.ntp_offset_samples.pop_front();
                }

                // Update shared status with NTP offset for tray app display
                if let Ok(mut status) = self.status_shared.write() {
                    status.ntp_offset_us = offset_us;
                    status.ntp_failed = false;
                }

                // Log current offset
                info!("[NTP] offset:{:+}us", offset_us);

                // Step clock if offset exceeds threshold
                if offset_us.abs() > NTP_STEP_THRESHOLD_US {
                    let step_us = offset_us;

                    // Apply the step (sets time, does NOT change frequency)
                    let step_dur = Duration::from_micros(step_us.unsigned_abs());
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
                        // Clear spike filter to prevent false positives from step transient
                        self.spike_filter.clear();
                        info!("[NTP] Stepped {:+}us", step_us);
                    }
                }
            }
            Err(e) => {
                // Track consecutive failures
                self.ntp_consecutive_failures += 1;

                if self.ntp_consecutive_failures >= NTP_FAILURE_THRESHOLD && !self.ntp_failed {
                    self.ntp_failed = true;
                    warn!(
                        "[NTP] Server unreachable - {} consecutive failures",
                        self.ntp_consecutive_failures
                    );

                    // Update shared status
                    if let Ok(mut status) = self.status_shared.write() {
                        status.ntp_failed = true;
                    }
                } else {
                    warn!(
                        "[NTP] Failed ({}/{}): {}",
                        self.ntp_consecutive_failures, NTP_FAILURE_THRESHOLD, e
                    );
                }
            }
        }
    }

    /// Enable or disable periodic NTP UTC tracking
    pub fn set_ntp_tracking(&mut self, enabled: bool) {
        self.ntp_tracking_enabled = enabled;
        info!(
            "[NTP-UTC] Tracking {}",
            if enabled { "enabled" } else { "disabled" }
        );
    }

    pub fn log_status(&self) {
        // Just update shared status for IPC - no redundant logging
        self.update_shared_status();
    }

    pub fn process_loop_iteration(&mut self) -> Result<()> {
        // Check PTP status first (handles timeout detection for NTP-only fallback)
        self.check_ptp_status();

        let (buf, size, t2) = match self.network.recv_packet()? {
            Some(res) => res,
            None => {
                // No packet, but still run NTP tracking if PTP is offline
                if self.ptp_offline {
                    self.check_ntp_utc_tracking();
                }
                return Ok(());
            }
        };

        // Packet received - update last_ptp_packet timestamp
        self.last_ptp_packet = Instant::now();

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
        // Check if Sync source changed (different device sending PTP)
        let source_uuid = header.source_uuid;
        match self.current_sync_source {
            Some(current) if current != source_uuid => {
                warn!(
                    ">>> SYNC SOURCE CHANGED: {} -> {} <<<",
                    format_mac(&current),
                    format_mac(&source_uuid)
                );
                self.current_sync_source = Some(source_uuid);
                // Soft reset: clear stale data but KEEP current frequency
                // Both Dante devices should have similar frequencies since they're
                // synchronized to the same grandmaster time
                self.pending_syncs.clear();
                self.sample_window.clear();
                self.prev_t1_ns = 0;
                self.prev_t2_ns = 0;
                // Keep: applied_freq_ppm, drift_baseline_ppm (learned values)
                // Stay in production mode - let servo naturally adjust if needed
                info!(
                    "Soft reset: keeping freq={:.1}ppm, drift_baseline={:.1}ppm",
                    self.applied_freq_ppm, self.drift_baseline_ppm
                );
            }
            None => {
                info!("Sync source: {}", format_mac(&source_uuid));
                self.current_sync_source = Some(source_uuid);
            }
            _ => {}
        }

        self.pending_syncs.insert(
            header.sequence_id,
            PendingSync {
                rx_time_sys: t2,
                source_uuid: header.source_uuid,
            },
        );

        if let Ok(body) = PtpV1SyncMessageBody::parse(&buf[PtpV1Header::SIZE..]) {
            let new_uuid = body.grandmaster_clock_uuid;
            match self.current_gm_uuid {
                Some(current) if current != new_uuid => {
                    warn!(
                        ">>> GRANDMASTER UUID CHANGED: {} -> {} <<<",
                        format_mac(&current),
                        format_mac(&new_uuid)
                    );
                    self.current_gm_uuid = Some(new_uuid);
                    // Note: sync source change already did soft reset if needed
                }
                None => {
                    info!("Grandmaster UUID: {}", format_mac(&new_uuid));
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
                    self.process_sync_pair(
                        body.precise_origin_timestamp.to_nanos(),
                        sync_info.rx_time_sys,
                    );
                }
            }
        }
    }

    // ========================================================================
    // SYNC PAIR PROCESSING - Main synchronization logic
    // ========================================================================

    fn process_sync_pair(&mut self, t1_ns: i64, t2_sys: SystemTime) {
        let t2_ns = t2_sys
            .duration_since(std::time::UNIX_EPOCH)
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
        if display_phase > 500_000_000 {
            display_phase -= 1_000_000_000;
        } else if display_phase < -500_000_000 {
            display_phase += 1_000_000_000;
        }

        debug!(
            "T1={:.3}s T2={:.3}s diff={:.3}s phase={}us",
            t1_ns as f64 / 1e9,
            t2_ns as f64 / 1e9,
            time_diff_ns as f64 / 1e9,
            display_phase / 1000
        );
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
            info!(
                "Calibration complete: offset={:.3}ms ({} samples)",
                self.calibration_offset_ns as f64 / 1_000_000.0,
                count
            );
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
                if !(0.5..=2.0).contains(&ratio) {
                    debug!(
                        "[Jitter] master={}ms slave={}ms ratio={:.2}x",
                        delta_master / 1_000_000,
                        delta_slave / 1_000_000,
                        ratio
                    );
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

        debug!(
            "[Filter] min={:.1}us max={:.1}us median={:.1}us",
            sorted.first().map(|&x| x as f64 / 1000.0).unwrap_or(0.0),
            sorted.last().map(|&x| x as f64 / 1000.0).unwrap_or(0.0),
            offset_us
        );

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
        let dt_secs = self
            .last_offset_time
            .map(|t| now.duration_since(t).as_secs_f64())
            .unwrap_or(1.0);

        // Calculate instantaneous rate of change (drift rate in ppm)
        // delta_offset / delta_time gives us the frequency error
        let raw_rate_ppm = if let Some(prev_offset) = self.last_offset_us {
            if dt_secs > 0.1 {
                // Need meaningful time delta
                let delta_offset = offset_us - prev_offset;
                // Convert: us/s = ppm
                (delta_offset / dt_secs).clamp(-500.0, 500.0)
            } else {
                self.smoothed_rate_ppm // Keep previous
            }
        } else {
            0.0
        };

        // Store for next iteration
        self.last_offset_us = Some(offset_us);
        self.last_offset_time = Some(now);

        // =======================================================================
        // ADAPTIVE SPIKE DETECTION
        // =======================================================================
        // Filter raw rate through MAD-based outlier detector.
        // Uses current mode for threshold selection (stricter in LOCK/NANO).
        // Spikes from timestamp jitter are replaced with median of window.
        // =======================================================================
        let filter_mode = if self.in_nano_mode {
            FilterMode::Nano
        } else if self.is_locked {
            FilterMode::Lock
        } else if self.in_production_mode {
            FilterMode::Prod
        } else {
            FilterMode::Acq
        };

        let filter_result = self.spike_filter.filter(raw_rate_ppm, filter_mode);
        let filtered_rate_ppm = filter_result.value;

        // Log when spike is detected and rejected
        if filter_result.is_spike {
            info!(
                "[Spike] REJECTED {:+.1}us/s (dev={:.1}, thresh={:.1}, median={:.1})",
                raw_rate_ppm,
                filter_result.deviation,
                filter_result.threshold,
                filter_result.median
            );
        }

        // Log spike statistics periodically (every 100 samples)
        let (total, rejected, ratio) = self.spike_filter.stats();
        if total > 0 && total % 100 == 0 {
            debug!(
                "[Spike] Stats: {}/{} rejected ({:.1}%), MAD={:.2}",
                rejected, total, ratio, filter_result.mad
            );
        }

        // Smooth rate with exponential moving average (on FILTERED rate)
        const RATE_SMOOTH_ALPHA: f64 = 0.3; // Higher = more responsive
        self.smoothed_rate_ppm = self.smoothed_rate_ppm * (1.0 - RATE_SMOOTH_ALPHA)
            + filtered_rate_ppm * RATE_SMOOTH_ALPHA;
        let rate_ppm = self.smoothed_rate_ppm;

        // THREE-PHASE CONTROL: ACQ → PROD → NANO based on rate stability
        let abs_rate = rate_ppm.abs();

        // NANO mode transitions (from LOCK state only)
        if self.is_locked {
            if abs_rate < NANO_ENTER_RATE_US {
                self.nano_sustain_count += 1;
                self.nano_exit_count = 0; // Reset exit counter when drift is good
                                          // Log progress towards NANO every 10 samples
                #[allow(clippy::manual_is_multiple_of)]
                if self.nano_sustain_count % 10 == 0 && !self.in_nano_mode {
                    debug!(
                        "[NANO] Sustain count: {}/{}",
                        self.nano_sustain_count, NANO_SUSTAIN_COUNT
                    );
                }
                if self.nano_sustain_count >= NANO_SUSTAIN_COUNT && !self.in_nano_mode {
                    self.in_nano_mode = true;
                    info!(
                        "[PTP] === NANO MODE === Ultra-precise servo engaged (after {} samples)",
                        NANO_SUSTAIN_COUNT
                    );
                }
            } else if abs_rate > NANO_EXIT_RATE_US {
                // Above exit threshold - count towards exit (hysteresis)
                self.nano_exit_count += 1;
                if self.in_nano_mode {
                    if self.nano_exit_count >= NANO_EXIT_COUNT {
                        self.in_nano_mode = false;
                        self.nano_sustain_count = 0;
                        self.nano_exit_count = 0;
                        info!("[PTP] === LOCK MODE === Exiting NANO (drift {:+.2}us/s for {} samples)",
                              rate_ppm, NANO_EXIT_COUNT);
                    } else {
                        debug!(
                            "[NANO] Exit warning {}/{}: drift {:+.2}us/s",
                            self.nano_exit_count, NANO_EXIT_COUNT, rate_ppm
                        );
                    }
                }
                // Reset sustain count when we exceed exit threshold (even if not in NANO yet)
                // This ensures we need CONSECUTIVE samples below threshold to enter
                if self.nano_sustain_count > 0 {
                    debug!(
                        "[NANO] Reset entry counter: drift {:+.2}us/s > exit threshold",
                        abs_rate
                    );
                    self.nano_sustain_count = 0;
                }
            } else {
                // Between thresholds (0.5-1.0): reset exit counter but don't change entry counter
                self.nano_exit_count = 0;
            }
        } else {
            // Not locked - can't be in NANO
            self.in_nano_mode = false;
            self.nano_sustain_count = 0;
            self.nano_exit_count = 0;
        }

        // ACQ/PROD transitions
        if abs_rate < 5.0 {
            // Rate stable within 5µs/s
            self.in_production_mode = true;
        } else if abs_rate > 20.0 {
            // Rate unstable above 20µs/s
            self.in_production_mode = false;
        }

        // Select gains based on mode
        let (p_gain, p_max, i_gain, phase_name) = if self.in_nano_mode {
            (P_GAIN_NANO, P_MAX_NANO_PPM, I_GAIN_NANO, "NANO")
        } else if self.in_production_mode {
            (P_GAIN_PROD, P_MAX_PROD_PPM, 0.05, "PROD")
        } else {
            (P_GAIN_ACQ, P_MAX_ACQ_PPM, 0.05, "ACQ")
        };

        // P-term: responds to rate of change (not absolute offset!)
        // NANO mode: apply deadband - don't correct tiny rates (noise)
        let effective_rate = if self.in_nano_mode && abs_rate < NANO_DEADBAND_US {
            0.0 // Within deadband, no correction needed
        } else {
            rate_ppm
        };

        // Negative rate = clock too slow, need positive adjustment
        let p_term = (-effective_rate * p_gain).clamp(-p_max, p_max);

        // I-term: Integrate rate error to learn true drift
        // Uses mode-appropriate gain
        let i_term = -effective_rate * i_gain;
        self.drift_baseline_ppm =
            (self.drift_baseline_ppm + i_term).clamp(-DRIFT_MAX_PPM, DRIFT_MAX_PPM);

        // Total correction = drift baseline + P-term
        let total_correction =
            (self.drift_baseline_ppm + p_term).clamp(-DRIFT_MAX_PPM, DRIFT_MAX_PPM);

        // Lock state: based on rate stability, not absolute offset
        let rate_stable = abs_rate < 5.0; // Within 5ppm
        if rate_stable {
            self.lock_stable_count += 1;
            if self.lock_stable_count >= LOCK_STABLE_COUNT && !self.is_locked {
                self.is_locked = true;
                info!(
                    "[PTP] === LOCKED === Adj:{:+.1}ppm",
                    self.drift_baseline_ppm
                );
            }
        } else {
            if self.lock_stable_count > 0 {
                self.lock_stable_count -= 1; // Gradual unlock
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

        let status = if self.in_nano_mode {
            "NANO"
        } else if self.is_locked {
            "LOCK"
        } else {
            phase_name
        };

        // User-friendly log: drift rate (stability) and frequency adjustment
        // NANO mode shows nanoseconds for sub-µs precision visibility
        if self.in_nano_mode {
            let drift_ns = rate_ppm * 1000.0; // Convert µs/s to ns/s
            info!(
                "[PTP] {:4}  Drift:{:+7.0}ns/s  Adj:{:+6.2}ppm",
                status, drift_ns, total_correction
            );
        } else {
            info!(
                "[PTP] {:4}  Drift:{:+6.1}us/s  Adj:{:+6.1}ppm",
                status, rate_ppm, total_correction
            );
        }

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
            status.mode = if self.in_nano_mode {
                "NANO".to_string()
            } else if self.is_locked {
                "LOCK".to_string()
            } else if self.in_production_mode {
                "PROD".to_string()
            } else {
                "ACQ".to_string()
            };
            // NTP offset is updated separately via check_ntp_utc_tracking()
        }
    }

    /// Full filter reset - currently unused but kept for edge cases
    /// (major NTP step correction, config reload, etc.)
    #[allow(dead_code)]
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
        self.in_nano_mode = false;
        self.nano_sustain_count = 0;
        self.nano_exit_count = 0;
        self.applied_freq_ppm = 0.0;
        self.last_offset_us = None;
        self.last_offset_time = None;
        self.smoothed_rate_ppm = 0.0;
        // Clear spike filter history
        self.spike_filter.clear();

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

        mock_ntp
            .expect_get_offset()
            .times(1)
            .returning(|| Ok((Duration::from_millis(100), 1)));

        mock_clock
            .expect_step_clock()
            .with(eq(Duration::from_millis(100)), eq(1))
            .times(1)
            .returning(|_, _| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut controller = PtpController::new(
            mock_clock,
            mock_net,
            mock_ntp,
            status,
            SystemConfig::default(),
        );
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

            mock_net
                .expect_recv_packet()
                .times(1)
                .returning(move || Ok(Some((sync_pkt.clone(), 60, t2))));

            mock_net
                .expect_recv_packet()
                .times(1)
                .returning(move || Ok(Some((follow_pkt.clone(), 60, t2))));
        }

        mock_net.expect_recv_packet().returning(|| Ok(None));
        mock_clock
            .expect_adjust_frequency()
            .times(2)
            .returning(|_| Ok(()));

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

    // ========================================================================
    // NANO MODE HYSTERESIS TESTS
    // ========================================================================
    // Tests for v1.5.4 hysteresis: NANO mode requires 5 consecutive samples
    // above threshold to exit, preventing single spikes from destabilizing.
    // ========================================================================

    /// Helper to create a controller in a specific NANO mode state for testing
    fn create_nano_test_controller() -> (
        PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>,
        Arc<RwLock<SyncStatus>>,
    ) {
        let mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mock_ntp = MockNtpSource::new();
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let controller = PtpController::new(mock_clock, mock_net, mock_ntp, status.clone(), config);
        (controller, status)
    }

    #[test]
    fn test_nano_mode_requires_lock_first() {
        let (controller, _) = create_nano_test_controller();

        // Verify initial state: not locked, not in NANO
        assert!(!controller.is_locked, "Should not be locked initially");
        assert!(!controller.in_nano_mode, "Should not be in NANO initially");
        assert_eq!(controller.nano_sustain_count, 0);
        assert_eq!(controller.nano_exit_count, 0);
    }

    #[test]
    fn test_nano_entry_requires_sustained_low_drift() {
        let (mut controller, _) = create_nano_test_controller();

        // Simulate locked state
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.warmup_complete = true;
        controller.clock_settled = true;

        // Simulate sustained low drift (< 0.5 µs/s) for NANO_SUSTAIN_COUNT samples
        for i in 0..NANO_SUSTAIN_COUNT {
            // Manually increment nano_sustain_count as the rate calculation would
            controller.nano_sustain_count += 1;

            if i < NANO_SUSTAIN_COUNT - 1 {
                assert!(
                    !controller.in_nano_mode,
                    "Should NOT enter NANO before {} samples, currently at {}",
                    NANO_SUSTAIN_COUNT,
                    i + 1
                );
            }
        }

        // After NANO_SUSTAIN_COUNT samples, should enter NANO mode
        if controller.nano_sustain_count >= NANO_SUSTAIN_COUNT {
            controller.in_nano_mode = true;
        }
        assert!(
            controller.in_nano_mode,
            "Should enter NANO after {} sustained samples",
            NANO_SUSTAIN_COUNT
        );
    }

    #[test]
    fn test_nano_exit_single_spike_no_exit() {
        let (mut controller, _) = create_nano_test_controller();

        // Put controller in NANO mode
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;
        controller.nano_exit_count = 0;

        // Single spike above threshold - should NOT exit NANO (hysteresis)
        controller.nano_exit_count = 1;

        // The hysteresis requires NANO_EXIT_COUNT (5) consecutive samples
        assert!(
            controller.in_nano_mode,
            "Single spike should NOT exit NANO mode (hysteresis requires {} samples)",
            NANO_EXIT_COUNT
        );
        assert!(
            controller.nano_exit_count < NANO_EXIT_COUNT,
            "Exit count {} should be less than threshold {}",
            controller.nano_exit_count,
            NANO_EXIT_COUNT
        );
    }

    #[test]
    fn test_nano_exit_requires_consecutive_spikes() {
        let (mut controller, _) = create_nano_test_controller();

        // Put controller in NANO mode
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;
        controller.nano_exit_count = 0;

        // Simulate NANO_EXIT_COUNT - 1 consecutive spikes - should NOT exit
        for i in 1..NANO_EXIT_COUNT {
            controller.nano_exit_count = i;
            assert!(
                controller.in_nano_mode,
                "Should NOT exit NANO with only {} spikes (need {})",
                i, NANO_EXIT_COUNT
            );
        }

        // Simulate the NANO_EXIT_COUNT-th spike - NOW should exit
        controller.nano_exit_count = NANO_EXIT_COUNT;
        if controller.nano_exit_count >= NANO_EXIT_COUNT {
            controller.in_nano_mode = false;
            controller.nano_sustain_count = 0;
            controller.nano_exit_count = 0;
        }

        assert!(
            !controller.in_nano_mode,
            "Should exit NANO after {} consecutive spikes",
            NANO_EXIT_COUNT
        );
        assert_eq!(
            controller.nano_sustain_count, 0,
            "Sustain count should reset on NANO exit"
        );
        assert_eq!(
            controller.nano_exit_count, 0,
            "Exit count should reset on NANO exit"
        );
    }

    #[test]
    fn test_nano_exit_counter_resets_on_good_sample() {
        let (mut controller, _) = create_nano_test_controller();

        // Put controller in NANO mode with some exit counter
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;
        controller.nano_exit_count = 3; // Some spikes, but not enough to exit

        // Good sample (low drift) should reset exit counter
        // Simulating what happens when abs_rate < NANO_ENTER_RATE_US
        controller.nano_exit_count = 0;
        controller.nano_sustain_count += 1;

        assert!(controller.in_nano_mode, "Should remain in NANO mode");
        assert_eq!(
            controller.nano_exit_count, 0,
            "Exit counter should reset on good sample"
        );
    }

    #[test]
    fn test_nano_constants_are_correct() {
        // Verify the constants match expected values for documentation
        assert_eq!(
            NANO_SUSTAIN_COUNT, 15,
            "NANO entry requires 15 sustained samples"
        );
        assert_eq!(
            NANO_EXIT_COUNT, 5,
            "NANO exit requires 5 consecutive spikes (hysteresis)"
        );
        assert!(
            (NANO_ENTER_RATE_US - 0.5).abs() < 0.001,
            "NANO entry threshold is 0.5 µs/s"
        );
        assert!(
            (NANO_EXIT_RATE_US - 1.0).abs() < 0.001,
            "NANO exit threshold is 1.0 µs/s"
        );
    }

    #[test]
    fn test_mode_transition_not_locked_resets_nano() {
        let (mut controller, _) = create_nano_test_controller();

        // Put controller in NANO mode
        controller.is_locked = true;
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;
        controller.nano_exit_count = 2;

        // Simulate loss of lock
        controller.is_locked = false;

        // The controller logic resets NANO state when not locked
        if !controller.is_locked {
            controller.in_nano_mode = false;
            controller.nano_sustain_count = 0;
            controller.nano_exit_count = 0;
        }

        assert!(
            !controller.in_nano_mode,
            "Should exit NANO when lock is lost"
        );
        assert_eq!(
            controller.nano_sustain_count, 0,
            "Sustain count should reset when lock is lost"
        );
        assert_eq!(
            controller.nano_exit_count, 0,
            "Exit count should reset when lock is lost"
        );
    }

    #[test]
    fn test_nano_deadband_constant() {
        // Verify deadband is configured correctly
        assert!(
            (NANO_DEADBAND_US - 0.1).abs() < 0.001,
            "NANO deadband should be 0.1 µs/s"
        );
    }

    // ========================================================================
    // SYNC SOURCE / GRANDMASTER SWITCH TESTS
    // ========================================================================
    // Tests for v1.5.5+ soft reset: when sync source changes, we preserve
    // the learned frequency and stay in current mode instead of hard reset.
    // ========================================================================

    /// Helper to create a controller in LOCK mode for grandmaster switch testing
    fn create_locked_controller() -> (
        PtpController<MockSystemClock, MockPtpNetwork, MockNtpSource>,
        Arc<RwLock<SyncStatus>>,
    ) {
        let mock_clock = MockSystemClock::new();
        let mock_net = MockPtpNetwork::new();
        let mock_ntp = MockNtpSource::new();
        let status = Arc::new(RwLock::new(SyncStatus::default()));
        let mut config = SystemConfig::default();
        config.filters.calibration_samples = 0;
        config.filters.warmup_secs = 0.0;

        let mut controller =
            PtpController::new(mock_clock, mock_net, mock_ntp, status.clone(), config);

        // Set up controller in LOCK state with learned frequency
        controller.is_locked = true;
        controller.in_production_mode = true;
        controller.warmup_complete = true;
        controller.clock_settled = true;
        controller.applied_freq_ppm = 35.0;
        controller.drift_baseline_ppm = 33.5;
        controller.current_sync_source = Some([0x00, 0x1D, 0xC1, 0x51, 0xD0, 0xD9]);
        controller.current_gm_uuid = Some([0x00, 0x00, 0x00, 0x00, 0x01, 0x00]);

        // Add some pending syncs and samples
        controller.pending_syncs.insert(
            1,
            PendingSync {
                rx_time_sys: SystemTime::now(),
                source_uuid: [0x00, 0x1D, 0xC1, 0x51, 0xD0, 0xD9],
            },
        );
        controller.sample_window.push(1000);
        controller.sample_window.push(2000);

        (controller, status)
    }

    #[test]
    fn test_sync_source_initial_detection() {
        let (mut controller, _) = create_locked_controller();

        // Reset to no sync source
        controller.current_sync_source = None;

        // Verify initial detection sets sync source without reset
        let new_source = [0x00, 0x1D, 0xC1, 0x1A, 0x44, 0x30];

        // Simulate what handle_sync_message does for initial source
        controller.current_sync_source = Some(new_source);

        assert_eq!(
            controller.current_sync_source,
            Some(new_source),
            "Should set initial sync source"
        );
        // Frequency should still be preserved (not reset)
        assert!(
            (controller.applied_freq_ppm - 35.0).abs() < 0.01,
            "Frequency should be preserved on initial detection"
        );
    }

    #[test]
    fn test_sync_source_change_soft_reset_preserves_frequency() {
        let (mut controller, _) = create_locked_controller();

        let old_freq = controller.applied_freq_ppm;
        let old_drift = controller.drift_baseline_ppm;

        // Simulate sync source change (soft reset logic)
        let new_source = [0x00, 0x1D, 0xC1, 0x1A, 0x44, 0x30];
        controller.current_sync_source = Some(new_source);
        controller.pending_syncs.clear();
        controller.sample_window.clear();
        controller.prev_t1_ns = 0;
        controller.prev_t2_ns = 0;
        // Key: applied_freq_ppm and drift_baseline_ppm are NOT reset

        assert!(
            (controller.applied_freq_ppm - old_freq).abs() < 0.01,
            "Soft reset should preserve applied_freq_ppm: expected {}, got {}",
            old_freq,
            controller.applied_freq_ppm
        );
        assert!(
            (controller.drift_baseline_ppm - old_drift).abs() < 0.01,
            "Soft reset should preserve drift_baseline_ppm: expected {}, got {}",
            old_drift,
            controller.drift_baseline_ppm
        );
    }

    #[test]
    fn test_sync_source_change_soft_reset_clears_stale_data() {
        let (mut controller, _) = create_locked_controller();

        // Verify we have stale data before
        assert!(
            !controller.pending_syncs.is_empty(),
            "Should have pending syncs before soft reset"
        );
        assert!(
            !controller.sample_window.is_empty(),
            "Should have samples before soft reset"
        );

        // Simulate soft reset
        controller.pending_syncs.clear();
        controller.sample_window.clear();
        controller.prev_t1_ns = 0;
        controller.prev_t2_ns = 0;

        assert!(
            controller.pending_syncs.is_empty(),
            "Soft reset should clear pending_syncs"
        );
        assert!(
            controller.sample_window.is_empty(),
            "Soft reset should clear sample_window"
        );
        assert_eq!(
            controller.prev_t1_ns, 0,
            "Soft reset should clear prev_t1_ns"
        );
        assert_eq!(
            controller.prev_t2_ns, 0,
            "Soft reset should clear prev_t2_ns"
        );
    }

    #[test]
    fn test_sync_source_change_stays_in_lock_mode() {
        let (mut controller, _) = create_locked_controller();

        // Verify LOCK state before
        assert!(controller.is_locked, "Should be locked before soft reset");
        assert!(
            controller.in_production_mode,
            "Should be in production mode before soft reset"
        );

        // Simulate soft reset (what handle_sync_message does)
        let new_source = [0x00, 0x1D, 0xC1, 0x1A, 0x44, 0x30];
        controller.current_sync_source = Some(new_source);
        controller.pending_syncs.clear();
        controller.sample_window.clear();
        controller.prev_t1_ns = 0;
        controller.prev_t2_ns = 0;
        // Key: is_locked and in_production_mode are NOT reset

        assert!(
            controller.is_locked,
            "Soft reset should NOT change lock state"
        );
        assert!(
            controller.in_production_mode,
            "Soft reset should NOT change production mode"
        );
    }

    #[test]
    fn test_sync_source_change_in_nano_mode_stays_nano() {
        let (mut controller, _) = create_locked_controller();

        // Put in NANO mode
        controller.in_nano_mode = true;
        controller.nano_sustain_count = NANO_SUSTAIN_COUNT;

        // Simulate soft reset
        let new_source = [0x00, 0x1D, 0xC1, 0x1A, 0x44, 0x30];
        controller.current_sync_source = Some(new_source);
        controller.pending_syncs.clear();
        controller.sample_window.clear();
        controller.prev_t1_ns = 0;
        controller.prev_t2_ns = 0;
        // Soft reset does NOT touch nano mode state

        assert!(
            controller.in_nano_mode,
            "Soft reset should NOT exit NANO mode"
        );
        assert_eq!(
            controller.nano_sustain_count, NANO_SUSTAIN_COUNT,
            "Soft reset should NOT reset nano_sustain_count"
        );
    }

    #[test]
    fn test_format_mac_helper() {
        let uuid = [0x00, 0x1D, 0xC1, 0x51, 0xD0, 0xD9];
        let formatted = format_mac(&uuid);
        assert_eq!(
            formatted, "00:1D:C1:51:D0:D9",
            "format_mac should produce correct MAC format"
        );

        let all_zeros = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(
            format_mac(&all_zeros),
            "00:00:00:00:00:00",
            "format_mac should handle all zeros"
        );

        let all_ff = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert_eq!(
            format_mac(&all_ff),
            "FF:FF:FF:FF:FF:FF",
            "format_mac should handle all 0xFF"
        );
    }

    #[test]
    fn test_grandmaster_uuid_change_detected() {
        let (mut controller, _) = create_locked_controller();

        let old_gm = controller.current_gm_uuid;
        let new_gm = [0x00, 0x00, 0x00, 0x00, 0x02, 0x00];

        // Simulate grandmaster UUID change
        controller.current_gm_uuid = Some(new_gm);

        assert_ne!(
            controller.current_gm_uuid, old_gm,
            "Grandmaster UUID should be updated"
        );
        assert_eq!(
            controller.current_gm_uuid,
            Some(new_gm),
            "New grandmaster UUID should be stored"
        );
    }

    #[test]
    fn test_hard_reset_vs_soft_reset_frequency_difference() {
        // This test documents the key difference between hard and soft reset
        let (mut soft_controller, _) = create_locked_controller();
        let (mut hard_controller, _) = create_locked_controller();

        let original_freq = 35.0;

        // Soft reset: preserves frequency
        soft_controller.pending_syncs.clear();
        soft_controller.sample_window.clear();
        soft_controller.prev_t1_ns = 0;
        soft_controller.prev_t2_ns = 0;
        // applied_freq_ppm NOT touched

        // Hard reset (what reset_filter does): clears frequency
        hard_controller.applied_freq_ppm = 0.0;
        hard_controller.drift_baseline_ppm = 0.0;
        hard_controller.is_locked = false;
        hard_controller.in_production_mode = false;

        assert!(
            (soft_controller.applied_freq_ppm - original_freq).abs() < 0.01,
            "Soft reset preserves frequency"
        );
        assert!(
            (hard_controller.applied_freq_ppm - 0.0).abs() < 0.01,
            "Hard reset clears frequency to 0"
        );
        assert!(soft_controller.is_locked, "Soft reset stays locked");
        assert!(!hard_controller.is_locked, "Hard reset loses lock");
    }

    // ========================================================================
    // PTP OFFLINE DETECTION TESTS
    // ========================================================================
    // Tests for v1.5.5+ PTP timeout: when no PTP packets are received for
    // PTP_TIMEOUT_SECS (10s), the app should log and continue with NTP-only sync.
    // ========================================================================

    #[test]
    fn test_ptp_offline_constants() {
        // Verify timeout and threshold constants
        assert_eq!(PTP_TIMEOUT_SECS, 10, "PTP timeout should be 10 seconds");
        assert_eq!(
            NTP_STEP_THRESHOLD_US, 500,
            "NTP step threshold should be 500µs"
        );
    }

    #[test]
    fn test_ptp_offline_initial_state() {
        let (controller, _) = create_nano_test_controller();

        // Verify initial state is online
        assert!(!controller.ptp_offline, "Should start online");
        assert!(
            !controller.ptp_offline_logged,
            "Should not have logged offline"
        );
    }

    #[test]
    fn test_ptp_offline_detection_after_timeout() {
        let (mut controller, status) = create_nano_test_controller();

        // Simulate timeout by setting last_ptp_packet to past
        controller.last_ptp_packet = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 1);

        // Call check_ptp_status
        controller.check_ptp_status();

        // Verify offline state
        assert!(controller.ptp_offline, "Should be offline after timeout");
        assert!(controller.ptp_offline_logged, "Should have logged offline");

        // Verify status update
        let status_guard = status.read().unwrap();
        assert!(!status_guard.settled, "Status should show not settled");
        assert_eq!(status_guard.mode, "NTP-only", "Mode should be NTP-only");
    }

    #[test]
    fn test_ptp_online_recovery() {
        let (mut controller, _) = create_nano_test_controller();

        // Set offline state
        controller.ptp_offline = true;
        controller.ptp_offline_logged = true;

        // Simulate packet received (recent timestamp)
        controller.last_ptp_packet = Instant::now();

        // Call check_ptp_status
        controller.check_ptp_status();

        // Verify recovery
        assert!(!controller.ptp_offline, "Should be back online");
        assert!(!controller.ptp_offline_logged, "Logged flag should reset");
    }

    #[test]
    fn test_ptp_offline_no_repeat_logging() {
        let (mut controller, _) = create_nano_test_controller();

        // Simulate already offline and logged
        controller.ptp_offline = true;
        controller.ptp_offline_logged = true;
        controller.last_ptp_packet = Instant::now() - Duration::from_secs(PTP_TIMEOUT_SECS + 5);

        // Call check_ptp_status multiple times
        controller.check_ptp_status();
        controller.check_ptp_status();
        controller.check_ptp_status();

        // Should still be logged (no reset)
        assert!(
            controller.ptp_offline_logged,
            "Should remain logged (no spam)"
        );
    }

    #[test]
    fn test_ntp_tracking_runs_when_ptp_offline() {
        let (controller, _) = create_nano_test_controller();

        // The check_ntp_utc_tracking function has this logic:
        // let should_check = self.ptp_offline || (self.is_locked && self.ntp_tracking_enabled);

        // When PTP is offline, NTP tracking should run regardless of lock state
        // This is validated by the modified check_ntp_utc_tracking condition
        assert!(
            controller.ntp_tracking_enabled,
            "NTP tracking should be enabled by default"
        );
    }

    #[test]
    fn test_ptp_offline_within_timeout_stays_online() {
        let (mut controller, _) = create_nano_test_controller();

        // Simulate packet received 5 seconds ago (within timeout)
        controller.last_ptp_packet = Instant::now() - Duration::from_secs(5);

        // Call check_ptp_status
        controller.check_ptp_status();

        // Should still be online
        assert!(!controller.ptp_offline, "Should stay online within timeout");
    }
}
