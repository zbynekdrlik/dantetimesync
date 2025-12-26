# Dante Time Sync Installer for Windows
# Run as Administrator in PowerShell

$ErrorActionPreference = "Stop"

$RepoOwner = "zbynekdrlik"
$RepoName = "dantetimesync"
$InstallDir = "C:\Program Files\DanteTimeSync"
$ServiceName = "dantetimesync"
$DataDir = "C:\ProgramData\DanteTimeSync"

Write-Host ">>> Dante Time Sync Windows Installer <<<" -ForegroundColor Cyan

# 1. Check for Npcap/WinPcap (Required for High Precision)
if (!(Test-Path "C:\Windows\System32\Packet.dll")) {
    Write-Warning "Npcap or WinPcap does not appear to be installed (Packet.dll missing)."
    Write-Host "Downloading Npcap (Required for PTP precision)..."
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

# Disable Windows Time service to prevent conflicts
Write-Host "Disabling Windows Time service (W32Time)..."
Stop-Service -Name "W32Time" -Force -ErrorAction SilentlyContinue
Set-Service -Name "W32Time" -StartupType Disabled -ErrorAction SilentlyContinue

# Kill processes - try graceful close for tray first to avoid ghost icons
Write-Host "Checking for running processes..."
Stop-Process -Name "dantetimesync" -Force -ErrorAction SilentlyContinue

# Gracefully close dantetray by sending close message to its window
$trayProc = Get-Process -Name "dantetray" -ErrorAction SilentlyContinue
if ($trayProc) {
    Write-Host "  - Closing tray application gracefully..."
    # Try to close main window first (allows cleanup of tray icon)
    $trayProc.CloseMainWindow() | Out-Null
    Start-Sleep -Seconds 2
    # If still running, force kill
    if (!$trayProc.HasExited) {
        Stop-Process -Name "dantetray" -Force -ErrorAction SilentlyContinue
    }
}
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

# 8. Setup Tray App (Startup) - Dual approach for reliability
if (Test-Path $TrayPath) {
    Write-Host "Setting up Tray App to run at startup..."

    # Method 1: Scheduled Task (Primary - works for all users at logon)
    Write-Host "  - Registering scheduled task..."
    Unregister-ScheduledTask -TaskName "DanteTray" -Confirm:$false -ErrorAction SilentlyContinue
    $Trigger = New-ScheduledTaskTrigger -AtLogon
    $Action = New-ScheduledTaskAction -Execute $TrayPath
    $Principal = New-ScheduledTaskPrincipal -GroupId "BUILTIN\Users" -RunLevel Limited
    Register-ScheduledTask -TaskName "DanteTray" -Trigger $Trigger -Action $Action -Principal $Principal -Force | Out-Null

    # Method 2: Registry Run entry (Fallback - per-user, more reliable in some scenarios)
    Write-Host "  - Adding registry startup entry..."
    $RegPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
    try {
        # Set for current user
        Set-ItemProperty -Path $RegPath -Name "DanteTray" -Value "`"$TrayPath`"" -ErrorAction Stop
        Write-Host "    Registry entry added for current user." -ForegroundColor Gray
    } catch {
        Write-Warning "Failed to add registry entry: $_"
    }

    # Also add to HKLM for all users (requires admin, which we have)
    $RegPathLM = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"
    try {
        Set-ItemProperty -Path $RegPathLM -Name "DanteTray" -Value "`"$TrayPath`"" -ErrorAction Stop
        Write-Host "    Registry entry added for all users." -ForegroundColor Gray
    } catch {
        Write-Warning "Failed to add machine-wide registry entry: $_"
    }

    # Start tray in user's interactive session (works over SSH/remote)
    # Using scheduled task ensures it runs on the logged-in user's desktop
    $TrayProcess = Get-Process -Name "dantetray" -ErrorAction SilentlyContinue
    if (!$TrayProcess) {
        Write-Host "Starting Tray App in interactive session..."

        # Get the currently logged-in user
        $LoggedInUser = (Get-WmiObject -Class Win32_ComputerSystem).UserName
        if ($LoggedInUser) {
            # Create a one-time scheduled task to run immediately in user's session
            $TrayTaskName = "DanteTrayStart"
            Unregister-ScheduledTask -TaskName $TrayTaskName -Confirm:$false -ErrorAction SilentlyContinue

            $Action = New-ScheduledTaskAction -Execute $TrayPath
            $Principal = New-ScheduledTaskPrincipal -UserId $LoggedInUser -LogonType Interactive -RunLevel Limited
            $Settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries

            Register-ScheduledTask -TaskName $TrayTaskName -Action $Action -Principal $Principal -Settings $Settings -Force | Out-Null
            Start-ScheduledTask -TaskName $TrayTaskName -ErrorAction SilentlyContinue
            Start-Sleep -Seconds 2

            # Clean up the one-time task
            Unregister-ScheduledTask -TaskName $TrayTaskName -Confirm:$false -ErrorAction SilentlyContinue
            Write-Host "    Tray started for user: $LoggedInUser" -ForegroundColor Gray
        } else {
            # Fallback: try direct start (works if running interactively)
            Write-Host "    No interactive user detected, starting directly..."
            Start-Process -FilePath $TrayPath -ErrorAction SilentlyContinue
        }
    } else {
        Write-Host "Tray App is already running."
    }
}

# 9. Register in Add/Remove Programs (Windows "Installed Apps")
Write-Host "Registering in Windows Installed Apps..."
$UninstallKey = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\DanteTimeSync"

# Get version from executable
$FileVersion = $Version -replace '^v', ''  # Remove 'v' prefix if present

try {
    if (!(Test-Path $UninstallKey)) {
        New-Item -Path $UninstallKey -Force | Out-Null
    }

    Set-ItemProperty -Path $UninstallKey -Name "DisplayName" -Value "Dante Time Sync"
    Set-ItemProperty -Path $UninstallKey -Name "DisplayVersion" -Value $FileVersion
    Set-ItemProperty -Path $UninstallKey -Name "Publisher" -Value "Zbyněk Drlík"
    Set-ItemProperty -Path $UninstallKey -Name "InstallLocation" -Value $InstallDir
    Set-ItemProperty -Path $UninstallKey -Name "DisplayIcon" -Value "$TrayPath,0"
    Set-ItemProperty -Path $UninstallKey -Name "UninstallString" -Value "powershell -ExecutionPolicy Bypass -File `"$InstallDir\uninstall.ps1`""
    Set-ItemProperty -Path $UninstallKey -Name "NoModify" -Value 1 -Type DWord
    Set-ItemProperty -Path $UninstallKey -Name "NoRepair" -Value 1 -Type DWord
    Set-ItemProperty -Path $UninstallKey -Name "EstimatedSize" -Value 5120 -Type DWord  # ~5MB in KB

    Write-Host "  - Registered in Add/Remove Programs" -ForegroundColor Gray
} catch {
    Write-Warning "Failed to register in Add/Remove Programs: $_"
}

# Copy uninstall script to install directory
$UninstallScriptSource = Join-Path (Split-Path -Parent $MyInvocation.MyCommand.Path) "uninstall.ps1"
$UninstallScriptDest = "$InstallDir\uninstall.ps1"
if (Test-Path $UninstallScriptSource) {
    try {
        Copy-Item -Path $UninstallScriptSource -Destination $UninstallScriptDest -Force
        Write-Host "  - Uninstall script copied to $InstallDir" -ForegroundColor Gray
    } catch {
        Write-Warning "Failed to copy uninstall script: $_"
    }
}

Write-Host "Installation Complete!" -ForegroundColor Green
Write-Host "Service '$ServiceName' is running."
Write-Host "Logs available at: $DataDir\dantetimesync.log" -ForegroundColor Gray
Write-Host "Config available at: $DataDir\config.json" -ForegroundColor Gray
