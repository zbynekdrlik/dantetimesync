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
function Test-NpcapInstalled {
    # Check multiple indicators of Npcap installation
    $packetDll = Test-Path "C:\Windows\System32\Packet.dll"
    $npcapDll = Test-Path "C:\Windows\System32\Npcap\Packet.dll"
    $npcapService = Get-Service -Name "npcap" -ErrorAction SilentlyContinue
    return ($packetDll -or $npcapDll -or $npcapService)
}

function Test-InteractiveSession {
    # Check if we're running in an interactive session (not SSH/remote)
    # Method 1: Check for console window
    $hasConsole = [Environment]::UserInteractive
    # Method 2: Check if SSH client is in parent process chain
    $isSSH = $env:SSH_CLIENT -or $env:SSH_TTY -or $env:SSH_CONNECTION
    # Method 3: Check session type
    $sessionName = (Get-Process -Id $PID).SessionId
    $consoleSession = (Get-Process -Name "explorer" -ErrorAction SilentlyContinue | Select-Object -First 1).SessionId

    return ($hasConsole -and -not $isSSH -and ($sessionName -eq $consoleSession))
}

function Install-NpcapInteractive {
    # This function runs the actual GUI automation - must be called from interactive session
    param([string]$InstallerPath)

    # Load UIAutomation assemblies
    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes

    # Add mouse click helper using Windows API
    Add-Type @"
        using System;
        using System.Runtime.InteropServices;
        public class NpcapMouseHelper {
            [DllImport("user32.dll")]
            public static extern bool SetCursorPos(int X, int Y);
            [DllImport("user32.dll")]
            public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, int dwExtraInfo);
            public const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
            public const uint MOUSEEVENTF_LEFTUP = 0x0004;
            public static void Click(int x, int y) {
                SetCursorPos(x, y);
                mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
                mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
            }
        }
"@ -ErrorAction SilentlyContinue

    function Find-NpcapWindow {
        param([int]$TimeoutSeconds = 30)
        $rootElement = [System.Windows.Automation.AutomationElement]::RootElement
        $condition = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Window)
        $elapsed = 0
        while ($elapsed -lt $TimeoutSeconds) {
            $windows = $rootElement.FindAll([System.Windows.Automation.TreeScope]::Children, $condition)
            foreach ($win in $windows) {
                $name = $win.Current.Name
                if ($name -and $name -like "*Npcap*Setup*") { return $win }
            }
            Start-Sleep -Milliseconds 500
            $elapsed += 0.5
        }
        return $null
    }

    function Click-NpcapButton {
        param([System.Windows.Automation.AutomationElement]$Window, [string]$ButtonName)
        $titleBarIds = @("Minimize", "Maximize", "Close", "SmallDecrement", "SmallIncrement", "LargeDecrement", "LargeIncrement")
        $btnCondition = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Button)
        $buttons = $Window.FindAll([System.Windows.Automation.TreeScope]::Descendants, $btnCondition)

        foreach ($btn in $buttons) {
            $name = $btn.Current.Name
            $autoId = $btn.Current.AutomationId
            if ($titleBarIds -contains $autoId) { continue }
            if ($name -eq $ButtonName -or ($name -like "*$ButtonName*" -and $name -notlike "*Cancel*" -and $name -notlike "*Back*")) {
                try {
                    $rect = $btn.Current.BoundingRectangle
                    $x = [int]($rect.X + $rect.Width / 2)
                    $y = [int]($rect.Y + $rect.Height / 2)
                    [NpcapMouseHelper]::Click($x, $y)
                    Start-Sleep -Milliseconds 500
                    return $true
                } catch { }
            }
        }
        return $false
    }

    # Kill any existing Npcap installer processes
    Get-Process | Where-Object { $_.Name -like "*npcap*" } | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1

    # Start installer with pre-selected options
    $installerArgs = "/winpcap_mode=yes /loopback_support=no /dot11_support=no /vlan_support=no /admin_only=no /disable_restore_point=yes"
    $process = Start-Process -FilePath $InstallerPath -ArgumentList $installerArgs -PassThru
    Start-Sleep -Seconds 2

    # Step 1: Click "I Agree"
    $window = Find-NpcapWindow -TimeoutSeconds 30
    if (-not $window) { return $false }
    Start-Sleep -Milliseconds 500
    if (-not (Click-NpcapButton -Window $window -ButtonName "I Agree")) { return $false }

    # Step 2: Click "Install"
    Start-Sleep -Seconds 1
    $window = Find-NpcapWindow -TimeoutSeconds 10
    if ($window) { Click-NpcapButton -Window $window -ButtonName "Install" | Out-Null }

    # Step 3: Wait for driver installation
    Start-Sleep -Seconds 20

    # Step 4: Click through all remaining screens (Next, Finish, Close)
    $maxWait = 90
    $waited = 0
    while ($waited -lt $maxWait) {
        $window = Find-NpcapWindow -TimeoutSeconds 3
        if (-not $window) { break }  # Window closed = done

        # Try clicking any forward/finish button
        $clicked = Click-NpcapButton -Window $window -ButtonName "Next"
        if (-not $clicked) { $clicked = Click-NpcapButton -Window $window -ButtonName "Finish" }
        if (-not $clicked) { $clicked = Click-NpcapButton -Window $window -ButtonName "Close" }

        if ($clicked) {
            Start-Sleep -Seconds 1
        } else {
            Start-Sleep -Seconds 2
            $waited += 2
        }
    }

    # Ensure process exits
    if (-not $process.HasExited) { $process.WaitForExit(10000) }
    Start-Sleep -Seconds 2

    # Verify installation
    $packetDll = Test-Path "C:\Windows\System32\Packet.dll"
    $npcapService = Get-Service -Name "npcap" -ErrorAction SilentlyContinue
    return ($packetDll -or $npcapService)
}

function Install-NpcapWithAutomation {
    param([string]$InstallerPath)

    Write-Host "Attempting automated Npcap installation..." -ForegroundColor Yellow

    $isInteractive = Test-InteractiveSession

    if ($isInteractive) {
        Write-Host "  Running in interactive session - using direct GUI automation" -ForegroundColor Gray
        return Install-NpcapInteractive -InstallerPath $InstallerPath
    } else {
        Write-Host "  Running via SSH/remote - using scheduled task for GUI automation" -ForegroundColor Gray

        # Create a temporary script that will run in the interactive session
        $tempScript = "$env:TEMP\npcap-install-task.ps1"
        $resultFile = "$env:TEMP\npcap-install-result.txt"

        # Remove old result file
        Remove-Item $resultFile -Force -ErrorAction SilentlyContinue

        # Write the installation script - use Set-Content to write the script as a single string
        # This avoids encoding issues with line-by-line array output
        $csharpCode = @'
using System;
using System.Runtime.InteropServices;
public class NpcapMouseHelper {
    [DllImport("user32.dll")]
    public static extern bool SetCursorPos(int X, int Y);
    [DllImport("user32.dll")]
    public static extern void mouse_event(uint dwFlags, int dx, int dy, uint dwData, int dwExtraInfo);
    public const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
    public const uint MOUSEEVENTF_LEFTUP = 0x0004;
    public static void Click(int x, int y) {
        SetCursorPos(x, y);
        mouse_event(MOUSEEVENTF_LEFTDOWN, 0, 0, 0, 0);
        mouse_event(MOUSEEVENTF_LEFTUP, 0, 0, 0, 0);
    }
}
'@
        # Escape the C# code for embedding in the script
        $csharpCodeEscaped = $csharpCode -replace "'", "''"

        $scriptContent = @"
`$ErrorActionPreference = 'Continue'
try {
    Add-Type -AssemblyName UIAutomationClient
    Add-Type -AssemblyName UIAutomationTypes

    `$mouseCode = '$csharpCodeEscaped'
    Add-Type -TypeDefinition `$mouseCode -ErrorAction SilentlyContinue

    function Find-NpcapWindow {
        param([int]`$TimeoutSeconds = 30)
        `$rootElement = [System.Windows.Automation.AutomationElement]::RootElement
        `$condition = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Window)
        `$elapsed = 0
        while (`$elapsed -lt `$TimeoutSeconds) {
            `$windows = `$rootElement.FindAll([System.Windows.Automation.TreeScope]::Children, `$condition)
            foreach (`$win in `$windows) {
                `$name = `$win.Current.Name
                if (`$name -and `$name -like '*Npcap*Setup*') { return `$win }
            }
            Start-Sleep -Milliseconds 500
            `$elapsed += 0.5
        }
        return `$null
    }

    function Click-NpcapButton {
        param([System.Windows.Automation.AutomationElement]`$Window, [string]`$ButtonName)
        `$titleBarIds = @('Minimize', 'Maximize', 'Close', 'SmallDecrement', 'SmallIncrement', 'LargeDecrement', 'LargeIncrement')
        `$btnCondition = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
            [System.Windows.Automation.ControlType]::Button)
        `$buttons = `$Window.FindAll([System.Windows.Automation.TreeScope]::Descendants, `$btnCondition)
        foreach (`$btn in `$buttons) {
            `$name = `$btn.Current.Name
            `$autoId = `$btn.Current.AutomationId
            if (`$titleBarIds -contains `$autoId) { continue }
            if (`$name -eq `$ButtonName -or (`$name -like "*`$ButtonName*" -and `$name -notlike '*Cancel*' -and `$name -notlike '*Back*')) {
                try {
                    `$rect = `$btn.Current.BoundingRectangle
                    `$x = [int](`$rect.X + `$rect.Width / 2)
                    `$y = [int](`$rect.Y + `$rect.Height / 2)
                    [NpcapMouseHelper]::Click(`$x, `$y)
                    Start-Sleep -Milliseconds 500
                    return `$true
                } catch { }
            }
        }
        return `$false
    }

    # Kill existing installers
    Get-Process | Where-Object { `$_.Name -like '*npcap*' } | Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Sleep -Seconds 1

    `$process = Start-Process -FilePath '$InstallerPath' -ArgumentList '/winpcap_mode=yes /loopback_support=no /dot11_support=no /vlan_support=no /admin_only=no /disable_restore_point=yes' -PassThru
    Start-Sleep -Seconds 2

    # Click I Agree
    `$window = Find-NpcapWindow -TimeoutSeconds 30
    if (`$window) {
        Start-Sleep -Milliseconds 500
        Click-NpcapButton -Window `$window -ButtonName 'I Agree' | Out-Null
    }

    # Click Install
    Start-Sleep -Seconds 1
    `$window = Find-NpcapWindow -TimeoutSeconds 10
    if (`$window) { Click-NpcapButton -Window `$window -ButtonName 'Install' | Out-Null }

    # Wait for driver installation
    Start-Sleep -Seconds 20

    # Click through remaining screens
    `$maxWait = 90
    `$waited = 0
    while (`$waited -lt `$maxWait) {
        `$window = Find-NpcapWindow -TimeoutSeconds 3
        if (-not `$window) { break }
        `$clicked = Click-NpcapButton -Window `$window -ButtonName 'Next'
        if (-not `$clicked) { `$clicked = Click-NpcapButton -Window `$window -ButtonName 'Finish' }
        if (-not `$clicked) { `$clicked = Click-NpcapButton -Window `$window -ButtonName 'Close' }
        if (`$clicked) { Start-Sleep -Seconds 1 } else { Start-Sleep -Seconds 2; `$waited += 2 }
    }

    if (-not `$process.HasExited) { `$process.WaitForExit(10000) }
    Start-Sleep -Seconds 2

    # Write result
    `$packetDll = Test-Path 'C:\Windows\System32\Packet.dll'
    `$npcapService = Get-Service -Name 'npcap' -ErrorAction SilentlyContinue
    if (`$packetDll -or `$npcapService) {
        'SUCCESS' | Out-File '$resultFile'
    } else {
        'FAILED' | Out-File '$resultFile'
    }
} catch {
    `$_.Exception.Message | Out-File '$resultFile'
}
"@
        Set-Content -Path $tempScript -Value $scriptContent -Encoding UTF8

        # Get interactive user
        $loggedInUser = (Get-WmiObject -Class Win32_ComputerSystem).UserName
        if (-not $loggedInUser) {
            Write-Warning "No interactive user logged in - cannot run GUI automation"
            return $false
        }

        Write-Host "  Creating scheduled task for user: $loggedInUser" -ForegroundColor Gray

        # Create and run scheduled task in interactive session
        $taskName = "NpcapInstall_$(Get-Random)"
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue

        $action = New-ScheduledTaskAction -Execute "powershell.exe" -Argument "-ExecutionPolicy Bypass -WindowStyle Hidden -File `"$tempScript`""
        $principal = New-ScheduledTaskPrincipal -UserId $loggedInUser -LogonType Interactive -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries

        Register-ScheduledTask -TaskName $taskName -Action $action -Principal $principal -Settings $settings -Force | Out-Null
        Start-ScheduledTask -TaskName $taskName

        Write-Host "  Waiting for Npcap installation to complete..." -ForegroundColor Gray

        # Wait for task to complete (max 2 minutes)
        $maxWait = 120
        $waited = 0
        while ($waited -lt $maxWait) {
            Start-Sleep -Seconds 5
            $waited += 5

            if (Test-Path $resultFile) {
                $result = Get-Content $resultFile -Raw
                break
            }

            $taskInfo = Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue
            if ($taskInfo -and $taskInfo.State -eq "Ready") {
                # Task finished but no result file - check directly
                if (Test-NpcapInstalled) {
                    $result = "SUCCESS"
                    break
                }
            }

            Write-Host "    Still installing... ($waited seconds)" -ForegroundColor Gray
        }

        # Cleanup
        Unregister-ScheduledTask -TaskName $taskName -Confirm:$false -ErrorAction SilentlyContinue
        Remove-Item $tempScript -Force -ErrorAction SilentlyContinue
        Remove-Item $resultFile -Force -ErrorAction SilentlyContinue

        if ($result -and $result.Trim() -eq "SUCCESS") {
            Write-Host "  Npcap installed successfully!" -ForegroundColor Green
            return $true
        } else {
            Write-Warning "Npcap installation failed: $result"
            return $false
        }
    }
}

if (!(Test-NpcapInstalled)) {
    Write-Host ""
    Write-Host "========================================" -ForegroundColor Red
    Write-Host "  Npcap is NOT installed!" -ForegroundColor Red
    Write-Host "========================================" -ForegroundColor Red
    Write-Host ""
    Write-Host "Npcap is REQUIRED for high-precision PTP time synchronization." -ForegroundColor Yellow
    Write-Host "The free version of Npcap does not support silent installation." -ForegroundColor Gray
    Write-Host ""

    $NpcapVersion = "1.85"
    $NpcapUrl = "https://npcap.com/dist/npcap-$NpcapVersion.exe"
    $NpcapPath = "$env:TEMP\npcap-$NpcapVersion.exe"

    # Check if running interactively or via SSH/remote
    $isInteractive = Test-InteractiveSession

    if ($isInteractive) {
        # Ask user what they want to do
        Write-Host "Options:" -ForegroundColor Cyan
        Write-Host "  [1] Attempt automated installation (downloads Npcap, tries to click through GUI)"
        Write-Host "  [2] Open Npcap download page (manual installation)"
        Write-Host "  [3] Abort installation"
        Write-Host ""

        $choice = Read-Host "Enter choice (1/2/3)"
    } else {
        # Running via SSH - auto-select automated installation
        Write-Host "Detected non-interactive session (SSH/remote)" -ForegroundColor Yellow
        Write-Host "Automatically attempting Npcap installation..." -ForegroundColor Cyan
        $choice = "1"
    }

    switch ($choice) {
        "1" {
            Write-Host ""
            Write-Host "Downloading Npcap $NpcapVersion..." -ForegroundColor Cyan
            try {
                Invoke-WebRequest -Uri $NpcapUrl -OutFile $NpcapPath
                Write-Host "Downloaded to: $NpcapPath" -ForegroundColor Gray

                $success = Install-NpcapWithAutomation -InstallerPath $NpcapPath

                if ($success) {
                    Write-Host "Npcap installed successfully!" -ForegroundColor Green
                } else {
                    Write-Host ""
                    Write-Host "Npcap installation could not be verified." -ForegroundColor Red
                    Write-Host "The installer may still be running - please complete it manually." -ForegroundColor Yellow
                    Write-Host ""
                    Write-Host "After installing Npcap, run this installer again." -ForegroundColor Cyan
                    if ($isInteractive) { Read-Host "Press Enter to exit" }
                    exit 1
                }
            } catch {
                Write-Error "Failed to download Npcap: $_"
                Write-Host "Please download manually from: $NpcapUrl" -ForegroundColor Yellow
                if ($isInteractive) { Read-Host "Press Enter to exit" }
                exit 1
            }
        }
        "2" {
            Write-Host ""
            Write-Host "Opening Npcap download page..." -ForegroundColor Cyan
            Start-Process "https://npcap.com/#download"
            Write-Host ""
            Write-Host "After installing Npcap, run this installer again." -ForegroundColor Yellow
            if ($isInteractive) { Read-Host "Press Enter to exit" }
            exit 1
        }
        default {
            Write-Host ""
            Write-Host "Installation aborted." -ForegroundColor Yellow
            exit 1
        }
    }

    # Final verification
    if (!(Test-NpcapInstalled)) {
        Write-Host ""
        Write-Host "ERROR: Npcap is still not detected after installation attempt." -ForegroundColor Red
        Write-Host "Please install Npcap manually from: $NpcapUrl" -ForegroundColor Yellow
        Write-Host "Then run this installer again." -ForegroundColor Yellow
        if ($isInteractive) { Read-Host "Press Enter to exit" }
        exit 1
    }

    Write-Host ""
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

# Copy uninstall script to install directory (only works when run from file, not irm | iex)
$ScriptPath = $MyInvocation.MyCommand.Path
if ($ScriptPath) {
    $UninstallScriptSource = Join-Path (Split-Path -Parent $ScriptPath) "uninstall.ps1"
    $UninstallScriptDest = "$InstallDir\uninstall.ps1"
    if (Test-Path $UninstallScriptSource) {
        try {
            Copy-Item -Path $UninstallScriptSource -Destination $UninstallScriptDest -Force
            Write-Host "  - Uninstall script copied to $InstallDir" -ForegroundColor Gray
        } catch {
            Write-Warning "Failed to copy uninstall script: $_"
        }
    }
} else {
    # Running via irm | iex - download uninstall script from GitHub
    $UninstallScriptDest = "$InstallDir\uninstall.ps1"
    try {
        $UninstallUrl = "https://raw.githubusercontent.com/$RepoOwner/$RepoName/master/uninstall.ps1"
        Invoke-WebRequest -Uri $UninstallUrl -OutFile $UninstallScriptDest
        Write-Host "  - Uninstall script downloaded to $InstallDir" -ForegroundColor Gray
    } catch {
        Write-Warning "Failed to download uninstall script: $_"
    }
}

Write-Host "Installation Complete!" -ForegroundColor Green
Write-Host "Service '$ServiceName' is running."
Write-Host "Logs available at: $DataDir\dantetimesync.log" -ForegroundColor Gray
Write-Host "Config available at: $DataDir\config.json" -ForegroundColor Gray
