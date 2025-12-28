# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Guidelines

- **FOCUSED APPLICATION:** This is NOT a general-purpose all-situations application. This is a highly focused app for our exact Dante audio network synchronization use case. Code that adds complexity for unused features should be removed, not kept "just in case."
- Act as senior Rust, Windows, hardware, and clock-skilled developer
- Use TDD approach and ensure all code has test coverage
- **Critical Self-Review:** Be skeptical of your own conclusions. Before assuming something "doesn't work" or "isn't supported":
  1. Search online documentation and GitHub issues for evidence
  2. Verify from multiple independent sources (official docs, GitHub, Stack Overflow)
  3. Never make assumptions about API behavior without documentation
  4. If debugging, confirm the actual cause before implementing workarounds
  5. When you find contradicting evidence to your assumption, acknowledge the error immediately
- **Study Open Source Code:** When using open source libraries/frameworks:
  1. Read the actual source code to understand internal behavior
  2. Don't rely solely on documentation - verify by reading implementation
  3. If something doesn't work as expected, investigate the source to find why
  4. Consider fixing issues in the library itself rather than working around them
- **No Circular Development:** Never give up on a promising approach after first struggles:
  1. If an approach should theoretically work, investigate WHY it doesn't instead of reverting
  2. Don't cycle between approaches (try A → fail → try B → fail → try A again)
  3. Commit to achieving the goal (e.g., Linux-level 50µs precision) - don't settle for inferior solutions
  4. If a library has issues, consider contributing fixes rather than abandoning it
- **HARD REQUIREMENT - Precision Target <50µs:**
  1. The ONLY acceptable precision for both Linux AND Windows is <50 microseconds
  2. NEVER accept, propose, or implement solutions with worse precision (100ms, 1ms, etc.)
  3. If current approach shows precision worse than 50µs, it is FAILING - do not present it as "working"
  4. Before trying new approaches, RESEARCH how other projects (PTPSync, Meinberg, ptpd) achieve <50µs on Windows
  5. Consult with user before switching approaches - do not endlessly iterate without results
- **Test Quality:** All code must have 100% test coverage with high-quality, complex E2E tests. Never put out a broken version. The `tests/simulation_e2e.rs` contains critical simulation tests that validate the servo and controller behavior under various conditions.
- **Local Verification:** Prioritize local `cargo build`, `cargo test`, and running the binary locally to verify changes before pushing to GitHub or deploying remotely
- **CI/CD Verification:** Wait until GitHub Actions CI/CD pipeline has successfully finished (green checkmark) before telling the user to update or run commands. Monitor `gh run view` until completion
- **Autonomous Deployment:** Install and verify updates on remote machines (Windows/Linux) listed in `TARGETS.md` using available tools (SSH, etc.)

## Build Commands

```bash
# Build release binary
cargo build --release

# Run tests
cargo test

# Run a specific test
cargo test test_name

# Run with logging
RUST_LOG=debug cargo run
```

**Windows cross-compilation** requires Npcap SDK 1.13+ with `LIB` env var set to `npcap-sdk/Lib/x64`.

## Hardware Constraints (CRITICAL)

**This project implements SOFTWARE-ONLY PTP frequency synchronization:**
- NONE of the target computers have NICs with hardware timestamping support
- Standard consumer/enterprise Ethernet NICs (Intel, Realtek) are used
- DO NOT waste time on approaches requiring hardware timestamping (SIO_TIMESTAMPING, PTP hardware clocks, etc.)
- The goal is to achieve <50µs precision using SOFTWARE timestamps only
- Linux achieves this with kernel-level SO_TIMESTAMPNS - Windows needs equivalent software approach
- Audinate/Dante drivers are NOT involved - we use standard Windows/Linux network stack

## Architecture Overview

Dante PTP Time Sync is a high-precision PTP (Precision Time Protocol) synchronization tool for Dante Audio networks. It implements PTPv1 over UDP multicast (ports 319/320) with a hybrid NTP+PTP approach.

### Core Components

- **`main.rs`** - Entry point, CLI parsing, Windows service logic, IPC server setup, and main sync loop orchestration
- **`controller.rs`** - `PtpController<C, N, S>` - Generic controller that coordinates PTP sync. Contains rate-based servo logic, mode transitions (ACQ→PROD→LOCK), and clock adjustments
- **`ptp.rs`** - PTPv1 packet parsing (headers, Sync bodies, FollowUp bodies)
- **`clock/mod.rs`** - `SystemClock` trait with platform-specific implementations:
  - `linux.rs` - Uses `adjtimex` for frequency adjustment
  - `windows.rs` - Uses `SetSystemTimeAdjustmentPrecise` API
- **`traits.rs`** - `NtpSource` and `PtpNetwork` traits (mockable for testing)
- **`config.rs`** - `SystemConfig`, `ServoConfig`, `FilterConfig` - tuning parameters with different defaults for Linux vs Windows
- **`net.rs`** / **`net_pcap.rs`** / **`net_winsock.rs`** - Network utilities (multicast, timestamping, platform-specific packet capture)
- **`ntp.rs`** - NTP client for UTC alignment
- **`status.rs`** - `SyncStatus` struct shared via IPC to tray app (includes `is_locked`, `smoothed_rate_ppm`, `mode`)

### CRITICAL: Dante Time vs UTC Time

**Dante PTP provides DEVICE UPTIME, not UTC time.** This is fundamental to the architecture:

- Dante grandmaster clock uses device uptime (time since power-on), NOT real UTC
- The PTP offset between local clock and Dante master is MEANINGLESS for absolute time
- PTP is used ONLY for **frequency synchronization** (making clocks tick at the same rate)
- NTP is used for **UTC phase alignment** (setting the correct absolute time)

**Dual-Source Architecture:**
1. **PTP (Dante)** → `adjust_frequency()` - controls clock tick rate
2. **NTP (UTC)** → `step_clock()` - periodically corrects absolute time

These operations are INDEPENDENT:
- `step_clock()` sets absolute time value (does NOT affect frequency)
- `adjust_frequency()` sets tick rate (does NOT affect absolute time)

**PTP stepping has been removed from the codebase** - stepping based on Dante offset would desync from UTC. Only NTP steps the clock via `check_ntp_utc_tracking()`.

### Sync Flow

1. NTP sync for initial coarse UTC alignment
2. Join PTP multicast groups (224.0.1.129 on ports 319/320)
3. Process Sync messages → store pending with receive timestamp
4. Match FollowUp messages → calculate phase offset from (T1, T2) pair
5. Lucky packet filter selects minimum offset from sample window
6. PI servo calculates frequency adjustment in PPM
7. Platform clock adjusts system frequency (PTP controls rate only)
8. Periodic NTP checks maintain UTC alignment (NTP controls absolute time)

### Key Design Patterns

- **Generic controller**: `PtpController<C: SystemClock, N: PtpNetwork, S: NtpSource>` allows dependency injection and mocking
- **Lucky packet filtering**: Selects minimum offset from N samples to filter network jitter
- **Platform abstraction**: `clock/mod.rs` re-exports `PlatformClock` based on target OS

### Configuration

Config file locations:
- Linux: `/etc/dantesync/config.json`
- Windows: `C:\ProgramData\DanteSync\config.json`

Key tunable parameters (in `config.rs`):
- Servo gains: `kp`, `ki` (reference only - controller uses adaptive gains)
- Filter settings: `sample_window_size`, `min_delta_ns`, `calibration_samples`, `warmup_secs`

### Binaries

- `dantesync` - Main sync daemon/service
- `dantesync-tray` - Windows tray application:
  - Dynamic icon with pulsing ring based on drift rate
  - Toast notifications for state transitions (lock/unlock/offline)
  - Service control (Restart/Stop) via menu
  - Reads status via named pipe IPC (`\\.\pipe\dantesync`)
