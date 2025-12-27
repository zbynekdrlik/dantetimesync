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
            rate_history: VecDeque::with_capacity(25),
            window_size: 20,

            // k values based on robust statistics:
            // k=3: catches 99% of Gaussian outliers
            // We use higher values because we want to be conservative
            // and only reject clear spikes, not normal variation
            k_acq: 4.0,  // Permissive: allow fast convergence
            k_prod: 5.0, // Balanced: moderate protection
            k_lock: 6.0, // Strict: protect locked state
            k_nano: 8.0, // Very strict: ultra-stable mode

            min_mad: 0.5,      // Minimum MAD floor (µs/s)
            warmup_samples: 5, // Need at least 5 samples

            consecutive_spikes: 0,
            max_consecutive_spikes: 5, // Accept after 5 consecutive "spikes"

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
        filter.window_size = window_size.max(5); // Minimum 5 for valid statistics
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
        assert_eq!(filter.window_size, 20);
        assert_eq!(filter.warmup_samples, 5);
        assert!((filter.min_mad - 0.5).abs() < 0.01);
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
        assert_eq!(filter_small.window_size, 5);
    }
}
