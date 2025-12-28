//! Adaptive Spike Detection Filter
//!
//! This module implements robust outlier detection for PTP drift rate measurements.
//!
//! ## The Problem
//! Software timestamps on Windows/Linux have jitter from:
//! - OS scheduler preemption
//! - Network interrupt latency
//! - Memory/cache effects
//!
//! This jitter appears as "spikes" in the measured drift rate, but these are NOT
//! real frequency changes. Real crystal oscillator frequency changes are physically
//! limited by thermal inertia (~0.04 ppm/°C, thermal time constant 10-60s).
//!
//! ## The Solution
//! Use Median Absolute Deviation (MAD) for robust outlier detection:
//! - MAD is resistant to outliers (unlike standard deviation)
//! - Self-calibrating to each computer's noise profile
//! - Mode-aware thresholds (stricter in LOCK/NANO modes)
//!
//! ## Algorithm
//! 1. Maintain rolling window of recent drift rate samples
//! 2. Calculate median and MAD of window
//! 3. Sample is "spike" if deviation > k * MAD
//! 4. k varies by mode: permissive in ACQ, strict in NANO
//! 5. Spikes are replaced with median (a real observed value)

use log::debug;
use std::collections::VecDeque;

// ============================================================================
// SPIKE FILTER CONSTANTS
// ============================================================================
// k values based on robust statistics (MAD multipliers):
// k=3: catches 99% of Gaussian outliers
// We use higher values to be conservative and only reject clear spikes

/// ACQ mode: Permissive threshold to allow fast convergence
const K_ACQ: f64 = 4.0;
/// PROD mode: Balanced protection
const K_PROD: f64 = 5.0;
/// LOCK mode: Strict threshold to protect locked state
const K_LOCK: f64 = 6.0;
/// NANO mode: Very strict threshold for ultra-stable operation
const K_NANO: f64 = 8.0;

/// Default rolling window size (~20 seconds at 1 sample/sec)
const DEFAULT_WINDOW_SIZE: usize = 20;
/// Minimum MAD floor to prevent over-sensitivity on ultra-stable systems (µs/s)
const MIN_MAD_FLOOR: f64 = 0.5;
/// Minimum samples before spike detection activates
const WARMUP_SAMPLES: usize = 5;
/// Accept as real step change after this many consecutive "spikes"
const MAX_CONSECUTIVE_SPIKES: usize = 5;

/// Operating mode for threshold selection
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterMode {
    /// Acquisition: Fast convergence, permissive filtering
    Acq,
    /// Production: Balanced filtering
    Prod,
    /// Locked: Protect stability, strict filtering
    Lock,
    /// Nano: Ultra-precise, very strict filtering
    Nano,
}

/// Adaptive spike detection filter using MAD (Median Absolute Deviation)
#[derive(Debug)]
pub struct SpikeFilter {
    /// Rolling window of rate samples (µs/s)
    rate_history: VecDeque<f64>,

    /// Maximum window size
    window_size: usize,

    /// Threshold multipliers for each mode
    /// Higher k = more permissive (fewer rejections)
    k_acq: f64,
    k_prod: f64,
    k_lock: f64,
    k_nano: f64,

    /// Minimum MAD floor to prevent issues on super-stable systems
    min_mad: f64,

    /// Minimum samples before spike detection activates
    warmup_samples: usize,

    /// Consecutive spike counter (for detecting real step changes)
    consecutive_spikes: usize,

    /// Maximum consecutive spikes before accepting as real
    max_consecutive_spikes: usize,

    /// Statistics
    total_samples: u64,
    rejected_spikes: u64,

    /// Last computed statistics (for debugging/status)
    last_median: f64,
    last_mad: f64,
    last_threshold: f64,
}

/// Result of filtering a sample
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilterResult {
    /// The value to use (filtered or original)
    pub value: f64,
    /// Whether this sample was detected as a spike
    pub is_spike: bool,
    /// The deviation from median (for logging)
    pub deviation: f64,
    /// The threshold used (for logging)
    pub threshold: f64,
    /// Current median of window
    pub median: f64,
    /// Current MAD of window
    pub mad: f64,
}

impl Default for SpikeFilter {
    fn default() -> Self {
        Self::new()
    }
}

impl SpikeFilter {
    /// Create a new spike filter with default parameters
    ///
    /// Default parameters are tuned for typical Dante PTP sync:
    /// - 20 sample window (~20 seconds at 1 sample/sec)
    /// - MAD floor of 0.5 µs/s (prevents issues on ultra-stable systems)
    /// - Mode-dependent k values for appropriate sensitivity
    pub fn new() -> Self {
        Self {
            rate_history: VecDeque::with_capacity(DEFAULT_WINDOW_SIZE + 5),
            window_size: DEFAULT_WINDOW_SIZE,

            k_acq: K_ACQ,
            k_prod: K_PROD,
            k_lock: K_LOCK,
            k_nano: K_NANO,

            min_mad: MIN_MAD_FLOOR,
            warmup_samples: WARMUP_SAMPLES,

            consecutive_spikes: 0,
            max_consecutive_spikes: MAX_CONSECUTIVE_SPIKES,

            total_samples: 0,
            rejected_spikes: 0,

            last_median: 0.0,
            last_mad: 0.0,
            last_threshold: 0.0,
        }
    }

    /// Create a spike filter with custom window size
    pub fn with_window_size(window_size: usize) -> Self {
        let mut filter = Self::new();
        filter.window_size = window_size.max(WARMUP_SAMPLES); // Minimum for valid statistics
        filter.rate_history = VecDeque::with_capacity(window_size + 5);
        filter
    }

    /// Filter a raw rate sample, returning filtered value and spike info
    ///
    /// # Arguments
    /// * `raw_rate` - Raw drift rate in µs/s (equivalent to ppm)
    /// * `mode` - Current operating mode (affects threshold)
    ///
    /// # Returns
    /// FilterResult with the value to use and diagnostic info
    pub fn filter(&mut self, raw_rate: f64, mode: FilterMode) -> FilterResult {
        self.total_samples += 1;

        // Add to history
        self.rate_history.push_back(raw_rate);
        if self.rate_history.len() > self.window_size {
            self.rate_history.pop_front();
        }

        // During warmup, pass through without filtering
        if self.rate_history.len() < self.warmup_samples {
            return FilterResult {
                value: raw_rate,
                is_spike: false,
                deviation: 0.0,
                threshold: f64::MAX,
                median: raw_rate,
                mad: 0.0,
            };
        }

        // Calculate median
        let mut sorted: Vec<f64> = self.rate_history.iter().cloned().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[sorted.len() / 2];

        // Calculate MAD (Median Absolute Deviation)
        let mut deviations: Vec<f64> = sorted.iter().map(|&x| (x - median).abs()).collect();
        deviations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mad = deviations[deviations.len() / 2];

        // Apply minimum MAD floor
        let effective_mad = mad.max(self.min_mad);

        // Get threshold multiplier based on mode
        let k = match mode {
            FilterMode::Acq => self.k_acq,
            FilterMode::Prod => self.k_prod,
            FilterMode::Lock => self.k_lock,
            FilterMode::Nano => self.k_nano,
        };

        // Calculate spike threshold
        let threshold = k * effective_mad;

        // Store for debugging
        self.last_median = median;
        self.last_mad = mad;
        self.last_threshold = threshold;

        // Check if this is a spike
        let deviation = (raw_rate - median).abs();
        let is_spike = deviation > threshold;

        let filtered_value = if is_spike {
            self.consecutive_spikes += 1;

            // If too many consecutive "spikes", accept as real step change
            if self.consecutive_spikes >= self.max_consecutive_spikes {
                debug!(
                    "[Spike] Accepting step change after {} consecutive: {:+.1}us/s",
                    self.consecutive_spikes, raw_rate
                );
                self.consecutive_spikes = 0;
                raw_rate // Accept the value
            } else {
                self.rejected_spikes += 1;
                debug!(
                    "[Spike] Rejected: {:+.1}us/s (median={:+.1}, MAD={:.2}, k={:.1}, threshold={:.1})",
                    raw_rate, median, mad, k, threshold
                );
                median // Replace with median
            }
        } else {
            // Good sample, reset consecutive counter
            self.consecutive_spikes = 0;
            raw_rate
        };

        FilterResult {
            value: filtered_value,
            is_spike,
            deviation,
            threshold,
            median,
            mad,
        }
    }

    /// Get spike rejection statistics
    pub fn stats(&self) -> (u64, u64, f64) {
        let ratio = if self.total_samples > 0 {
            self.rejected_spikes as f64 / self.total_samples as f64 * 100.0
        } else {
            0.0
        };
        (self.total_samples, self.rejected_spikes, ratio)
    }

    /// Get last computed statistics
    pub fn last_stats(&self) -> (f64, f64, f64) {
        (self.last_median, self.last_mad, self.last_threshold)
    }

    /// Clear history (call after NTP step or major event)
    pub fn clear(&mut self) {
        self.rate_history.clear();
        self.consecutive_spikes = 0;
        self.last_median = 0.0;
        self.last_mad = 0.0;
        self.last_threshold = 0.0;
    }

    /// Get current window size
    pub fn window_len(&self) -> usize {
        self.rate_history.len()
    }
}

// ============================================================================
// JITTER ESTIMATOR - Adaptive EMA Smoothing for Noisy Systems
// ============================================================================

/// Jitter estimator for adaptive EMA smoothing
///
/// Measures the standard deviation of drift rate samples to detect
/// high-jitter systems (like those with Realtek NICs) and adjust
/// the EMA smoothing factor accordingly.
///
/// ## Design Principles
/// 1. **Conservative activation**: Only reduces α when jitter clearly high
/// 2. **Gradual adjustment**: Linear interpolation, no sudden jumps
/// 3. **Reversible**: α increases back when jitter decreases
/// 4. **No interference with spike filter**: Measures variance, not outliers
#[derive(Debug)]
pub struct JitterEstimator {
    /// Rolling window of rate samples for jitter estimation
    rate_history: VecDeque<f64>,

    /// Window size for jitter calculation
    window_size: usize,

    /// Minimum samples before jitter estimation is valid
    min_samples: usize,

    /// Jitter threshold below which no smoothing adjustment (µs/s)
    jitter_low: f64,

    /// Jitter threshold above which maximum smoothing applied (µs/s)
    jitter_high: f64,

    /// Normal EMA alpha (used when jitter is low)
    alpha_normal: f64,

    /// Smoothed EMA alpha (used when jitter is high)
    alpha_smooth: f64,

    /// Last computed jitter (stddev)
    last_jitter: f64,

    /// Last computed adaptive alpha
    last_alpha: f64,
}

impl JitterEstimator {
    /// Create a new jitter estimator with default parameters
    pub fn new() -> Self {
        Self {
            rate_history: VecDeque::with_capacity(30),
            window_size: 30,   // 30 samples (~30 seconds)
            min_samples: 15,   // Need at least 15 for valid estimate
            jitter_low: 2.0,   // Below this: α = 0.3 (normal)
            jitter_high: 8.0,  // Above this: α = 0.1 (heavily smoothed)
            alpha_normal: 0.3, // Standard EMA alpha
            alpha_smooth: 0.1, // Smoothed EMA alpha for noisy systems
            last_jitter: 0.0,
            last_alpha: 0.3, // Default to normal alpha
        }
    }

    /// Create with custom parameters (for testing)
    #[cfg(test)]
    pub fn with_params(
        window_size: usize,
        min_samples: usize,
        jitter_low: f64,
        jitter_high: f64,
    ) -> Self {
        Self {
            rate_history: VecDeque::with_capacity(window_size),
            window_size,
            min_samples,
            jitter_low,
            jitter_high,
            alpha_normal: 0.3,
            alpha_smooth: 0.1,
            last_jitter: 0.0,
            last_alpha: 0.3,
        }
    }

    /// Add a rate sample and compute adaptive alpha
    ///
    /// Returns the EMA alpha to use for this sample:
    /// - 0.3 for low-jitter systems (normal responsiveness)
    /// - 0.1-0.3 for medium-jitter systems (interpolated)
    /// - 0.1 for high-jitter systems (heavy smoothing)
    pub fn add_sample(&mut self, rate: f64) -> f64 {
        // Add to history
        self.rate_history.push_back(rate);
        if self.rate_history.len() > self.window_size {
            self.rate_history.pop_front();
        }

        // Not enough samples yet - use normal alpha
        if self.rate_history.len() < self.min_samples {
            self.last_alpha = self.alpha_normal;
            return self.alpha_normal;
        }

        // Calculate jitter (standard deviation)
        let jitter = self.calculate_stddev();
        self.last_jitter = jitter;

        // Compute adaptive alpha based on jitter level
        let alpha = self.compute_alpha(jitter);
        self.last_alpha = alpha;

        alpha
    }

    /// Calculate standard deviation of rate samples
    fn calculate_stddev(&self) -> f64 {
        if self.rate_history.is_empty() {
            return 0.0;
        }

        let n = self.rate_history.len() as f64;
        let mean: f64 = self.rate_history.iter().sum::<f64>() / n;
        let variance: f64 = self
            .rate_history
            .iter()
            .map(|x| (x - mean).powi(2))
            .sum::<f64>()
            / n;
        variance.sqrt()
    }

    /// Compute adaptive alpha based on jitter level
    fn compute_alpha(&self, jitter: f64) -> f64 {
        if jitter <= self.jitter_low {
            // Low jitter: use normal alpha (full responsiveness)
            self.alpha_normal
        } else if jitter >= self.jitter_high {
            // High jitter: use smooth alpha (heavy smoothing)
            self.alpha_smooth
        } else {
            // Medium jitter: linear interpolation
            let t = (jitter - self.jitter_low) / (self.jitter_high - self.jitter_low);
            self.alpha_normal - t * (self.alpha_normal - self.alpha_smooth)
        }
    }

    /// Get last computed jitter (stddev)
    pub fn last_jitter(&self) -> f64 {
        self.last_jitter
    }

    /// Get last computed adaptive alpha
    pub fn last_alpha(&self) -> f64 {
        self.last_alpha
    }

    /// Clear history (call after NTP step or major event)
    pub fn clear(&mut self) {
        self.rate_history.clear();
        self.last_jitter = 0.0;
        self.last_alpha = self.alpha_normal;
    }

    /// Get current sample count
    pub fn sample_count(&self) -> usize {
        self.rate_history.len()
    }
}

impl Default for JitterEstimator {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // BASIC FUNCTIONALITY TESTS
    // ========================================================================

    #[test]
    fn test_new_filter_defaults() {
        let filter = SpikeFilter::new();
        assert_eq!(filter.window_size, DEFAULT_WINDOW_SIZE);
        assert_eq!(filter.warmup_samples, WARMUP_SAMPLES);
        assert!((filter.min_mad - MIN_MAD_FLOOR).abs() < 0.01);
        assert_eq!(filter.rate_history.len(), 0);
    }

    #[test]
    fn test_warmup_passes_through() {
        let mut filter = SpikeFilter::new();

        // During warmup (first 5 samples), all values should pass through
        for i in 0..5 {
            let result = filter.filter(i as f64 * 10.0, FilterMode::Lock);
            assert!(
                !result.is_spike,
                "Sample {} should not be marked as spike during warmup",
                i
            );
            assert!((result.value - (i as f64 * 10.0)).abs() < 0.01);
        }
    }

    #[test]
    fn test_normal_samples_pass_through() {
        let mut filter = SpikeFilter::new();

        // Fill with normal samples around 0
        for _ in 0..20 {
            let result = filter.filter(0.5, FilterMode::Lock);
            assert!(!result.is_spike);
        }

        // A sample within normal range should pass
        let result = filter.filter(1.0, FilterMode::Lock);
        assert!(!result.is_spike);
        assert!((result.value - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_obvious_spike_rejected() {
        let mut filter = SpikeFilter::new();

        // Fill with stable samples around 0 (±1 µs/s)
        for i in 0..20 {
            let noise = (i as f64 % 3.0) - 1.0; // -1, 0, 1
            filter.filter(noise, FilterMode::Lock);
        }

        // A massive spike (+100 µs/s) should be rejected
        let result = filter.filter(100.0, FilterMode::Lock);
        assert!(result.is_spike, "100 µs/s spike should be detected");
        // Should be replaced with median (close to 0)
        assert!(
            result.value.abs() < 5.0,
            "Spike should be replaced with median, got {}",
            result.value
        );
    }

    #[test]
    fn test_negative_spike_rejected() {
        let mut filter = SpikeFilter::new();

        // Fill with stable samples
        for i in 0..20 {
            filter.filter((i % 3) as f64 - 1.0, FilterMode::Lock);
        }

        // A massive negative spike should also be rejected
        let result = filter.filter(-100.0, FilterMode::Lock);
        assert!(result.is_spike);
        assert!(result.value.abs() < 5.0);
    }

    // ========================================================================
    // MODE-DEPENDENT THRESHOLD TESTS
    // ========================================================================

    #[test]
    fn test_acq_mode_more_permissive() {
        let mut filter_acq = SpikeFilter::new();
        let mut filter_nano = SpikeFilter::new();

        // Fill both with same stable data
        for i in 0..20 {
            let val = (i % 3) as f64 - 1.0;
            filter_acq.filter(val, FilterMode::Acq);
            filter_nano.filter(val, FilterMode::Nano);
        }

        // Moderate spike (15 µs/s)
        let result_acq = filter_acq.filter(15.0, FilterMode::Acq);
        let result_nano = filter_nano.filter(15.0, FilterMode::Nano);

        // ACQ should be more permissive (lower threshold)
        // NANO should be stricter (higher threshold relative to data)
        assert!(
            result_acq.threshold < result_nano.threshold,
            "ACQ threshold {} should be < NANO threshold {}",
            result_acq.threshold,
            result_nano.threshold
        );
    }

    #[test]
    fn test_mode_thresholds_increase() {
        let filter = SpikeFilter::new();

        // Verify k values are ordered correctly
        assert!(filter.k_acq < filter.k_prod, "ACQ should have lowest k");
        assert!(filter.k_prod < filter.k_lock, "PROD k < LOCK k");
        assert!(filter.k_lock < filter.k_nano, "LOCK k < NANO k");
    }

    // ========================================================================
    // ADAPTIVE BEHAVIOR TESTS
    // ========================================================================

    #[test]
    fn test_adapts_to_noisy_system() {
        let mut filter = SpikeFilter::new();

        // Simulate noisy system with ±5 µs/s variation
        for i in 0..20 {
            let noise = ((i as f64) * 1.7).sin() * 5.0;
            filter.filter(noise, FilterMode::Lock);
        }

        let (_, mad1, threshold1) = filter.last_stats();

        // Clear and simulate quieter system with ±1 µs/s
        filter.clear();
        for i in 0..20 {
            let noise = ((i as f64) * 1.7).sin() * 1.0;
            filter.filter(noise, FilterMode::Lock);
        }

        let (_, mad2, threshold2) = filter.last_stats();

        // Noisy system should have higher MAD and threshold
        assert!(
            mad1 > mad2,
            "Noisy system MAD {} should be > quiet system MAD {}",
            mad1,
            mad2
        );
        assert!(
            threshold1 > threshold2,
            "Noisy system threshold should be higher"
        );
    }

    #[test]
    fn test_min_mad_floor_prevents_over_sensitivity() {
        let mut filter = SpikeFilter::new();

        // Ultra-stable system with identical samples
        for _ in 0..20 {
            filter.filter(0.0, FilterMode::Lock);
        }

        let (_, mad, threshold) = filter.last_stats();

        // MAD should be at minimum floor
        assert!(mad >= 0.0, "MAD should be non-negative");
        // Threshold should still be meaningful
        assert!(
            threshold >= filter.k_lock * filter.min_mad,
            "Threshold should respect min_mad floor"
        );

        // A small deviation (2 µs/s) should NOT be marked as spike
        let result = filter.filter(2.0, FilterMode::Lock);
        assert!(
            !result.is_spike,
            "2 µs/s should not be spike on stable system with min_mad floor"
        );
    }

    // ========================================================================
    // CONSECUTIVE SPIKE (STEP CHANGE) TESTS
    // ========================================================================

    #[test]
    fn test_consecutive_spikes_accepted_as_step() {
        let mut filter = SpikeFilter::new();

        // Fill with samples around 0
        for _ in 0..20 {
            filter.filter(0.0, FilterMode::Lock);
        }

        // Send multiple consecutive "spikes" at new level (simulating real step change)
        let mut accepted = false;
        for i in 0..10 {
            let result = filter.filter(50.0, FilterMode::Lock);
            if !result.is_spike && (result.value - 50.0).abs() < 1.0 {
                accepted = true;
                assert!(
                    i >= 4,
                    "Should take at least 5 consecutive before accepting, accepted at {}",
                    i
                );
                break;
            }
        }

        assert!(
            accepted,
            "Consecutive spikes at same level should eventually be accepted"
        );
    }

    #[test]
    fn test_single_spike_resets_consecutive() {
        let mut filter = SpikeFilter::new();

        // Fill with stable data
        for _ in 0..20 {
            filter.filter(0.0, FilterMode::Lock);
        }

        // Two spikes
        filter.filter(100.0, FilterMode::Lock);
        filter.filter(100.0, FilterMode::Lock);

        // Good sample (resets counter)
        filter.filter(0.0, FilterMode::Lock);

        // Two more spikes (should restart counting)
        let r1 = filter.filter(100.0, FilterMode::Lock);
        let r2 = filter.filter(100.0, FilterMode::Lock);

        // Should still be rejected (counter was reset)
        assert!(
            r1.is_spike && r2.is_spike,
            "Spikes after reset should still be rejected"
        );
    }

    // ========================================================================
    // STATISTICS TESTS
    // ========================================================================

    #[test]
    fn test_statistics_tracking() {
        let mut filter = SpikeFilter::new();

        // Warmup
        for _ in 0..5 {
            filter.filter(0.0, FilterMode::Lock);
        }

        // More samples
        for _ in 0..10 {
            filter.filter(0.0, FilterMode::Lock);
        }

        // One spike
        filter.filter(100.0, FilterMode::Lock);

        let (total, rejected, ratio) = filter.stats();
        assert_eq!(total, 16);
        assert_eq!(rejected, 1);
        assert!((ratio - (1.0 / 16.0 * 100.0)).abs() < 0.1);
    }

    #[test]
    fn test_clear_resets_state() {
        let mut filter = SpikeFilter::new();

        // Add samples
        for _ in 0..20 {
            filter.filter(5.0, FilterMode::Lock);
        }

        assert_eq!(filter.window_len(), 20);

        filter.clear();

        assert_eq!(filter.window_len(), 0);
        assert_eq!(filter.consecutive_spikes, 0);
    }

    // ========================================================================
    // EDGE CASE TESTS
    // ========================================================================

    #[test]
    fn test_alternating_values() {
        let mut filter = SpikeFilter::new();

        // Alternating ±2 (this is normal variation, not spikes)
        for i in 0..20 {
            let val = if i % 2 == 0 { 2.0 } else { -2.0 };
            filter.filter(val, FilterMode::Lock);
        }

        // Should NOT be marked as spike (it's the normal pattern)
        let result = filter.filter(2.0, FilterMode::Lock);
        assert!(!result.is_spike);
    }

    #[test]
    fn test_gradually_changing_baseline() {
        let mut filter = SpikeFilter::new();

        // Slowly drifting baseline (real frequency change)
        for i in 0..30 {
            let val = i as f64 * 0.5; // 0.5 µs/s per sample increase
            let result = filter.filter(val, FilterMode::Lock);

            // Gradual changes should never be spikes
            assert!(
                !result.is_spike,
                "Gradual change at sample {} should not be spike",
                i
            );
        }
    }

    #[test]
    fn test_realistic_log_pattern() {
        let mut filter = SpikeFilter::new();

        // Simulate realistic pattern from mbc.lan logs
        let samples = [
            -0.4, 0.2, -0.2, 0.5, 1.0, 2.5, -0.7, -0.1, 3.6, -0.7, -1.2, -2.0, -3.9, 2.5, 1.8, 0.8,
            0.4, 0.3, 4.4, 11.7, // 11.7 is borderline
            -1.4, 2.3, -5.5, -4.1, -3.2, 2.0, 1.4, -2.3, -2.8, 1.5,
        ];

        let mut spike_count = 0;
        for (i, &val) in samples.iter().enumerate() {
            let result = filter.filter(val, FilterMode::Lock);
            if result.is_spike {
                spike_count += 1;
                // The only real spikes should be large outliers
                assert!(
                    val.abs() > 10.0,
                    "Sample {} ({}) marked as spike but < 10",
                    i,
                    val
                );
            }
        }

        // Should have very few spikes (maybe 1-2 for the 11.7)
        assert!(
            spike_count <= 2,
            "Too many normal samples marked as spikes: {}",
            spike_count
        );
    }

    #[test]
    fn test_massive_spike_from_logs() {
        let mut filter = SpikeFilter::new();

        // Build up normal baseline from log data
        let baseline = [
            -0.4, 0.2, -0.2, 0.5, 1.0, 2.5, -0.7, -0.1, 3.6, -0.7, -1.2, -2.0, -3.9, 2.5, 1.8, 0.8,
            0.4, -1.7, -1.5, 0.3,
        ];

        for &val in &baseline {
            filter.filter(val, FilterMode::Lock);
        }

        // The massive +149 µs/s spike from logs MUST be rejected
        let result = filter.filter(149.1, FilterMode::Lock);
        assert!(result.is_spike, "+149 µs/s must be detected as spike");
        assert!(
            result.value.abs() < 10.0,
            "Spike should be replaced with median, got {}",
            result.value
        );
    }

    #[test]
    fn test_with_window_size() {
        let filter = SpikeFilter::with_window_size(30);
        assert_eq!(filter.window_size, 30);

        // Minimum window size is 5
        let filter_small = SpikeFilter::with_window_size(3);
        assert_eq!(filter_small.window_size, WARMUP_SAMPLES); // Minimum enforced
    }

    // ========================================================================
    // JITTER ESTIMATOR TESTS
    // ========================================================================

    #[test]
    fn test_jitter_estimator_defaults() {
        let estimator = JitterEstimator::new();
        assert_eq!(estimator.window_size, 30);
        assert_eq!(estimator.min_samples, 15);
        assert!((estimator.jitter_low - 2.0).abs() < 0.01);
        assert!((estimator.jitter_high - 8.0).abs() < 0.01);
        assert!((estimator.alpha_normal - 0.3).abs() < 0.01);
        assert!((estimator.alpha_smooth - 0.1).abs() < 0.01);
    }

    #[test]
    fn test_jitter_warmup_uses_normal_alpha() {
        let mut estimator = JitterEstimator::with_params(20, 10, 2.0, 8.0);

        // During warmup (< 10 samples), should return normal alpha
        for i in 0..9 {
            let alpha = estimator.add_sample(i as f64 * 5.0);
            assert!(
                (alpha - 0.3).abs() < 0.01,
                "During warmup (sample {}), alpha should be 0.3, got {}",
                i,
                alpha
            );
        }
    }

    #[test]
    fn test_jitter_low_noise_system() {
        // Simulate strih.lan: stddev ~0.8 µs/s
        let mut estimator = JitterEstimator::with_params(20, 10, 2.0, 8.0);

        // Add samples with low variance (around 0, stddev ~0.8)
        let low_jitter_samples = [
            -0.7, 0.3, -0.5, 0.8, -0.2, 0.6, -0.8, 0.4, -0.3, 0.5, -0.6, 0.2, -0.4, 0.7, -0.1, 0.3,
            -0.5, 0.4, -0.6, 0.5,
        ];

        let mut last_alpha = 0.3;
        for &sample in &low_jitter_samples {
            last_alpha = estimator.add_sample(sample);
        }

        // Low jitter system should keep normal alpha (0.3)
        assert!(
            (last_alpha - 0.3).abs() < 0.01,
            "Low jitter system should have alpha=0.3, got {}. Jitter={}",
            last_alpha,
            estimator.last_jitter()
        );
        assert!(
            estimator.last_jitter() < 2.0,
            "Jitter should be < 2.0, got {}",
            estimator.last_jitter()
        );
    }

    #[test]
    fn test_jitter_high_noise_system() {
        // Simulate stream.lan: stddev ~10 µs/s
        let mut estimator = JitterEstimator::with_params(20, 10, 2.0, 8.0);

        // Add samples with high variance (oscillating ±15)
        let high_jitter_samples = [
            15.0, -12.0, 18.0, -14.0, 16.0, -10.0, 14.0, -16.0, 12.0, -15.0, 17.0, -11.0, 13.0,
            -17.0, 15.0, -13.0, 16.0, -14.0, 14.0, -12.0,
        ];

        let mut last_alpha = 0.3;
        for &sample in &high_jitter_samples {
            last_alpha = estimator.add_sample(sample);
        }

        // High jitter system should get smoothed alpha (0.1)
        assert!(
            (last_alpha - 0.1).abs() < 0.02,
            "High jitter system should have alpha≈0.1, got {}. Jitter={}",
            last_alpha,
            estimator.last_jitter()
        );
        assert!(
            estimator.last_jitter() > 8.0,
            "Jitter should be > 8.0, got {}",
            estimator.last_jitter()
        );
    }

    #[test]
    fn test_jitter_medium_noise_interpolation() {
        // Medium jitter: stddev ~5 µs/s -> alpha should be interpolated
        let mut estimator = JitterEstimator::with_params(20, 10, 2.0, 8.0);

        // Add samples with medium variance (oscillating ±7)
        let medium_jitter_samples = [
            7.0, -5.0, 6.0, -6.0, 5.0, -7.0, 6.0, -5.0, 7.0, -6.0, 5.0, -7.0, 6.0, -5.0, 7.0, -6.0,
            5.0, -7.0, 6.0, -5.0,
        ];

        let mut last_alpha = 0.3;
        for &sample in &medium_jitter_samples {
            last_alpha = estimator.add_sample(sample);
        }

        // Medium jitter: alpha should be between 0.1 and 0.3
        assert!(
            last_alpha > 0.1 && last_alpha < 0.3,
            "Medium jitter should interpolate alpha, got {}. Jitter={}",
            last_alpha,
            estimator.last_jitter()
        );
    }

    #[test]
    fn test_jitter_recovery_from_high_to_low() {
        let mut estimator = JitterEstimator::with_params(20, 10, 2.0, 8.0);

        // First, add high jitter samples
        for i in 0..15 {
            let val = if i % 2 == 0 { 15.0 } else { -15.0 };
            estimator.add_sample(val);
        }
        assert!(
            estimator.last_alpha() < 0.15,
            "Should have low alpha after high jitter"
        );

        // Now add low jitter samples - alpha should recover
        for _ in 0..20 {
            estimator.add_sample(0.5);
        }

        assert!(
            (estimator.last_alpha() - 0.3).abs() < 0.05,
            "Alpha should recover to ~0.3 after low jitter, got {}",
            estimator.last_alpha()
        );
    }

    #[test]
    fn test_jitter_clear_resets_state() {
        let mut estimator = JitterEstimator::new();

        // Add some samples
        for i in 0..20 {
            estimator.add_sample(i as f64);
        }
        assert!(estimator.sample_count() > 0);

        // Clear should reset
        estimator.clear();
        assert_eq!(estimator.sample_count(), 0);
        assert!((estimator.last_alpha() - 0.3).abs() < 0.01);
        assert!((estimator.last_jitter() - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_jitter_drift_step_not_confused_with_jitter() {
        // A real frequency step (drift changes from +2 to +10) should NOT
        // trigger high jitter because variance of sustained drift is low
        let mut estimator = JitterEstimator::with_params(20, 10, 2.0, 8.0);

        // Sustained positive drift with slight variation
        let drift_step_samples = [
            2.0, 2.5, 3.0, 3.5, 4.0, 4.5, 5.0, 5.5, 6.0, 6.5, 7.0, 7.5, 8.0, 8.5, 9.0, 9.5, 10.0,
            10.5, 10.0, 9.5,
        ];

        let mut last_alpha = 0.3;
        for &sample in &drift_step_samples {
            last_alpha = estimator.add_sample(sample);
        }

        // Sustained drift has moderate stddev but consistent direction
        // This should NOT trigger heavy smoothing
        assert!(
            last_alpha > 0.15,
            "Drift step should not trigger heavy smoothing, got alpha={}. Jitter={}",
            last_alpha,
            estimator.last_jitter()
        );
    }

    #[test]
    fn test_jitter_boundary_values() {
        let mut estimator = JitterEstimator::with_params(10, 5, 2.0, 8.0);

        // Test exactly at jitter_low boundary
        // Samples with stddev = 2.0: [-2, 2, -2, 2, -2, 2, -2, 2, -2, 2]
        for i in 0..10 {
            let val = if i % 2 == 0 { -2.0 } else { 2.0 };
            estimator.add_sample(val);
        }
        // stddev of [-2,2,-2,2,...] = 2.0
        assert!(
            (estimator.last_alpha() - 0.3).abs() < 0.05,
            "At jitter_low boundary, alpha should be ~0.3, got {}",
            estimator.last_alpha()
        );
    }

    #[test]
    fn test_jitter_strih_lan_simulation() {
        // Real data pattern from strih.lan LOCK mode: drift ±0.5-1.5 µs/s
        let mut estimator = JitterEstimator::new();

        let strih_samples = [
            -0.7, -0.3, 0.2, -0.5, 1.2, -0.8, 0.3, -1.0, -0.3, -1.0, -0.7, -0.6, 0.5, -0.6, 0.8,
            -0.2, -0.8, -0.7, 0.6, -0.8, 0.3, -1.0, -0.3, -0.6, -0.4, -0.7, -0.4, 0.4, -0.3, 0.2,
        ];

        let mut last_alpha = 0.3;
        for &sample in &strih_samples {
            last_alpha = estimator.add_sample(sample);
        }

        // strih.lan should keep normal alpha
        assert!(
            (last_alpha - 0.3).abs() < 0.01,
            "strih.lan pattern should have alpha=0.3, got {}. Jitter={}",
            last_alpha,
            estimator.last_jitter()
        );
    }

    #[test]
    fn test_jitter_stream_lan_simulation() {
        // Real data pattern from stream.lan: drift ±10-20 µs/s oscillation
        let mut estimator = JitterEstimator::new();

        let stream_samples = [
            4.2, -2.3, -18.1, 18.0, -7.5, -16.3, -1.8, 10.8, -0.2, 1.5, -0.5, 25.4, -9.6, -18.4,
            -3.7, 12.3, -13.3, 1.5, -2.1, 5.8, -9.2, 8.5, -10.5, 19.6, 4.2, 6.1, -17.9, 24.5,
            -24.2, -18.5,
        ];

        let mut last_alpha = 0.3;
        for &sample in &stream_samples {
            last_alpha = estimator.add_sample(sample);
        }

        // stream.lan should get heavy smoothing
        assert!(
            last_alpha < 0.15,
            "stream.lan pattern should have alpha<0.15, got {}. Jitter={}",
            last_alpha,
            estimator.last_jitter()
        );
    }

    #[test]
    fn test_jitter_mbc_lan_simulation() {
        // mbc.lan: moderate jitter with occasional spikes (spikes filtered by spike filter)
        // After spike filter, drift should be moderate (~3-8 µs/s)
        let mut estimator = JitterEstimator::new();

        let mbc_samples = [
            -1.6, 3.0, 1.3, -2.2, 7.4, -2.5, -4.5, 2.0, -1.7, 3.8, -3.2, 5.5, -0.8, 2.3, -4.1,
            -1.4, 3.2, -0.5, 2.1, -3.5, 2.5, 0.5, -2.4, 3.7, 0.8, 1.7, 3.9, -4.3, -0.1, 2.5,
        ];

        let mut last_alpha = 0.3;
        for &sample in &mbc_samples {
            last_alpha = estimator.add_sample(sample);
        }

        // mbc.lan after spike filtering should have moderate jitter
        // Alpha should be between 0.2 and 0.3
        assert!(
            last_alpha >= 0.2,
            "mbc.lan should have alpha>=0.2, got {}. Jitter={}",
            last_alpha,
            estimator.last_jitter()
        );
    }
}
