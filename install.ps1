# Dante Time Sync Installer for Windows
# Run as Administrator in PowerShell

$ErrorActionPreference = "Stop"

$RepoOwner = "zbynekdrlik"
$RepoName = "dantetimesync"
$InstallDir = "C:\Program Files\DanteTimeSync"
$ServiceName = "dantetimesync"
$DataDir = "C:\ProgramData\DanteTimeSync"

Write-Host ">>> Dante Time Sync Windows Installer <<<" -ForegroundColor Cyan

# 1. Check for Npcap/WinPcap
if (!(Test-Path "C:\Windows\System32\Packet.dll")) {
    Write-Warning "Npcap or WinPcap does not appear to be installed (Packet.dll missing)."
    
    $InstallNpcap = Read-Host "Do you want to download and install Npcap automatically? (Y/n) [Default: Y]"
    if ($InstallNpcap -eq 'Y' -or $InstallNpcap -eq 'y' -or $InstallNpcap -eq '') {
        Write-Host "Downloading Npcap..."
        $NpcapUrl = "https://npcap.com/dist/npcap-1.79.exe"
        $NpcapPath = "$env:TEMP\npcap-1.79.exe"
        try {
            Invoke-WebRequest -Uri $NpcapUrl -OutFile $NpcapPath
            Write-Host "Installing Npcap (Silent mode, WinPcap compatibility enabled)..."
            Start-Process -FilePath $NpcapPath -ArgumentList "/S", "/winpcap_mode=yes" -Wait
            Write-Host "Npcap installed successfully."
        } catch {
            Write-Error "Failed to install Npcap automatically: $_"
            Write-Host "Please install it manually from $NpcapUrl"
            Read-Host "Press Enter to continue..."
        }
    } else {
        Write-Host "Please install Npcap from https://npcap.com/dist/npcap-1.79.exe (Select 'Install Npcap in WinPcap API-compatible Mode')" -ForegroundColor Yellow
        Write-Host "Press Enter to continue if you have installed it, or Ctrl+C to exit..."
        Read-Host
    }
}

# 2. Create Directories
if (!(Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}

# Create Data Directory (ProgramData) and set permissions
if (!(Test-Path $DataDir)) {
    New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
}

# Grant Users Modify access to DataDir (for Config editing)
try {
    $Acl = Get-Acl $DataDir
    $Rule = New-Object System.Security.AccessControl.FileSystemAccessRule("BUILTIN\Users","Modify","ContainerInherit,ObjectInherit","None","Allow")
    $Acl.AddAccessRule($Rule)
    Set-Acl $DataDir $Acl
} catch {
    Write-Warning "Failed to set permissions on $DataDir. You might need Admin rights to edit config."
}

# 3. Fetch Latest Release Info
Write-Host "Fetching latest release info..."
try {
    $LatestReleaseUrl = "https://api.github.com/repos/$RepoOwner/$RepoName/releases/latest"
    $ReleaseInfo = Invoke-RestMethod -Uri $LatestReleaseUrl
} catch {
    Write-Error "Failed to fetch release info. Check internet connection."
}

$Version = $ReleaseInfo.tag_name
Write-Host "Installing Version: $Version" -ForegroundColor Green

# Use exact matching to avoid ambiguity
$Asset = $ReleaseInfo.assets | Where-Object { $_.name -eq "dantetimesync-windows-amd64.exe" } | Select-Object -First 1
$TrayAsset = $ReleaseInfo.assets | Where-Object { $_.name -eq "dantetray-windows-amd64.exe" } | Select-Object -First 1

if (!$Asset) {
    Write-Error "Could not find 'dantetimesync-windows-amd64.exe' in latest release."
}

$ExePath = "$InstallDir\dantetimesync.exe"
$TrayPath = "$InstallDir\dantetray.exe"

# 4. Stop & Remove Existing Service/Processes (CRITICAL: Do this BEFORE download)
Write-Host "Stopping services and processes..."

# Stop Service
$Service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($Service) {
    Write-Host "Stopping existing service '$ServiceName'..."
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2
    
    # Remove existing service using sc.exe (more reliable for removal)
    Write-Host "Removing existing service entry..."
    $scDelete = sc.exe delete $ServiceName
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 1060) { # 1060 = does not exist
        Write-Warning "sc delete returned exit code $LASTEXITCODE"
    }
    Start-Sleep -Seconds 1
}

# Kill processes forcefully to release file locks
Write-Host "Checking for running processes..."
Stop-Process -Name "dantetimesync" -Force -ErrorAction SilentlyContinue
Stop-Process -Name "dantetray" -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 1

# 5. Download Files
Write-Host "Downloading $($Asset.name)..."
try {
    Invoke-WebRequest -Uri $Asset.browser_download_url -OutFile $ExePath
} catch {
    Write-Error "Failed to download main executable. Ensure the file is not open. Error: $_"
}

if ($TrayAsset) {
    Write-Host "Downloading $($TrayAsset.name)..."
    try {
        Invoke-WebRequest -Uri $TrayAsset.browser_download_url -OutFile $TrayPath
    } catch {
        Write-Warning "Failed to download tray app. Error: $_"
    }
} else {
    Write-Warning "Tray application ('dantetray-windows-amd64.exe') not found in latest release."
}

# 6. Install Service
Write-Host "Installing Service..."

# Create Service using New-Service
$BinPath = "`"$ExePath`" --service"

try {
    New-Service -Name $ServiceName -BinaryPathName $BinPath -DisplayName "Dante Time Sync" -StartupType Automatic -Description "Synchronizes system time with Dante PTP Master"
} catch {
    Write-Error "Failed to create service. Ensure you are running as Administrator. Error: $_"
}

# 7. Start Service
Write-Host "Starting Service..."
try {
    Start-Service -Name $ServiceName
} catch {
    Write-Error "Failed to start service. Check Event Viewer for details. Error: $_"
}

# 8. Setup Tray App (Startup)
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
Write-Host "Logs available at: $DataDir\dantetimesync.log" -ForegroundColor Gray
Write-Host "Config available at: $DataDir\config.json" -ForegroundColor Gray
