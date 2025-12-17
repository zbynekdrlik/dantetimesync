use anyhow::Result;
use log::{info, warn, error};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};
use std::sync::{Arc, RwLock};
use crate::clock::SystemClock;
use crate::traits::{NtpSource, PtpNetwork};
use crate::ptp::{PtpV1Header, PtpV1Control, PtpV1FollowUpBody, PtpV1SyncMessageBody};
use crate::servo::PiServo;
use crate::status::SyncStatus;
#[cfg(unix)]
use crate::rtc;

// Constants
#[cfg(windows)]
const MIN_DELTA_NS: i64 = 0;               // Allow 0ms (bursts/quantization) on Windows
#[cfg(not(windows))]
const MIN_DELTA_NS: i64 = 1_000_000;       // 1ms on Linux

#[cfg(windows)]
const MASSIVE_DRIFT_THRESHOLD_NS: i64 = 10_000_000; // 10ms on Windows (High Jitter/Drift)
#[cfg(not(windows))]
const MASSIVE_DRIFT_THRESHOLD_NS: i64 = 500_000;    // 500us on Linux (Precision)

const MAX_DELTA_NS: i64 = 2_000_000_000;   // 2s
const MAX_PHASE_OFFSET_FOR_STEP_NS: i64 = 10_000_000; // 10ms (Initial alignment)
const RTC_UPDATE_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes
const SAMPLE_WINDOW_SIZE: usize = 4; // Reduced to 4 to speed up servo reaction

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

    // Status for IPC/Tray
    status_shared: Arc<RwLock<SyncStatus>>,
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
    pub fn new(clock: C, network: N, ntp: S, status_shared: Arc<RwLock<SyncStatus>>) -> Self {
        #[cfg(windows)]
        let servo = PiServo::new(0.1, 0.001); // Aggressive for Windows VM
        #[cfg(not(windows))]
        let servo = PiServo::new(0.0005, 0.00005); // Standard for Linux

        PtpController {
            clock,
            network,
            ntp,
            servo,
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
            status_shared,
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

                if phase_offset_ns.abs() > MAX_PHASE_OFFSET_FOR_STEP_NS {
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
                // Check for massive drift while settled
                if phase_offset_ns.abs() > MASSIVE_DRIFT_THRESHOLD_NS {
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
            
            if self.sample_window.len() >= SAMPLE_WINDOW_SIZE {
                if let Some(&lucky_offset) = self.sample_window.iter().min() {
                    
                    self.last_phase_offset_ns = lucky_offset;
                    
                    let adj_ppm = self.servo.sample(lucky_offset);
                    self.last_adj_ppm = adj_ppm;
                    
                    let factor = 1.0 + (adj_ppm / 1_000_000.0);
                    
                    if let Err(e) = self.clock.adjust_frequency(factor) {
                        warn!("Clock adjustment failed: {}", e);
                    }
                    
                    // Update shared status here (once every 8 packets)
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
        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp, status);
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
        let mut controller = PtpController::new(mock_clock, mock_net, mock_ntp, status);
        
        // Process 16 packets (8 Sync + 8 FollowUp)
        for _ in 0..16 {
            let _ = controller.process_loop_iteration();
        }
        
        // Check settled status
        assert!(controller.get_status_shared().read().unwrap().settled);
    }
}