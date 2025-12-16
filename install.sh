#!/bin/bash
set -e

echo ">>> Dante Time Sync Installer <<<"

if [ "$EUID" -ne 0 ]; then
  echo "Error: Please run as root (sudo ./install.sh)"
  exit 1
fi

# 1. Install System Dependencies
echo ">>> Installing system dependencies..."
apt-get update
# util-linux provides hwclock (if available on platform)
apt-get install -y build-essential curl util-linux

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
    # Ensure cargo is in path
    export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"
    
    # We assume the script is run inside the repo or we need to clone it?
    # The curl | bash usage assumes we are NOT in the repo usually?
    # Wait! "curl ... | bash" runs the script content.
    # The script says "cargo build --release".
    # This assumes the Current Working Directory IS the repo.
    # But "curl ... | bash" runs in whatever dir the user is in.
    # If the user runs it from /home/user, "cargo build" fails because no Cargo.toml.
    
    # The WINDOWS installer downloads the EXE.
    # The LINUX installer (as written previously) assumed user cloned the repo?
    # My instructions were: "git clone ... cd ... sudo ./install.sh".
    # So CWD is repo.
    
    # BUT, if the user wants "curl | bash" ONE LINER without cloning?
    # Then I MUST clone inside the script if I need to build.
    # OR the download method is the ONLY way for "curl | bash" without clone.
    
    # If I want to support "curl | bash" for Source Build, I must git clone to temp dir.
    
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
# We disable these to prevent them from fighting for the clock
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
Description=Dante PTP Time Sync Service
After=network-online.target
Wants=network-online.target

[Service]
# Run as root for port 319, adjtimex, and RTC ioctl access
User=root
Group=root
ExecStart=/usr/local/bin/dantetimesync
Restart=always
RestartSec=5
# High priority for timestamping accuracy (Redundant with internal code but good practice)
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

echo ">>> Installation Complete!"
echo ">>> Check status with: systemctl status dantetimesync"
echo ">>> View logs with: journalctl -u dantetimesync -f"
