#!/bin/bash
set -e

VERSION="1.1.82"

if [ "$1" == "--version" ]; then
    echo "Dante Time Sync Installer v$VERSION"
    exit 0
fi

echo ">>> Dante Time Sync Installer v$VERSION <<<"

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
    echo ">>> Detected x86_64 architecture. Attempting to download latest release..."
    DOWNLOAD_URL="https://github.com/zbynekdrlik/dantetimesync/releases/latest/download/dantetimesync-linux-amd64"
    
    if curl --fail -L -o dantetimesync_bin "$DOWNLOAD_URL"; then
        echo ">>> Download successful."
        
        echo ">>> Stopping existing service..."
        systemctl stop dantetimesync 2>/dev/null || true
        
        echo ">>> Installing binary to /usr/local/bin/..."
        chmod +x dantetimesync_bin
        mv dantetimesync_bin /usr/local/bin/dantetimesync
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

    echo ">>> Building dantetimesync from source..."
    export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"
    
    if [ ! -f "Cargo.toml" ]; then
        echo ">>> Cargo.toml not found. Cloning repository to build..."
        cd $(mktemp -d)
        git clone https://github.com/zbynekdrlik/dantetimesync.git .
    fi

    cargo build --release
    
    echo ">>> Stopping existing service..."
    systemctl stop dantetimesync 2>/dev/null || true

    echo ">>> Installing binary to /usr/local/bin/..."
    cp target/release/dantetimesync /usr/local/bin/
    chmod +x /usr/local/bin/dantetimesync
fi

# 4. Create Config Dir
echo ">>> Creating configuration directory..."
mkdir -p /etc/dantetimesync

# 5. Disable Conflicting Services
echo ">>> Disabling conflicting time services..."
systemctl stop systemd-timesyncd 2>/dev/null || true
systemctl disable systemd-timesyncd 2>/dev/null || true
systemctl stop chrony 2>/dev/null || true
systemctl disable chrony 2>/dev/null || true
systemctl stop ntp 2>/dev/null || true
systemctl disable ntp 2>/dev/null || true

# 6. Create Systemd Service
echo ">>> Creating systemd service..."
cat <<EOF > /etc/systemd/system/dantetimesync.service
[Unit]
Description=Dante PTP Time Sync Service v$VERSION
After=network-online.target
Wants=network-online.target

[Service]
# Run as root for port 319, adjtimex, and RTC ioctl access
User=root
Group=root
ExecStart=/usr/local/bin/dantetimesync
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
systemctl enable dantetimesync
systemctl restart dantetimesync

# 8. Final Verification
echo ">>> Verifying installation..."
INSTALLED_VERSION=$(/usr/local/bin/dantetimesync --version)
echo ">>> Installed: $INSTALLED_VERSION"

echo ">>> Installation Complete!"
echo ">>> Check status with: systemctl status dantetimesync"
echo ">>> View logs with: journalctl -u dantetimesync -f"