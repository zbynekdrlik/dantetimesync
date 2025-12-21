use anyhow::Result;
use log::{info, warn, error, debug};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};
use std::sync::{Arc, RwLock};
use crate::clock::SystemClock;
use crate::traits::{NtpSource, PtpNetwork};
use crate::ptp::{PtpV1Header, PtpV1Control, PtpV1FollowUpBody, PtpV1SyncMessageBody};
use crate::status::SyncStatus;
use crate::config::SystemConfig;
#[cfg(unix)]
use crate::rtc;

const MAX_DELTA_NS: i64 = 2_000_000_000;   // 2s (Hard safety limit)
const RTC_UPDATE_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes
const NTP_CHECK_INTERVAL: Duration = Duration::from_secs(60);   // Check NTP every 60 seconds
const NTP_BIAS_MAX_PPM: f64 = 100.0;  // Max bias to correct NTP drift (gentle correction)
const NTP_BIAS_THRESHOLD_MS: f64 = 5.0;  // Start correcting if NTP offset > 5ms

// Two-phase sync parameters - tuned for Windows timestamp jitter
const ACQUISITION_FILTER_WEIGHT: f64 = 0.2;  // Slower filter: less reactive to jitter
const PRODUCTION_FILTER_WEIGHT: f64 = 0.05; // Very slow filter for production stability
const ACQUISITION_MAX_PPM: f64 = 100.0;      // Moderate during acquisition
const PRODUCTION_MAX_PPM: f64 = 20.0;        // Very gentle during production
const ACQUISITION_STABLE_COUNT: usize = 3;   // Require 3 stable samples to switch to production
const ACQUISITION_STABLE_THRESHOLD_PPM: f64 = 20.0;  // Allow more variation due to jitter

pub struct PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource
{
    clock: C,
    network: N,
    ntp: S,
    config: SystemConfig,

    // State
    pending_syncs: HashMap<u16, PendingSync>,
    prev_t1_ns: i64,
    prev_t2_ns: i64,
    current_gm_uuid: Option<[u8; 6]>,

    // Filtering
    sample_window: Vec<i64>,

    // Metrics
    last_phase_offset_ns: i64,
    last_adj_ppm: f64,

    // Epoch Alignment
    initial_epoch_offset_ns: i64, // t2 - t1 at first lock
    epoch_aligned: bool,

    // RTC
    last_rtc_update: Instant,

    valid_count: usize,
    clock_settled: bool,
    settling_threshold: usize,

    // Status for IPC/Tray
    status_shared: Arc<RwLock<SyncStatus>>,

    // Timestamp Calibration (Windows only - compensates for pcap timestamp offset)
    calibration_samples: Vec<i64>,
    calibration_offset_ns: i64,
    calibration_complete: bool,

    // Phase unwrapping (handles second-boundary crossing artifacts)
    prev_raw_phase_ns: i64,
    phase_unwrap_correction_ns: i64,

    // Drift-based frequency lock (replaces PI servo)
    prev_offset_ns: i64,
    prev_offset_time: Instant,
    measured_drift_ppm: f64,      // Current measured drift rate
    applied_freq_ppm: f64,        // Currently applied frequency correction

    // NTP bias correction (keeps UTC in sync)
    last_ntp_check: Instant,
    ntp_bias_ppm: f64,            // Bias to slowly correct NTP drift
    last_ntp_offset_ms: f64,      // Last measured NTP offset

    // Two-phase sync (acquisition -> production)
    in_production_mode: bool,
    stable_drift_count: usize,    // Count of stable drift samples
    last_drift_for_stability: f64,
}

struct PendingSync {
    rx_time_sys: SystemTime,
    source_uuid: [u8; 6],
}

impl<C, N, S> PtpController<C, N, S>
where
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource
{
    pub fn new(clock: C, network: N, ntp: S, status_shared: Arc<RwLock<SyncStatus>>, config: SystemConfig) -> Self {
        let window_size = config.filters.sample_window_size;

        // Determine if calibration is needed (config.filters.calibration_samples > 0)
        let calibration_count = config.filters.calibration_samples;
        let calibration_complete = calibration_count == 0;

        // Log configuration at startup
        info!("=== PTP Controller Initialization ===");
        info!("Mode: Dante frequency lock (NTP bias DISABLED)");
        info!("Filter Config: WindowSize={}, MinDelta={}ns",
              config.filters.sample_window_size,
              config.filters.min_delta_ns);
        info!("Two-Phase Sync: Acquisition(max {} PPM, filter {}) -> Production(max {} PPM, filter {})",
              ACQUISITION_MAX_PPM, ACQUISITION_FILTER_WEIGHT, PRODUCTION_MAX_PPM, PRODUCTION_FILTER_WEIGHT);
        info!("Calibration: {} samples ({})",
              config.filters.calibration_samples,
              if config.filters.calibration_samples > 0 { "enabled" } else { "disabled" });
        info!("PTP Stepping: {}", if config.filters.ptp_stepping_enabled { "enabled" } else { "DISABLED (frequency-only sync)" });
        info!("=== PTP Controller Ready ===");

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
            last_rtc_update: now,
            valid_count: 0,
            clock_settled: false,
            settling_threshold: 1,
            status_shared,
            calibration_samples: Vec::with_capacity(calibration_count),
            calibration_offset_ns: 0,
            calibration_complete,
            prev_raw_phase_ns: 0,
            phase_unwrap_correction_ns: 0,
            // New drift-based fields
            prev_offset_ns: 0,
            prev_offset_time: now,
            measured_drift_ppm: 0.0,
            applied_freq_ppm: 0.0,
            // NTP bias fields
            last_ntp_check: now,
            ntp_bias_ppm: 0.0,
            last_ntp_offset_ms: 0.0,
            // Two-phase sync
            in_production_mode: false,
            stable_drift_count: 0,
            last_drift_for_stability: 0.0,
        }
    }

    pub fn get_status_shared(&self) -> Arc<RwLock<SyncStatus>> {
        self.status_shared.clone()
    }

    fn update_shared_status(&self) {
        if let Ok(mut status) = self.status_shared.write() {
            status.offset_ns = self.last_phase_offset_ns;
            status.drift_ppm = self.last_adj_ppm;
            status.gm_uuid = self.current_gm_uuid;
            status.settled = self.clock_settled;
            status.updated_ts = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
        }
    }

    pub fn run_ntp_sync(&mut self, skip: bool) {
        if skip { return; }
        
        match self.ntp.get_offset() {
            Ok((offset, sign)) => {
                let sign_str = if sign > 0 { "+" } else { "-" };
                info!("NTP Sync Successful. Offset: {}{:?}", sign_str, offset);
                
                if offset.as_millis() > 50 {
                    info!("Stepping clock (NTP)...");
                    if let Err(e) = self.clock.step_clock(offset, sign) {
                        error!("Failed to step clock: {}", e);
                    } else {
                        info!("Clock stepped successfully.");
                    }
                } else {
                    info!("Clock offset small, skipping step.");
                }
            }
            Err(e) => {
                warn!("NTP Sync failed: {}", e);
            }
        }
    }

    pub fn log_status(&self) {
        // Also ensure shared status is up to date periodically
        self.update_shared_status();

        if !self.calibration_complete {
            info!("[Status] Calibrating timestamp offset... ({}/{} samples)",
                  self.calibration_samples.len(), self.config.filters.calibration_samples);
            return;
        }

        if !self.clock_settled {
            info!("[Status] Settling... ({}/{}) Waiting for valid PTP pairs...", self.valid_count, self.settling_threshold);
        } else {
            let phase_offset_us = self.last_phase_offset_ns as f64 / 1_000.0;
            let action_str = if self.last_adj_ppm.abs() < 0.01 {
                "Locked (Stable)".to_string()
            } else if self.last_adj_ppm > 0.0 {
                format!("Speeding up ({:+.3} ppm)", self.last_adj_ppm)
            } else {
                format!("Slowing down ({:+.3} ppm)", self.last_adj_ppm)
            };

            let factor = 1.0 + (self.last_adj_ppm / 1_000_000.0);

            info!("[Status] {} | PTP Offset: {:.1}us | Drift: {:.1}ppm | Correction: {:.1}ppm | Factor: {:.9}",
                action_str, phase_offset_us, self.measured_drift_ppm, self.last_adj_ppm, factor);
        }
    }

    /// Check NTP offset and update bias correction
    pub fn check_ntp_offset(&mut self) {
        if self.last_ntp_check.elapsed() < NTP_CHECK_INTERVAL {
            return;
        }
        self.last_ntp_check = Instant::now();

        match self.ntp.get_offset() {
            Ok((offset, sign)) => {
                let offset_ms = offset.as_secs_f64() * 1000.0 * sign as f64;
                self.last_ntp_offset_ms = offset_ms;

                // Calculate bias to slowly correct NTP drift
                // Positive offset means we're ahead, need to slow down (negative bias)
                // Negative offset means we're behind, need to speed up (positive bias)
                if offset_ms.abs() > NTP_BIAS_THRESHOLD_MS {
                    // Proportional bias based on offset magnitude
                    // Scale: 10ms offset → ~10 PPM bias (will correct in ~1000 seconds)
                    let raw_bias = -offset_ms * 1.0;  // 1 PPM per ms of offset
                    self.ntp_bias_ppm = raw_bias.clamp(-NTP_BIAS_MAX_PPM, NTP_BIAS_MAX_PPM);

                    info!("[NTP Check] Offset: {:.1}ms, Applying bias: {:+.1} PPM to correct UTC drift",
                          offset_ms, self.ntp_bias_ppm);
                } else {
                    // Within threshold, reduce bias gradually
                    self.ntp_bias_ppm *= 0.5;
                    if self.ntp_bias_ppm.abs() < 0.1 {
                        self.ntp_bias_ppm = 0.0;
                    }
                    debug!("[NTP Check] Offset: {:.1}ms (within threshold), bias: {:+.1} PPM",
                           offset_ms, self.ntp_bias_ppm);
                }
            }
            Err(e) => {
                warn!("[NTP Check] Failed to get NTP offset: {}. Keeping current bias: {:+.1} PPM",
                      e, self.ntp_bias_ppm);
            }
        }
    }

    fn update_rtc(&mut self) {
        if self.last_rtc_update.elapsed() > RTC_UPDATE_INTERVAL {
            self.perform_rtc_update();
            self.last_rtc_update = Instant::now();
        }
    }
    
    fn perform_rtc_update(&self) {
        #[cfg(unix)]
        {
            info!("Updating RTC hardware clock (via ioctl)...");
            if let Err(e) = rtc::update_rtc(SystemTime::now()) {
                warn!("Failed to update RTC: {}", e);
            } else {
                info!("RTC updated successfully.");
            }
        }
        #[cfg(not(unix))]
        {
            // Windows fallback
        }
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
            PtpV1Control::Sync => {
                self.pending_syncs.insert(header.sequence_id, PendingSync {
                    rx_time_sys: t2,
                    source_uuid: header.source_uuid,
                });

                if let Ok(body) = PtpV1SyncMessageBody::parse(&buf[PtpV1Header::SIZE..size]) {
                    let new_uuid = body.grandmaster_clock_uuid;
                    if let Some(current) = self.current_gm_uuid {
                        if current != new_uuid {
                            info!("Grandmaster changed! Old: {:02X?}, New: {:02X?}", current, new_uuid);
                            self.current_gm_uuid = Some(new_uuid);
                            info!("Resetting filter due to GM change.");
                            self.reset_filter();
                        }
                    } else {
                        info!("Locked to Grandmaster: {:02X?}", new_uuid);
                        self.current_gm_uuid = Some(new_uuid);
                    }
                }
            }
            PtpV1Control::FollowUp => {
                if let Ok(body) = PtpV1FollowUpBody::parse(&buf[PtpV1Header::SIZE..size]) {
                    if let Some(sync_info) = self.pending_syncs.remove(&body.associated_sequence_id) {
                        if sync_info.source_uuid == header.source_uuid {
                            self.handle_sync_pair(body.precise_origin_timestamp.to_nanos(), sync_info.rx_time_sys);
                        }
                    }
                }
            }
            _ => {}
        }
        
        if self.pending_syncs.len() > 100 {
             let now_sys = SystemTime::now();
             self.pending_syncs.retain(|_, v| now_sys.duration_since(v.rx_time_sys).unwrap_or(Duration::ZERO) < Duration::from_secs(5));
        }
        
        Ok(())
    }

    fn handle_sync_pair(&mut self, t1_ns: i64, t2_sys: SystemTime) {
         let t2_ns = t2_sys.duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;

        // Calculate raw phase offset (before calibration)
        let mut raw_phase_offset_ns = (t2_ns % 1_000_000_000) - (t1_ns % 1_000_000_000);
        if raw_phase_offset_ns > 500_000_000 { raw_phase_offset_ns -= 1_000_000_000; }
        else if raw_phase_offset_ns < -500_000_000 { raw_phase_offset_ns += 1_000_000_000; }

        // Debug: Log raw T1, T2, and phase calculation
        debug!("T1={} T2={} T1_mod={} T2_mod={} raw_offset={}us",
               t1_ns, t2_ns,
               t1_ns % 1_000_000_000, t2_ns % 1_000_000_000,
               raw_phase_offset_ns / 1000);

        // Calibration phase: collect samples to measure systematic timestamp offset
        let calibration_count = self.config.filters.calibration_samples;
        if !self.calibration_complete {
            self.calibration_samples.push(raw_phase_offset_ns);

            if self.calibration_samples.len() >= calibration_count {
                // Calculate median as calibration offset (more robust than mean)
                let mut sorted = self.calibration_samples.clone();
                sorted.sort();
                self.calibration_offset_ns = sorted[sorted.len() / 2];
                self.calibration_complete = true;

                info!("Timestamp calibration complete. Systematic offset: {:.3} ms ({} samples, median)",
                      self.calibration_offset_ns as f64 / 1_000_000.0,
                      calibration_count);
            }
            return; // Don't process further during calibration
        }

        // Apply calibration offset to get corrected phase offset
        let calibrated_phase_ns = raw_phase_offset_ns - self.calibration_offset_ns;

        // Phase unwrapping: detect and compensate for second-boundary jumps.
        // When NTP and Dante epochs differ, the phase measurement can jump by ~1 second
        // when timestamps cross second boundaries at different times.
        // This unwrapping gives us a continuous phase signal the servo can track.
        if self.prev_raw_phase_ns != 0 {
            let phase_jump = calibrated_phase_ns - self.prev_raw_phase_ns;
            if phase_jump > 500_000_000 {
                // Positive jump of ~1 second detected, compensate
                self.phase_unwrap_correction_ns -= 1_000_000_000;
                debug!("Phase unwrap: jump of {}ms detected, correction now {}ms",
                       phase_jump / 1_000_000, self.phase_unwrap_correction_ns / 1_000_000);
            } else if phase_jump < -500_000_000 {
                // Negative jump of ~1 second detected, compensate
                self.phase_unwrap_correction_ns += 1_000_000_000;
                debug!("Phase unwrap: jump of {}ms detected, correction now {}ms",
                       phase_jump / 1_000_000, self.phase_unwrap_correction_ns / 1_000_000);
            }
        }
        self.prev_raw_phase_ns = calibrated_phase_ns;

        // Apply unwrap correction for continuous phase
        let phase_offset_ns = calibrated_phase_ns + self.phase_unwrap_correction_ns;

        if self.prev_t1_ns > 0 && self.prev_t2_ns > 0 {
            let delta_master = t1_ns - self.prev_t1_ns;
            let delta_slave = t2_ns - self.prev_t2_ns;
            
            if delta_master < self.config.filters.min_delta_ns || delta_master > MAX_DELTA_NS ||
               delta_slave < self.config.filters.min_delta_ns || delta_slave > MAX_DELTA_NS {
                warn!("Delta out of range. Skipping. Master={}ns, Slave={}ns", delta_master, delta_slave);
                self.prev_t1_ns = t1_ns;
                self.prev_t2_ns = t2_ns;
                return;
            }
        }

        self.valid_count += 1;
        if self.valid_count >= self.settling_threshold {
            if !self.clock_settled {
                self.clock_settled = true;
                self.initial_epoch_offset_ns = t2_ns - t1_ns;
                self.epoch_aligned = true;

                // Step clock for large initial offsets (only if PTP stepping is enabled).
                // On Windows, PTP stepping is disabled because Dante time is not "real" time.
                // Use NTP for absolute time alignment, PTP only for frequency sync.
                if self.config.filters.ptp_stepping_enabled && phase_offset_ns.abs() > self.config.filters.panic_threshold_ns {
                    info!("Initial Phase Offset {}ms is large. Stepping clock to align phase...", phase_offset_ns / 1_000_000);
                    let step_duration = Duration::from_nanos(phase_offset_ns.abs() as u64);
                    let sign = if phase_offset_ns > 0 { -1 } else { 1 };
                    if let Err(e) = self.clock.step_clock(step_duration, sign) {
                        error!("Failed to step clock for phase alignment: {}", e);
                    } else {
                        info!("Phase step complete.");
                        self.reset_filter();
                        // Do NOT reset servo integral. Frequency drift is constant.
                        return;
                    }
                }

                info!("Sync established. Updating RTC...");
                self.update_rtc_now();
            } else {
                // Check for massive drift while settled (only if PTP stepping is enabled)
                if self.config.filters.ptp_stepping_enabled && phase_offset_ns.abs() > self.config.filters.step_threshold_ns {
                     warn!("Large offset {}us detected while settled. Stepping clock (Servo Integral maintained).", phase_offset_ns / 1_000);

                     let step_duration = Duration::from_nanos(phase_offset_ns.abs() as u64);
                     let sign = if phase_offset_ns > 0 { -1 } else { 1 };

                     if let Err(e) = self.clock.step_clock(step_duration, sign) {
                         error!("Failed to step clock for drift correction: {}", e);
                     } else {
                         self.reset_filter();
                         return;
                     }
                }
            }

            // LUCKY PACKET FILTER LOGIC
            self.sample_window.push(phase_offset_ns);

            if self.sample_window.len() >= self.config.filters.sample_window_size {
                // Select the offset with minimum absolute value.
                // This handles second-boundary crossing artifacts where measurements
                // can flip between e.g. +300ms and -700ms due to T1/T2 crossing
                // different second boundaries.

                // Log sample window contents for diagnostics
                let window_us: Vec<f64> = self.sample_window.iter()
                    .map(|&x| x as f64 / 1000.0)
                    .collect();
                let min_us = window_us.iter().cloned().fold(f64::INFINITY, f64::min);
                let max_us = window_us.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let mean_us: f64 = window_us.iter().sum::<f64>() / window_us.len() as f64;
                let spread_us = max_us - min_us;

                // Detect if there's a bimodal distribution (second boundary crossing)
                let bimodal = spread_us > 100_000.0; // > 100ms spread suggests boundary crossing

                debug!("[Window] Samples(us): {:?}", window_us.iter().map(|x| format!("{:.1}", x)).collect::<Vec<_>>());
                debug!("[Window] Min={:.1}us, Max={:.1}us, Mean={:.1}us, Spread={:.1}us {}",
                       min_us, max_us, mean_us, spread_us,
                       if bimodal { "(BIMODAL - boundary crossing detected)" } else { "" });

                if let Some(&lucky_offset) = self.sample_window.iter().min_by_key(|&&x| x.abs()) {
                    let lucky_us = lucky_offset as f64 / 1000.0;
                    let now = Instant::now();

                    self.last_phase_offset_ns = lucky_offset;

                    // DRIFT-BASED FREQUENCY LOCK
                    // Calculate drift rate from consecutive offset measurements
                    let elapsed = now.duration_since(self.prev_offset_time);
                    let elapsed_ns = elapsed.as_nanos() as f64;

                    if self.prev_offset_ns != 0 && elapsed_ns > 0.0 {
                        // Drift = change in offset over time
                        // If offset is increasing (getting more behind), we need to speed up
                        // drift_ppm = (offset_change_ns / elapsed_ns) * 1_000_000
                        let offset_change = lucky_offset - self.prev_offset_ns;
                        let raw_drift_ppm = (offset_change as f64 / elapsed_ns) * 1_000_000.0;

                        // Two-phase filter: aggressive during acquisition, gentle during production
                        let filter_weight = if self.in_production_mode {
                            PRODUCTION_FILTER_WEIGHT
                        } else {
                            ACQUISITION_FILTER_WEIGHT
                        };

                        // Smooth the drift measurement (low-pass filter)
                        self.measured_drift_ppm = filter_weight * raw_drift_ppm + (1.0 - filter_weight) * self.measured_drift_ppm;

                        // Check for stability to switch to production mode
                        if !self.in_production_mode {
                            let drift_change = (self.measured_drift_ppm - self.last_drift_for_stability).abs();
                            if drift_change < ACQUISITION_STABLE_THRESHOLD_PPM {
                                self.stable_drift_count += 1;
                                if self.stable_drift_count >= ACQUISITION_STABLE_COUNT {
                                    self.in_production_mode = true;
                                    info!("[Sync] === SWITCHING TO PRODUCTION MODE === Drift stable at {:.1} PPM",
                                          self.measured_drift_ppm);
                                }
                            } else {
                                self.stable_drift_count = 0; // Reset if not stable
                            }
                            self.last_drift_for_stability = self.measured_drift_ppm;
                        }

                        let mode_str = if self.in_production_mode { "PROD" } else { "ACQ" };
                        debug!("[Drift-{}] OffsetChange={:.1}us, Elapsed={:.1}ms, RawDrift={:.1}ppm, SmoothedDrift={:.1}ppm",
                               mode_str, offset_change as f64 / 1000.0, elapsed.as_secs_f64() * 1000.0,
                               raw_drift_ppm, self.measured_drift_ppm);
                    }

                    // Store current offset for next drift calculation
                    self.prev_offset_ns = lucky_offset;
                    self.prev_offset_time = now;

                    // NTP bias is DISABLED - focus purely on Dante frequency lock
                    // NTP is only used for initial time alignment at startup
                    // self.check_ntp_offset();  // Disabled

                    // FREQUENCY CORRECTION:
                    // Counter the measured drift rate to lock frequency to Dante master
                    let ptp_correction = -self.measured_drift_ppm;
                    let total_correction = ptp_correction;  // No NTP bias

                    // Two-phase clamping: aggressive during acquisition, gentle during production
                    let max_ppm = if self.in_production_mode {
                        PRODUCTION_MAX_PPM
                    } else {
                        ACQUISITION_MAX_PPM
                    };
                    let clamped_correction = total_correction.clamp(-max_ppm, max_ppm);

                    self.last_adj_ppm = clamped_correction;
                    self.applied_freq_ppm = clamped_correction;

                    let factor = 1.0 + (clamped_correction / 1_000_000.0);

                    // Log the correction
                    let mode_str = if self.in_production_mode { "PROD" } else { "ACQ" };
                    info!("[Sync-{}] Offset={:+.1}us | MeasuredDrift={:+.1}ppm | Correction={:+.1}ppm | Factor={:.9}",
                          mode_str, lucky_us, self.measured_drift_ppm, clamped_correction, factor);

                    if let Err(e) = self.clock.adjust_frequency(factor) {
                        warn!("Clock adjustment failed: {}", e);
                    }

                    // Update shared status here (once every N packets)
                    self.update_shared_status();
                }
                self.sample_window.clear();
                self.update_rtc();
            }
        }
        
        self.prev_t1_ns = t1_ns;
        self.prev_t2_ns = t2_ns;
    }
    
    fn update_rtc_now(&mut self) {
        self.perform_rtc_update();
        self.last_rtc_update = Instant::now(); 
    }
    
    fn reset_filter(&mut self) {
        self.valid_count = 0;
        self.clock_settled = false;
        self.prev_t1_ns = 0;
        self.prev_t2_ns = 0;
        self.sample_window.clear();
        // Reset phase unwrapping state
        self.prev_raw_phase_ns = 0;
        self.phase_unwrap_correction_ns = 0;
        // Reset drift tracking
        self.prev_offset_ns = 0;
        self.prev_offset_time = Instant::now();
        self.measured_drift_ppm = 0.0;
        // Reset two-phase state (go back to acquisition)
        self.in_production_mode = false;
        self.stable_drift_count = 0;
        self.last_drift_for_stability = 0.0;

        if let Err(e) = self.network.reset() {
            warn!("Failed to reset network buffers: {}", e);
        }

        self.update_shared_status();
    }
}

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

        // GM UUID
        let gm_uuid = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

        // Helper to construct Sync packet
        let make_sync = move |seq: u16| -> Vec<u8> {
            let mut buf = vec![0u8; 60]; 
            buf[0] = 0x10; // Version 1
            buf[32] = 0x00; // Control: Sync
            buf[22..28].copy_from_slice(&gm_uuid);
            let mut w = &mut buf[30..32];
            w.write_u16::<BigEndian>(seq).unwrap();
            
            // Body starts at 36. GM UUID at 36 + 13 = 49.
            buf[49..55].copy_from_slice(&gm_uuid);
            buf
        };

        // Helper for FollowUp
        let make_followup = move |seq: u16, t1_ns: u64| -> Vec<u8> {
            let mut buf = vec![0u8; 60]; 
            buf[0] = 0x10; // Version 1
            buf[32] = 0x02; // Control: FollowUp
            buf[22..28].copy_from_slice(&gm_uuid);
            let mut w = &mut buf[30..32];
            w.write_u16::<BigEndian>(seq).unwrap();
            
            // Body starts at 36. Assoc Seq at 36 + 6 = 42.
            let mut w = &mut buf[42..44];
            w.write_u16::<BigEndian>(seq).unwrap();
            
            // Timestamp at 44.
            let mut w = &mut buf[44..52];
            let s = (t1_ns / 1_000_000_000) as u32;
            let n = (t1_ns % 1_000_000_000) as u32;
            w.write_u32::<BigEndian>(s).unwrap();
            w.write_u32::<BigEndian>(n).unwrap();
            
            buf
        };

        // Sequence of packets
        for i in 0..8 { 
            let t1 = 1_000_000_000 + i as u64 * 1_000_000_000; 
            let t2 = SystemTime::UNIX_EPOCH + Duration::from_nanos(t1 + 1000); // 1us offset
            
            let sync_pkt = make_sync(i as u16);
            let follow_pkt = make_followup(i as u16, t1);

            mock_net.expect_recv_packet()
                .times(1)
                .returning(move || Ok(Some((sync_pkt.clone(), 60, t2))));
            
            mock_net.expect_recv_packet()
                .times(1)
                .returning(move || Ok(Some((follow_pkt.clone(), 60, t2))));
        }
        
        mock_net.expect_recv_packet()
            .returning(|| Ok(None));

        mock_clock.expect_adjust_frequency()
            .times(2) 
            .returning(|_| Ok(()));

        let status = Arc::new(RwLock::new(SyncStatus::default()));

        // Force window size to 4 for this test, disable calibration
        let mut config = SystemConfig::default();
        config.filters.sample_window_size = 4;
        config.filters.calibration_samples = 0; // Disable calibration for unit test

        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp, status, config);
        
        // Process 16 packets (8 Sync + 8 FollowUp)
        for _ in 0..16 {
            let _ = controller.process_loop_iteration();
        }
        
        // Check settled status
        assert!(controller.get_status_shared().read().unwrap().settled);
    }
}