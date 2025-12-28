# Dante Time Sync Uninstaller for Windows
# Run as Administrator in PowerShell

$ErrorActionPreference = "Stop"

$ServiceName = "dantetimesync"
$InstallDir = "C:\Program Files\DanteTimeSync"
$DataDir = "C:\ProgramData\DanteTimeSync"

Write-Host ">>> Dante Time Sync Windows Uninstaller <<<" -ForegroundColor Cyan

# 1. Stop and remove tray app
Write-Host "Stopping tray application..."
Stop-Process -Name "dantetray" -Force -ErrorAction SilentlyContinue
Start-Sleep -Seconds 1

# 2. Remove scheduled task
Write-Host "Removing scheduled task..."
Unregister-ScheduledTask -TaskName "DanteTray" -Confirm:$false -ErrorAction SilentlyContinue

# 3. Remove registry startup entries
Write-Host "Removing registry startup entries..."
$RegPathCU = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
$RegPathLM = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run"

try {
    Remove-ItemProperty -Path $RegPathCU -Name "DanteTray" -ErrorAction SilentlyContinue
    Write-Host "  - Removed current user registry entry." -ForegroundColor Gray
} catch { }

try {
    Remove-ItemProperty -Path $RegPathLM -Name "DanteTray" -ErrorAction SilentlyContinue
    Write-Host "  - Removed machine-wide registry entry." -ForegroundColor Gray
} catch { }

# 3b. Remove Start Menu shortcut
Write-Host "Removing Start Menu shortcut..."
$StartMenuShortcut = "$env:ProgramData\Microsoft\Windows\Start Menu\Programs\Dante Time Sync.lnk"
try {
    if (Test-Path $StartMenuShortcut) {
        Remove-Item -Path $StartMenuShortcut -Force -ErrorAction SilentlyContinue
        Write-Host "  - Removed Start Menu shortcut" -ForegroundColor Gray
    }
} catch { }

# 3c. Remove Add/Remove Programs registry entry
Write-Host "Removing from Add/Remove Programs..."
$UninstallKey = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\DanteTimeSync"
try {
    if (Test-Path $UninstallKey) {
        Remove-Item -Path $UninstallKey -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host "  - Removed from Add/Remove Programs" -ForegroundColor Gray
    }
} catch { }

# 4. Stop and remove service
Write-Host "Stopping service..."
$Service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($Service) {
    Stop-Service -Name $ServiceName -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 2

    Write-Host "Removing service..."
    sc.exe delete $ServiceName | Out-Null
    Start-Sleep -Seconds 1
}

# Kill any remaining processes
Stop-Process -Name "dantetimesync" -Force -ErrorAction SilentlyContinue

# 5. Remove program files
Write-Host "Removing program files..."
if (Test-Path $InstallDir) {
    Remove-Item -Path $InstallDir -Recurse -Force -ErrorAction SilentlyContinue
    Write-Host "  - Removed $InstallDir" -ForegroundColor Gray
}

# 6. Ask about data directory (contains config and logs)
if (Test-Path $DataDir) {
    $Response = Read-Host "Remove configuration and logs at $DataDir? (y/N)"
    if ($Response -eq 'y' -or $Response -eq 'Y') {
        Remove-Item -Path $DataDir -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host "  - Removed $DataDir" -ForegroundColor Gray
    } else {
        Write-Host "  - Kept $DataDir (config and logs preserved)" -ForegroundColor Gray
    }
}

Write-Host "Uninstallation Complete!" -ForegroundColor Green
