# DanteSync

A high-precision PTP (Precision Time Protocol) synchronization tool optimized for Dante Audio networks, written in Rust.

## Features

### Core Sync
- **PTPv1 Support:** Syncs with Dante Grandmasters (PTPv1/UDP 319/320)
- **Hybrid Mode:** Uses NTP for UTC alignment + PTP for microsecond-precision frequency adjustment
- **Cross-Platform:** Runs on Linux and Windows as a system service
- **Rate-Based Servo:** Adaptive frequency control targeting <5µs/s drift rate
- **Lucky Packet Filtering:** Minimizes network jitter effects

### Windows Tray App
- **Dynamic Icon:** Pulsing ring indicates drift rate (green=locked, yellow=acquiring, red=offline)
- **Toast Notifications:** Alerts for lock achieved, lock lost, service online/offline
- **Service Control:** Restart/Stop service directly from tray menu
- **Live Status:** Tooltip shows drift rate, frequency adjustment, NTP offset

## Installation

### Linux
```bash
curl -sSL https://raw.githubusercontent.com/zbynekdrlik/dantesync/master/install.sh | sudo bash
```

### Windows
1. **Prerequisite:** Install [Npcap](https://npcap.com/#download) (Select "Install Npcap in WinPcap API-compatible Mode")
2. Open PowerShell as **Administrator**
3. Run:
```powershell
irm https://raw.githubusercontent.com/zbynekdrlik/dantesync/master/install.ps1 | iex
```

## Uninstall

### Windows
```powershell
irm https://raw.githubusercontent.com/zbynekdrlik/dantesync/master/uninstall.ps1 | iex
```

## Usage (Manual)
```bash
dantesync [OPTIONS]
```
- `--interface <NAME>`: Bind to specific interface (e.g., `eth0`)
- `--ntp-server <IP>`: NTP server for initial sync (default: `10.77.8.2`)
- `--skip-ntp`: Skip NTP sync
- `--service`: (Windows Only) Run as a Windows Service

## Build from Source
```bash
cargo build --release
```

**Windows Build Requirements:**
- Rust Toolchain (`x86_64-pc-windows-msvc`)
- Npcap SDK 1.13+ (set `LIB` env var to `npcap-sdk/Lib/x64`)

## Configuration

Config files:
- Linux: `/etc/dantesync/config.json`
- Windows: `C:\ProgramData\DanteSync\config.json`

Log files:
- Linux: `/var/log/dantesync/dantesync.log`
- Windows: `C:\ProgramData\DanteSync\dantesync.log`

## License
MIT
