# Dante Time Sync Installer for Windows
# Run as Administrator in PowerShell

$ErrorActionPreference = "Stop"

$RepoOwner = "zbynekdrlik"
$RepoName = "dantetimesync"
$InstallDir = "C:\Program Files\DanteTimeSync"
$ServiceName = "dantetimesync"

Write-Host ">>> Dante Time Sync Windows Installer <<<" -ForegroundColor Cyan

# 1. Check for Npcap/WinPcap
if (!(Test-Path "C:\Windows\System32\Packet.dll")) {
    Write-Warning "Npcap or WinPcap does not appear to be installed (Packet.dll missing)."
    Write-Host "Please install Npcap from https://npcap.com/dist/npcap-1.79.exe (Select 'Install Npcap in WinPcap API-compatible Mode')" -ForegroundColor Yellow
    Write-Host "Press Enter to continue if you have installed it, or Ctrl+C to exit..."
    Read-Host
}

# 2. Create Directory
if (!(Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# 3. Download Latest Release
Write-Host "Fetching latest release..."
try {
    $LatestReleaseUrl = "https://api.github.com/repos/$RepoOwner/$RepoName/releases/latest"
    $ReleaseInfo = Invoke-RestMethod -Uri $LatestReleaseUrl
} catch {
    Write-Error "Failed to fetch release info. Check internet connection."
}

# Use exact matching to avoid ambiguity
$Asset = $ReleaseInfo.assets | Where-Object { $_.name -eq "dantetimesync-windows-amd64.exe" } | Select-Object -First 1
$TrayAsset = $ReleaseInfo.assets | Where-Object { $_.name -eq "dantetray-windows-amd64.exe" } | Select-Object -First 1

if (!$Asset) {
    Write-Error "Could not find 'dantetimesync-windows-amd64.exe' in latest release."
}

$ExePath = "$InstallDir\dantetimesync.exe"
$TrayPath = "$InstallDir\dantetray.exe"

Write-Host "Downloading $($Asset.name)..."
Invoke-WebRequest -Uri $Asset.browser_download_url -OutFile $ExePath

if ($TrayAsset) {
    Write-Host "Downloading $($TrayAsset.name)..."
    Invoke-WebRequest -Uri $TrayAsset.browser_download_url -OutFile $TrayPath
} else {
    Write-Warning "Tray application ('dantetray-windows-amd64.exe') not found in latest release."
}

# 4. Install Service
Write-Host "Installing Service..."
# Stop if exists
$Service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($Service) {
    Write-Host "Stopping existing service..."
    Stop-Service -Name $ServiceName -Force
    Start-Sleep -Seconds 2
    
    # Remove existing service using sc.exe (more reliable for removal)
    Write-Host "Removing existing service..."
    $scDelete = sc.exe delete $ServiceName
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 1060) { # 1060 = does not exist
        Write-Warning "sc delete returned exit code $LASTEXITCODE"
    }
    Start-Sleep -Seconds 2
}

# Create Service using New-Service (PowerShell Cmdlet handles quoting/spaces better)
# binPath needs to include --service flag and quotes around path if it has spaces
# New-Service expects BinaryPathName to be the full command line
$BinPath = "`"$ExePath`" --service"

try {
    New-Service -Name $ServiceName -BinaryPathName $BinPath -DisplayName "Dante Time Sync" -StartupType Automatic -Description "Synchronizes system time with Dante PTP Master"
} catch {
    Write-Error "Failed to create service. Ensure you are running as Administrator. Error: $_"
}

# 5. Start Service
Write-Host "Starting Service..."
try {
    Start-Service -Name $ServiceName
} catch {
    Write-Error "Failed to start service. Check Event Viewer for details. Error: $_"
}

# 6. Setup Tray App (Startup)
if (Test-Path $TrayPath) {
    Write-Host "Setting up Tray App to run at startup..."
    
    # Unregister if exists to ensure update
    Unregister-ScheduledTask -TaskName "DanteTray" -Confirm:$false -ErrorAction SilentlyContinue

    $Trigger = New-ScheduledTaskTrigger -AtLogon
    $Action = New-ScheduledTaskAction -Execute $TrayPath
    $Principal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Highest
    Register-ScheduledTask -TaskName "DanteTray" -Trigger $Trigger -Action $Action -Principal $Principal -Force | Out-Null
    
    # Start it now if not running
    $TrayProcess = Get-Process -Name "dantetray" -ErrorAction SilentlyContinue
    if (!$TrayProcess) {
        Write-Host "Starting Tray App..."
        Start-Process -FilePath $TrayPath
    } else {
        Write-Host "Tray App is already running."
    }
}

Write-Host "Installation Complete!" -ForegroundColor Green
Write-Host "Service '$ServiceName' is running."
