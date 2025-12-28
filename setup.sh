#!/bin/bash
set -e

REPO="zbynekdrlik/dantesync"
# We strictly expect the binary name produced by our CI/CD pipeline
BIN_NAME="dantesync-linux-amd64"
TARGET_BIN="dantesync"
INSTALL_DIR="/usr/local/bin"
SERVICE_FILE="/etc/systemd/system/dantesync.service"
TEMP_BIN="/tmp/$BIN_NAME"

# Allow overriding NTP server via env var, default to 10.77.8.2
NTP_SERVER="${NTP_SERVER:-10.77.8.2}"

echo ">>> DanteSync Web Installer (SOTA) <<<"
echo ">>> Using NTP Server: $NTP_SERVER"

if [ "$EUID" -ne 0 ]; then
  echo "Error: Please run as root (sudo bash ...)"
  exit 1
fi

# 1. Determine Download URL (Latest Release)
echo ">>> Fetching latest release info..."
RELEASE_JSON=$(curl -s "https://api.github.com/repos/$REPO/releases/latest")
LATEST_URL=$(echo "$RELEASE_JSON" | grep "browser_download_url" | grep "$BIN_NAME\"" | cut -d '"' -f 4)

if [ -z "$LATEST_URL" ]; then
    echo "Error: Could not find release asset '$BIN_NAME' in $REPO."
    echo "The release might be building or the architecture is unsupported."
    exit 1
fi

# 2. Install System Dependencies (Runtime only)
echo ">>> Installing runtime dependencies..."
apt-get update -qq
apt-get install -y -qq util-linux curl

# 3. Stop Service (to release binary lock)
echo ">>> Stopping existing service (if any)..."
systemctl stop dantesync 2>/dev/null || true

# 4. Download Binary to Temp
echo ">>> Downloading binary from $LATEST_URL..."
curl -L -o "$TEMP_BIN" "$LATEST_URL"
chmod +x "$TEMP_BIN"

# 5. Install Binary (Atomic Move)
echo ">>> Installing binary to $INSTALL_DIR/$TARGET_BIN..."
mv -f "$TEMP_BIN" "$INSTALL_DIR/$TARGET_BIN"

# 6. Disable Conflicting Services
echo ">>> Disabling conflicting time services..."
systemctl stop systemd-timesyncd 2>/dev/null || true
systemctl disable systemd-timesyncd 2>/dev/null || true
systemctl stop chrony 2>/dev/null || true
systemctl disable chrony 2>/dev/null || true
systemctl stop ntp 2>/dev/null || true
systemctl disable ntp 2>/dev/null || true

# 7. Create Systemd Service
echo ">>> Creating systemd service..."
cat <<EOF > "$SERVICE_FILE"
[Unit]
Description=DanteSync PTP Time Sync Service
After=network-online.target
Wants=network-online.target

[Service]
User=root
Group=root
ExecStart=$INSTALL_DIR/$TARGET_BIN --ntp-server $NTP_SERVER
Restart=always
RestartSec=5
# Realtime Priority
CPUSchedulingPolicy=fifo
CPUSchedulingPriority=50

[Install]
WantedBy=multi-user.target
EOF

# 8. Enable and Start
echo ">>> Starting service..."
systemctl daemon-reload
systemctl enable dantesync
systemctl restart dantesync

echo ">>> Installation Complete!"
echo "Status: systemctl status dantesync"
echo "Logs:   journalctl -u dantesync -f"
