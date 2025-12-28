#!/bin/bash
set -e

echo ">>> DanteSync Installer <<<"

# Version is extracted from the installed binary
if [ "$1" == "--version" ]; then
    VERSION=$(/usr/local/bin/dantesync --version 2>/dev/null | grep -oP '\d+\.\d+\.\d+' || echo "not installed")
    echo "DanteSync v$VERSION"
    exit 0
fi

if [ "$EUID" -ne 0 ]; then
  echo "Error: Please run as root (sudo ./install.sh)"
  exit 1
fi

# 1. Install System Dependencies
echo ">>> Installing system dependencies..."
apt-get update
# util-linux provides hwclock
apt-get install -y build-essential curl util-linux

# 2. Try to Download Binary (x86_64 only)
ARCH=$(uname -m)
SKIP_BUILD=false

if [ "$ARCH" == "x86_64" ]; then
    echo ">>> Detected x86_64 architecture. Fetching latest release..."

    # Get release version from GitHub API
    RELEASE_VERSION=$(curl -sL https://api.github.com/repos/zbynekdrlik/dantesync/releases/latest | grep -oP '"tag_name":\s*"\K[^"]+' || echo "latest")
    echo ">>> Installing Version: $RELEASE_VERSION"

    DOWNLOAD_URL="https://github.com/zbynekdrlik/dantesync/releases/latest/download/dantesync-linux-amd64"

    if curl --fail -L -o dantesync_bin "$DOWNLOAD_URL"; then
        echo ">>> Download successful."

        echo ">>> Stopping existing service..."
        systemctl stop dantesync 2>/dev/null || true

        echo ">>> Installing binary to /usr/local/bin/..."
        chmod +x dantesync_bin
        mv dantesync_bin /usr/local/bin/dantesync
        SKIP_BUILD=true
    else
        echo ">>> Download failed. Falling back to source build."
    fi
else
    echo ">>> Architecture $ARCH detected. Building from source required."
fi

# 3. Build Release Binary (If download skipped or failed)
if [ "$SKIP_BUILD" = false ]; then
    # Install Rust
    if ! command -v cargo &> /dev/null; then
        echo ">>> Installing Rust..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source "$HOME/.cargo/env" || source "/root/.cargo/env"
    fi

    echo ">>> Building dantesync from source..."
    export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"

    if [ ! -f "Cargo.toml" ]; then
        echo ">>> Cargo.toml not found. Cloning repository to build..."
        cd $(mktemp -d)
        git clone https://github.com/zbynekdrlik/dantesync.git .
    fi

    cargo build --release

    echo ">>> Stopping existing service..."
    systemctl stop dantesync 2>/dev/null || true

    echo ">>> Installing binary to /usr/local/bin/..."
    cp target/release/dantesync /usr/local/bin/
    chmod +x /usr/local/bin/dantesync
fi

# 4. Create Config Dir
echo ">>> Creating configuration directory..."
mkdir -p /etc/dantesync

# 5. Disable Conflicting Services
echo ">>> Disabling conflicting time services..."
systemctl stop systemd-timesyncd 2>/dev/null || true
systemctl disable systemd-timesyncd 2>/dev/null || true
systemctl stop chrony 2>/dev/null || true
systemctl disable chrony 2>/dev/null || true
systemctl stop ntp 2>/dev/null || true
systemctl disable ntp 2>/dev/null || true
# Additional PTP/NTP services found on some systems
systemctl stop ptp4l phc2sys time-sync-coordinator 2>/dev/null || true
systemctl disable ptp4l phc2sys time-sync-coordinator 2>/dev/null || true
# Disable NTP via timedatectl
timedatectl set-ntp false 2>/dev/null || true

# 6. Create Systemd Service
echo ">>> Creating systemd service..."
# Extract version from installed binary for service description
BINARY_VERSION=$(/usr/local/bin/dantesync --version 2>/dev/null | grep -oP '\d+\.\d+\.\d+' || echo "unknown")
cat <<EOF > /etc/systemd/system/dantesync.service
[Unit]
Description=DanteSync PTP Time Sync Service v$BINARY_VERSION
After=network-online.target
Wants=network-online.target

[Service]
# Run as root for port 319, adjtimex, and RTC ioctl access
User=root
Group=root
ExecStart=/usr/local/bin/dantesync
Restart=always
RestartSec=5
# High priority for timestamping accuracy
CPUSchedulingPolicy=fifo
CPUSchedulingPriority=50

[Install]
WantedBy=multi-user.target
EOF

# 7. Enable and Start Service
echo ">>> Starting service..."
systemctl daemon-reload
systemctl enable dantesync
systemctl restart dantesync

# 8. Final Verification
echo ">>> Verifying installation..."
INSTALLED_VERSION=$(/usr/local/bin/dantesync --version)
echo ">>> Installed: $INSTALLED_VERSION"

echo ">>> Installation Complete!"
echo ">>> Check status with: systemctl status dantesync"
echo ">>> View logs with: journalctl -u dantesync -f"
