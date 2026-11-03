<#
.SYNOPSIS
  Mount a remote SSH host as a Windows drive (via SSHFS-Win / WinFsp) so any
  file manager -- Sigma File Manager, Explorer, etc. -- can browse it natively.

  Credentials are read from the same ssh.txt that ssh_grabber_rs uses:
      line 1: host
      line 2: username
      line 3: password
      line 4: port      (optional, default 22)

.EXAMPLES
  # Mount home dir to X: and open Sigma on it
  .\remote-mount.ps1 -LaunchSigma

  # Mount filesystem root to drive R:
  .\remote-mount.ps1 -Drive R -Root

  # Mount a specific remote subfolder (relative to home)
  .\remote-mount.ps1 -RemotePath "projects/data"

  # Unmount
  .\remote-mount.ps1 -Unmount
#>
[CmdletBinding()]
param(
    [string]$Drive       = "X",
    [string]$RemotePath  = "",
    [string]$SshTxt      = "$env:USERPROFILE\ssh.txt",
    [switch]$Root,
    [switch]$LaunchSigma,
    [switch]$Unmount,
    [switch]$InstallStartup,
    [switch]$RemoveStartup
)

$ErrorActionPreference = "Stop"
$Drive = $Drive.TrimEnd(':').ToUpper()
$DriveColon = "${Drive}:"

$SigmaAumid = "6609AlekseyHoffman.SigmaFileManager_3yjvzj1s1mye4!App"
$SshfsBin   = "C:\Program Files\SSHFS-Win\bin"

function Test-Mounted($d) {
    (net use) -match [regex]::Escape("$d") | Where-Object { $_ -match "sshfs" } | Select-Object -First 1
}

# ---- Persistence: install/remove a logon task ---------------------------
$TaskName = "SSHFS-RemoteMount-$Drive"

if ($RemoveStartup) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false -ErrorAction SilentlyContinue
    Write-Host "Removed logon task '$TaskName'. $DriveColon will no longer auto-mount." -ForegroundColor Green
    return
}

if ($InstallStartup) {
    $argline = "-NoProfile -WindowStyle Hidden -File `"$PSCommandPath`" -Drive $Drive"
    if ($Root)              { $argline += " -Root" }
    if ($RemotePath -ne "") { $argline += " -RemotePath `"$RemotePath`"" }
    if ($LaunchSigma)       { $argline += " -LaunchSigma" }

    $action   = New-ScheduledTaskAction -Execute "powershell.exe" -Argument $argline
    $trigger  = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
    $trigger.Delay = "PT15S"   # let the WinFsp service come up first
    $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries `
                    -DontStopIfGoingOnBatteries -StartWhenAvailable `
                    -ExecutionTimeLimit (New-TimeSpan -Hours 0)

    Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger `
        -Settings $settings -Force `
        -Description "Auto-mount remote SSH host as $DriveColon via SSHFS-Win" | Out-Null

    Write-Host "Installed logon task '$TaskName'. $DriveColon will mount at sign-in." -ForegroundColor Green
    Write-Host "Mounting now too..." -ForegroundColor Cyan
    # fall through to mount immediately as well
}

# ---- Unmount path -------------------------------------------------------
if ($Unmount) {
    Write-Host "Unmounting $DriveColon ..." -ForegroundColor Cyan
    net use $DriveColon /delete /yes 2>&1 | Out-Null
    if ($LASTEXITCODE -eq 0) { Write-Host "Unmounted $DriveColon." -ForegroundColor Green }
    else { Write-Host "Nothing mounted at $DriveColon (or already gone)." -ForegroundColor Yellow }
    return
}

# ---- Sanity checks ------------------------------------------------------
if (-not (Test-Path "$SshfsBin\sshfs.exe")) {
    throw "SSHFS-Win not found at $SshfsBin. Install with: winget install SSHFS-Win.SSHFS-Win"
}
if (-not (Test-Path $SshTxt)) {
    throw "Credentials file not found: $SshTxt"
}

# ---- Parse ssh.txt ------------------------------------------------------
$lines = Get-Content $SshTxt | ForEach-Object { $_.Trim() } | Where-Object { $_ -ne "" }
if ($lines.Count -lt 3) { throw "ssh.txt must have at least host, username, password." }
$remHost = $lines[0]
$user    = $lines[1]
$pass    = $lines[2]
$port    = if ($lines.Count -ge 4) { $lines[3] } else { "22" }

# ---- Build the SSHFS-Win UNC share path --------------------------------
# Prefix selects auth + base dir:
#   \\sshfs\    password auth, paths relative to remote HOME
#   \\sshfs.r\  password auth, paths relative to remote ROOT (/)
$prefix = if ($Root) { "\\sshfs.r" } else { "\\sshfs" }
$share  = "$prefix\$user@$remHost"
if ($port -ne "22") { $share += "!$port" }
if ($RemotePath -ne "") {
    $share += "\" + ($RemotePath -replace '/', '\')
}

# ---- Mount --------------------------------------------------------------
if (Test-Mounted $DriveColon) {
    Write-Host "$DriveColon already mounted; reusing it." -ForegroundColor Yellow
} else {
    Write-Host "Mounting $share  ->  $DriveColon" -ForegroundColor Cyan
    # net use: drive  share  <password>  /user:<user>
    net use $DriveColon $share $pass /user:"$user@$remHost" /persistent:no 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path "$DriveColon\")) {
        throw "Mount failed (exit $LASTEXITCODE). Check host/credentials, or that the WinFsp service is running."
    }
    Write-Host "Mounted. $remHost is now $DriveColon" -ForegroundColor Green
}

# ---- Open Sigma on it ---------------------------------------------------
if ($LaunchSigma) {
    Write-Host "Opening Sigma File Manager ..." -ForegroundColor Cyan
    Start-Process "explorer.exe" "shell:AppsFolder\$SigmaAumid"
}
