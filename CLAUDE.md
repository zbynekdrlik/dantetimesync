# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Guidelines

- Act as senior Rust, Windows, hardware, and clock-skilled developer
- Use TDD approach and ensure all code has test coverage
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

**Windows cross-compilation** requires WinPcap Developer's Pack with `LIB` env var set to `WpdPack/Lib/x64`.

## Architecture Overview

Dante PTP Time Sync is a high-precision PTP (Precision Time Protocol) synchronization tool for Dante Audio networks. It implements PTPv1 over UDP multicast (ports 319/320) with a hybrid NTP+PTP approach.

### Core Components

- **`main.rs`** - Entry point, CLI parsing, Windows service logic, IPC server setup, and main sync loop orchestration
- **`controller.rs`** - `PtpController<C, N, S>` - Generic controller that coordinates PTP sync. Handles Sync/FollowUp message pairs, lucky packet filtering, and clock adjustments
- **`servo.rs`** - `PiServo` - PI (Proportional-Integral) servo loop for frequency adjustment. Takes phase offset in nanoseconds, outputs correction in PPM
- **`ptp.rs`** - PTPv1 packet parsing (headers, Sync bodies, FollowUp bodies)
- **`clock/mod.rs`** - `SystemClock` trait with platform-specific implementations:
  - `linux.rs` - Uses `adjtimex` for frequency adjustment
  - `windows.rs` - Uses `SetSystemTimeAdjustmentPrecise` API
- **`traits.rs`** - `NtpSource` and `PtpNetwork` traits (mockable for testing)
- **`config.rs`** - `SystemConfig`, `ServoConfig`, `FilterConfig` - tuning parameters with different defaults for Linux vs Windows
- **`net.rs`** - Network utilities (multicast socket creation, interface detection, timestamping)
- **`ntp.rs`** - NTP client for initial coarse time alignment
- **`status.rs`** - `SyncStatus` struct shared via IPC to tray app
- **`rtc.rs`** (Unix only) - Hardware RTC updates via ioctl

### Sync Flow

1. NTP sync for initial coarse alignment (optional, skippable)
2. Join PTP multicast groups (224.0.1.129 on ports 319/320)
3. Process Sync messages → store pending with receive timestamp
4. Match FollowUp messages → calculate phase offset from (T1, T2) pair
5. Lucky packet filter selects minimum offset from sample window
6. PI servo calculates frequency adjustment in PPM
7. Platform clock adjusts system frequency

### Key Design Patterns

- **Generic controller**: `PtpController<C: SystemClock, N: PtpNetwork, S: NtpSource>` allows dependency injection and mocking
- **Lucky packet filtering**: Selects minimum offset from N samples to filter network jitter
- **Platform abstraction**: `clock/mod.rs` re-exports `PlatformClock` based on target OS

### Configuration

Config file locations:
- Linux: `/etc/dantetimesync/config.json`
- Windows: `C:\ProgramData\DanteTimeSync\config.json`

Key tunable parameters (in `config.rs`):
- Servo gains: `kp`, `ki`
- Filter thresholds: `step_threshold_ns`, `panic_threshold_ns`, `sample_window_size`

### Binaries

- `dantetimesync` - Main sync daemon/service
- `dantetray` - Windows tray application (reads status via named pipe IPC)
