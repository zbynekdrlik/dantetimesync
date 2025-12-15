use anyhow::Result;
use log::{info, warn, error, debug};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};
use std::process::Command;
use crate::clock::SystemClock;
use crate::traits::{NtpSource, PtpNetwork};
use crate::ptp::{PtpV1Header, PtpV1Control, PtpV1FollowUpBody, PtpV1SyncMessageBody};
use crate::servo::PiServo;
#[cfg(unix)]
use crate::rtc;

// Constants
const MIN_DELTA_NS: i64 = 1_000_000;       // 1ms
const MAX_DELTA_NS: i64 = 2_000_000_000;   // 2s
const MAX_PHASE_OFFSET_FOR_STEP_NS: i64 = 1_000_000; // 1ms
const RTC_UPDATE_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes
const SAMPLE_WINDOW_SIZE: usize = 8; // Enabled Lucky Packet Filter

pub struct PtpController<C, N, S> 
where 
    C: SystemClock,
    N: PtpNetwork,
    S: NtpSource
{
    clock: C,
    network: N,
    ntp: S,
    servo: PiServo,
    
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
    pub fn new(clock: C, network: N, ntp: S) -> Self {
        PtpController {
            clock,
            network,
            ntp,
            servo: PiServo::new(0.0005, 0.00005),
            pending_syncs: HashMap::new(),
            prev_t1_ns: 0,
            prev_t2_ns: 0,
            current_gm_uuid: None,
            sample_window: Vec::with_capacity(SAMPLE_WINDOW_SIZE),
            last_phase_offset_ns: 0,
            last_adj_ppm: 0.0,
            initial_epoch_offset_ns: 0,
            epoch_aligned: false,
            last_rtc_update: Instant::now(), 
            valid_count: 0,
            clock_settled: false,
            settling_threshold: 1, 
        }
    }

    pub fn run_ntp_sync(&mut self, skip: bool) {
        if skip { return; }
        
        match self.ntp.get_offset() {
            Ok((offset, sign)) => {
                let sign_str = if sign > 0 { "+" } else { "-" };
                info!("NTP Offset: {}{:?}", sign_str, offset);
                
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
        if !self.clock_settled {
            info!("[Status] Settling... ({}/{}) Waiting for valid PTP pairs...", self.valid_count, self.settling_threshold);
        } else {
            let phase_offset_us = self.last_phase_offset_ns as f64 / 1_000.0;
            let action_str = if self.last_adj_ppm.abs() < 0.01 {
                format!("Locked (Stable)")
            } else if self.last_adj_ppm > 0.0 {
                format!("Speeding up ({:+.3} ppm)", self.last_adj_ppm)
            } else {
                format!("Slowing down ({:+.3} ppm)", self.last_adj_ppm)
            };
            
            let factor = 1.0 + (self.last_adj_ppm / 1_000_000.0);

            info!("[Status] {} | Phase Offset: {:.3} µs | Factor: {:.9}", 
                action_str, phase_offset_us, factor);
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
                            info!("Resetting servo filter due to GM change.");
                            self.reset_filter();
                            self.servo.reset();
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
        
        let mut phase_offset_ns = (t2_ns % 1_000_000_000) - (t1_ns % 1_000_000_000);
        if phase_offset_ns > 500_000_000 { phase_offset_ns -= 1_000_000_000; }
        else if phase_offset_ns < -500_000_000 { phase_offset_ns += 1_000_000_000; }

        if self.prev_t1_ns > 0 && self.prev_t2_ns > 0 {
            let delta_master = t1_ns - self.prev_t1_ns;
            let delta_slave = t2_ns - self.prev_t2_ns;
            
            if delta_master < MIN_DELTA_NS || delta_master > MAX_DELTA_NS ||
               delta_slave < MIN_DELTA_NS || delta_slave > MAX_DELTA_NS {
                warn!("Delta out of range. Skipping.");
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

                if phase_offset_ns.abs() > MAX_PHASE_OFFSET_FOR_STEP_NS {
                    info!("Initial Phase Offset {}ms is large. Stepping clock to align phase...", phase_offset_ns / 1_000_000);
                    let step_duration = Duration::from_nanos(phase_offset_ns.abs() as u64);
                    let sign = if phase_offset_ns > 0 { -1 } else { 1 };
                    if let Err(e) = self.clock.step_clock(step_duration, sign) {
                        error!("Failed to step clock for phase alignment: {}", e);
                    } else {
                        info!("Phase step complete.");
                        self.reset_filter();
                        self.servo.reset();
                        return;
                    }
                }
                
                info!("Sync established. Updating RTC...");
                self.update_rtc_now();
            }

            // LUCKY PACKET FILTER LOGIC
            self.sample_window.push(phase_offset_ns);
            
            if self.sample_window.len() >= SAMPLE_WINDOW_SIZE {
                if let Some(&lucky_offset) = self.sample_window.iter().min() {
                    
                    self.last_phase_offset_ns = lucky_offset;
                    
                    let adj_ppm = self.servo.sample(lucky_offset);
                    self.last_adj_ppm = adj_ppm;
                    
                    let factor = 1.0 + (adj_ppm / 1_000_000.0);
                    
                    if let Err(e) = self.clock.adjust_frequency(factor) {
                        warn!("Clock adjustment failed: {}", e);
                    }
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::MockSystemClock;
    use crate::traits::{MockNtpSource, MockPtpNetwork};
    use mockall::predicate::*;
    use mockall::Sequence;
    use byteorder::{BigEndian, WriteBytesExt};

    #[test]
    fn test_ntp_sync_trigger() {
        let _ = env_logger::builder().is_test(true).try_init();
        let mut mock_clock = MockSystemClock::new();
        let mut mock_net = MockPtpNetwork::new();
        let mut mock_ntp = MockNtpSource::new();

        mock_ntp.expect_get_offset()
            .times(1)
            .returning(|| Ok((Duration::from_millis(100), 1)));

        mock_clock.expect_step_clock()
            .with(eq(Duration::from_millis(100)), eq(1))
            .times(1)
            .returning(|_, _| Ok(()));

        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp);
        controller.run_ntp_sync(false);
    }
}
