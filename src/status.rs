use serde::{Deserialize, Serialize};

/// Sync status shared via IPC between service and tray app
///
/// This struct contains all the information needed for the tray app to:
/// - Display sync state (locked, acquiring, offline)
/// - Animate the icon based on drift rate
/// - Show detailed status in tooltips and menus
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SyncStatus {
    // ========================================================================
    // Core PTP Status (existing fields)
    // ========================================================================
    /// Current phase offset from Dante master (nanoseconds)
    /// Note: Absolute value is meaningless for Dante (device uptime, not UTC)
    pub offset_ns: i64,

    /// Current frequency adjustment being applied (PPM)
    pub drift_ppm: f64,

    /// Grandmaster clock UUID (from PTP Sync messages)
    pub gm_uuid: Option<[u8; 6]>,

    /// True once sync is established (receiving valid packets)
    pub settled: bool,

    /// Unix timestamp of last status update
    pub updated_ts: u64,

    // ========================================================================
    // Extended Status (new fields for tray app)
    // ========================================================================
    /// True when frequency is locked (rate stable < 5us/s)
    /// Used for icon badge color (green = locked)
    pub is_locked: bool,

    /// Smoothed rate of offset change (us/s)
    /// Used for icon animation speed - higher rate = faster pulse
    pub smoothed_rate_ppm: f64,

    /// Last NTP offset measurement (microseconds)
    /// Used for NTP status display in tray menu
    pub ntp_offset_us: i64,

    /// Current operating mode: "ACQ" (acquiring), "PROD" (production), "LOCK" (locked), "NTP-only"
    /// Used for status display and icon state
    pub mode: String,

    /// True when NTP sync has failed (can't reach server)
    pub ntp_failed: bool,
}

impl Default for SyncStatus {
    fn default() -> Self {
        SyncStatus {
            // Core fields
            offset_ns: 0,
            drift_ppm: 0.0,
            gm_uuid: None,
            settled: false,
            updated_ts: 0,

            // Extended fields for tray app
            is_locked: false,
            smoothed_rate_ppm: 0.0,
            ntp_offset_us: 0,
            mode: "ACQ".to_string(),
            ntp_failed: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_status_default() {
        let status = SyncStatus::default();
        assert_eq!(status.offset_ns, 0);
        assert_eq!(status.drift_ppm, 0.0);
        assert!(!status.is_locked);
        assert_eq!(status.mode, "ACQ");
    }

    #[test]
    fn test_sync_status_serde_roundtrip() {
        let mut status = SyncStatus::default();
        status.is_locked = true;
        status.mode = "LOCK".to_string();
        status.smoothed_rate_ppm = 2.5;
        status.ntp_offset_us = 150;

        let json = serde_json::to_string(&status).expect("serialize failed");
        let restored: SyncStatus = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(restored.is_locked, true);
        assert_eq!(restored.mode, "LOCK");
        assert!((restored.smoothed_rate_ppm - 2.5).abs() < f64::EPSILON);
        assert_eq!(restored.ntp_offset_us, 150);
    }
}
