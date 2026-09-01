#Requires -Version 5.1
<#
.SYNOPSIS
    Reports Smart App Control (SAC) / Code Integrity state and recent CodeIntegrity
    block/audit events, so you can tell at a glance why a binary refused to run.

.DESCRIPTION
    SAC is a whole-machine Code Integrity policy: it evaluates every PE as it is
    loaded and hard-blocks anything that is neither signed by a trusted publisher
    nor blessed by Microsoft's Intelligent Security Graph. A blocked launch
    surfaces to a caller as `os error 4551`
    (ERROR_VIRUS_INFECTED / "Operation did not complete successfully because the
    file contains a virus or potentially unwanted software"), which is why cargo
    build failures caused by SAC look nothing like code-signing failures.

    This script prints, in order:
      1. OS edition / build / UBR, and whether the build is at or past the
         KB5079391 baseline (26200.8116) at which SAC can be switched back On
         after having been turned Off.
      2. The SAC registry state under HKLM\SYSTEM\CurrentControlSet\Control\CI\Policy.
      3. Active Code Integrity policies via CiTool.exe -lp (needs elevation).
      4. The last N CodeIntegrity 3076 (audit) / 3077 (block) events, with the
         blocked file and the launching process as separate columns.
      5. A summary of how many blocks were seen and how many touched this repo.

    Everything except section 3 (and, on some machines, section 4) works unelevated.
    Nothing here throws on missing privileges; it says so and moves on.

.PARAMETER Count
    How many CodeIntegrity 3076/3077 events to display. Default 20.

.EXAMPLE
    pwsh -File scripts/sac-status.ps1
    powershell -ExecutionPolicy Bypass -File scripts/sac-status.ps1 -Count 100

.NOTES
    See docs/SIGNING.md for what to do about what this reports.
#>
[CmdletBinding()]
param(
    [ValidateRange(1, 1000)]
    [int] $Count = 20
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Continue'

$CodeIntegrityLog = 'Microsoft-Windows-CodeIntegrity/Operational'
# Builds at or above this can re-enable SAC from Settings after it was turned Off
# (KB5079391 restored the previously one-way "Off" transition).
$SacReEnableBaseline = [version]'26200.8116'
# Event 3077 = block, 3076 = audit-mode "would have blocked".
$CodeIntegrityEventIds = @(3076, 3077)

function Write-Section {
    param([Parameter(Mandatory)][string] $Title)
    Write-Host ''
    Write-Host (('== {0} ' -f $Title).PadRight(78, '=')) -ForegroundColor Cyan
}

function Write-Field {
    param(
        [Parameter(Mandatory)][string] $Name,
        $Value,
        [string] $Note
    )
    $rendered = if ($null -eq $Value -or "$Value" -eq '') { '(not set)' } else { "$Value" }
    $line = '  {0,-34} {1}' -f ($Name + ':'), $rendered
    if ($Note) { $line = '{0}   [{1}]' -f $line, $Note }
    Write-Host $line
}

function Test-IsElevated {
    try {
        $id = [Security.Principal.WindowsIdentity]::GetCurrent()
        return (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(
            [Security.Principal.WindowsBuiltInRole]::Administrator)
    } catch {
        return $false
    }
}

# Maps \Device\HarddiskVolumeN\... (the form CodeIntegrity logs use) back to a
# drive-lettered path. Falls back to the raw NT path if the lookup is unavailable.
$script:NtVolumeMap = $null
function Get-NtVolumeMap {
    if ($null -ne $script:NtVolumeMap) { return $script:NtVolumeMap }
    $map = @{}
    try {
        if (-not ('YSpot.NativeVolume' -as [type])) {
            Add-Type -Namespace 'YSpot' -Name 'NativeVolume' -MemberDefinition @'
[System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true, CharSet = System.Runtime.InteropServices.CharSet.Unicode)]
public static extern uint QueryDosDeviceW(string lpDeviceName, System.Text.StringBuilder lpTargetPath, uint ucchMax);
'@
        }
        foreach ($drive in [System.IO.DriveInfo]::GetDrives()) {
            $letter = $drive.Name.TrimEnd('\')   # e.g. "C:"
            $sb = New-Object System.Text.StringBuilder 1024
            if ([YSpot.NativeVolume]::QueryDosDeviceW($letter, $sb, 1024) -ne 0) {
                $target = $sb.ToString()
                if ($target -and -not $map.ContainsKey($target)) { $map[$target] = $letter }
            }
        }
    } catch {
        Write-Verbose ("volume map unavailable: {0}" -f $_.Exception.Message)
    }
    $script:NtVolumeMap = $map
    return $map
}

function Convert-NtPath {
    param([string] $Path)
    if (-not $Path) { return '' }
    $map = Get-NtVolumeMap
    foreach ($device in $map.Keys) {
        if ($Path.StartsWith($device, [System.StringComparison]::OrdinalIgnoreCase)) {
            return $map[$device] + $Path.Substring($device.Length)
        }
    }
    return $Path
}

function Get-EventDataMap {
    param([Parameter(Mandatory)] $Event)
    $map = @{}
    try {
        $xml = [xml] $Event.ToXml()
        $data = $xml.Event.EventData.Data
        if ($data) {
            foreach ($node in $data) {
                # Named fields only. Parsing the rendered Message instead is a trap:
                # it prints the *parent* process in parentheses, which is how these
                # logs get misread as "cargo.exe was blocked" when cargo only
                # launched the thing that was blocked.
                if ($node.Name) { $map[[string]$node.Name] = [string]$node.'#text' }
            }
        }
    } catch {
        Write-Verbose ("could not parse event XML: {0}" -f $_.Exception.Message)
    }
    return $map
}

function Get-PolicyStateMeaning {
    param($Value)
    switch ("$Value") {
        '0'     { 'Off - no Smart App Control policy is applied' }
        '1'     { 'On / ENFORCEMENT - unsigned, unreputable binaries are blocked' }
        '2'     { 'Evaluation - Windows is deciding whether to auto-enable enforcement' }
        default { 'unknown value' }
    }
}

# --------------------------------------------------------------------------
Write-Host ''
Write-Host 'YSpot - Smart App Control / Code Integrity status' -ForegroundColor Green
Write-Host ("Run at {0}{1}" -f (Get-Date), $(if (Test-IsElevated) { ' (elevated)' } else { ' (not elevated)' }))

# --- 1. OS ----------------------------------------------------------------
Write-Section 'Operating system'
$build = $null
$ubr = $null
try {
    $cv = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion' -ErrorAction Stop
    $build = [int] $cv.CurrentBuild
    $ubr = [int] $cv.UBR

    $caption = $null
    try { $caption = (Get-CimInstance Win32_OperatingSystem -ErrorAction Stop).Caption } catch { }
    if (-not $caption) {
        # CurrentVersion\ProductName still says "Windows 10" on Windows 11.
        $caption = $cv.ProductName
    }

    Write-Field 'Product'        $caption
    Write-Field 'Edition ID'     $cv.EditionID
    Write-Field 'Display version' $cv.DisplayVersion
    Write-Field 'Build.UBR'      ('{0}.{1}' -f $build, $ubr)
} catch {
    Write-Host ("  Could not read OS version from the registry: {0}" -f $_.Exception.Message) -ForegroundColor Yellow
}

if ($null -ne $build -and $null -ne $ubr) {
    $current = [version] ('{0}.{1}' -f $build, $ubr)
    if ($current -ge $SacReEnableBaseline) {
        Write-Field 'SAC re-enable baseline' ('{0} - MET' -f $SacReEnableBaseline) 'turning SAC Off is reversible from Settings'
    } else {
        Write-Field 'SAC re-enable baseline' ('{0} - NOT met' -f $SacReEnableBaseline) 'turning SAC Off here is ONE-WAY (reinstall to restore)'
    }
}

# --- 2. SAC policy state --------------------------------------------------
Write-Section 'Smart App Control policy state'
$ciPolicyPath = 'HKLM:\SYSTEM\CurrentControlSet\Control\CI\Policy'
try {
    $ciPolicy = Get-ItemProperty -Path $ciPolicyPath -ErrorAction Stop

    $state = $null
    if ($ciPolicy.PSObject.Properties.Name -contains 'VerifiedAndReputablePolicyState') {
        $state = $ciPolicy.VerifiedAndReputablePolicyState
    }
    Write-Field 'VerifiedAndReputablePolicyState' $state (Get-PolicyStateMeaning $state)

    $previous = $null
    if ($ciPolicy.PSObject.Properties.Name -contains 'SAC_PreviousState') {
        $previous = $ciPolicy.SAC_PreviousState
    }
    Write-Field 'SAC_PreviousState' $previous (Get-PolicyStateMeaning $previous)

    $reason = $null
    if ($ciPolicy.PSObject.Properties.Name -contains 'SAC_EnforcementReason') {
        $reason = $ciPolicy.SAC_EnforcementReason
    }
    Write-Field 'SAC_EnforcementReason' $reason 'not publicly documented; 1 is what auto-promotion from evaluation reports'

    if ("$state" -eq '1') {
        Write-Host ''
        Write-Host '  SAC is ENFORCING. Unsigned binaries you build locally will be blocked at' -ForegroundColor Yellow
        Write-Host '  launch with os error 4551. See docs/SIGNING.md - signing release artifacts' -ForegroundColor Yellow
        Write-Host '  does NOT fix the local build loop.' -ForegroundColor Yellow
    }
} catch {
    Write-Host ("  Could not read {0}: {1}" -f $ciPolicyPath, $_.Exception.Message) -ForegroundColor Yellow
    Write-Host '  (Absent key usually means this machine has no Smart App Control policy at all.)'
}

# --- 3. Active CI policies ------------------------------------------------
Write-Section 'Active Code Integrity policies (CiTool.exe -lp)'
$citool = Get-Command 'CiTool.exe' -ErrorAction SilentlyContinue
if (-not $citool) {
    Write-Host '  CiTool.exe not found on PATH (expected at C:\Windows\System32\CiTool.exe). Skipping.'
} else {
    try {
        # CiTool prints "Press Enter to Continue" on failure, so feed it a closed
        # stdin to guarantee this stays non-interactive.
        $citoolOutput = '' | & $citool.Source -lp 2>&1
        $exit = $LASTEXITCODE
        $lines = @($citoolOutput | ForEach-Object { "$_" } |
            Where-Object { $_ -notmatch 'Press Enter to Continue' })
        if ($exit -ne 0) {
            Write-Host ("  CiTool exited with 0x{0:X8}." -f $exit) -ForegroundColor Yellow
            if (-not (Test-IsElevated)) {
                Write-Host '  CiTool -lp requires an elevated shell. Re-run this script as Administrator' -ForegroundColor Yellow
                Write-Host '  to see the active policy list.' -ForegroundColor Yellow
            }
            foreach ($line in $lines) { if ($line.Trim()) { Write-Host ('  {0}' -f $line) } }
        } else {
            foreach ($line in $lines) { Write-Host ('  {0}' -f $line) }
        }
    } catch {
        Write-Host ("  CiTool invocation failed: {0}" -f $_.Exception.Message) -ForegroundColor Yellow
    }
}

# --- 4. Recent CodeIntegrity events ---------------------------------------
Write-Section ("Recent CodeIntegrity events (last {0}, IDs 3076/3077)" -f $Count)
$repoRoot = Split-Path -Parent $PSScriptRoot
$repoTail = $repoRoot
if ($repoTail -match '^[A-Za-z]:') { $repoTail = $repoTail.Substring(2) }

$events = @()
$eventsReadable = $true
try {
    $events = @(Get-WinEvent -FilterHashtable @{
            LogName = $CodeIntegrityLog
            Id      = $CodeIntegrityEventIds
        } -MaxEvents $Count -ErrorAction Stop)
} catch {
    $message = $_.Exception.Message
    if ($message -match 'No events were found') {
        Write-Host '  No 3076/3077 events in the log. Nothing has been blocked or audited.' -ForegroundColor Green
    } elseif ($message -match 'unauthorized|access is denied|Attempted to perform an unauthorized operation') {
        $eventsReadable = $false
        Write-Host '  ACCESS DENIED reading the CodeIntegrity operational log.' -ForegroundColor Yellow
        Write-Host '  Re-run this script from an elevated PowerShell to see block events.' -ForegroundColor Yellow
    } elseif ($message -match 'There is not an event log|could not be found') {
        $eventsReadable = $false
        Write-Host ("  The '{0}' log is not present on this machine." -f $CodeIntegrityLog) -ForegroundColor Yellow
    } else {
        $eventsReadable = $false
        Write-Host ("  Could not read the CodeIntegrity log: {0}" -f $message) -ForegroundColor Yellow
    }
}

$rows = @()
foreach ($event in $events) {
    $data = Get-EventDataMap -Event $event
    $fileName = ''
    if ($data.ContainsKey('File Name')) { $fileName = $data['File Name'] }
    $processName = ''
    if ($data.ContainsKey('Process Name')) { $processName = $data['Process Name'] }
    $status = ''
    if ($data.ContainsKey('Status')) { $status = $data['Status'] }

    $rows += [pscustomobject]@{
        Time    = $event.TimeCreated
        Kind    = $(if ($event.Id -eq 3077) { 'BLOCK' } else { 'AUDIT' })
        Status  = $status
        File    = Convert-NtPath $fileName
        Process = Convert-NtPath $processName
    }
}

if ($rows.Count -gt 0) {
    Write-Host '  "File" is the binary Code Integrity refused; "Process" is whatever launched it.'
    Write-Host '  A cargo.exe in the Process column means cargo was the launcher, not the victim.'
    Write-Host ''
    # Force a wide layout: at the default console width Format-Table silently
    # drops the Process column, which is exactly the field people misread.
    $tableWidth = 200
    try {
        $hostWidth = $Host.UI.RawUI.BufferSize.Width
        if ($hostWidth -gt $tableWidth) { $tableWidth = $hostWidth - 1 }
    } catch { }
    ($rows | Format-Table -Property Time, Kind, Status, File, Process -Wrap |
        Out-String -Width $tableWidth).TrimEnd() | ForEach-Object { Write-Host $_ }
}

# --- 5. Summary -----------------------------------------------------------
Write-Section 'Summary'
if (-not $eventsReadable) {
    Write-Host '  Event summary unavailable (see above).' -ForegroundColor Yellow
} else {
    $blocks = @($rows | Where-Object { $_.Kind -eq 'BLOCK' })
    $audits = @($rows | Where-Object { $_.Kind -eq 'AUDIT' })
    $repoHits = @($blocks | Where-Object {
            $_.File -like "*$repoTail*" -or $_.Process -like "*$repoTail*"
        })

    Write-Host ("  In the last {0} event(s) examined: {1} block(s) (3077), {2} audit(s) (3076)." -f `
            $rows.Count, $blocks.Count, $audits.Count)
    Write-Host ("  {0} of those block(s) involve this repository ({1})." -f $repoHits.Count, $repoRoot)

    # Cheap total across the whole log, capped so a huge log cannot hang the script.
    try {
        $allBlocks = @(Get-WinEvent -FilterHashtable @{
                LogName = $CodeIntegrityLog
                Id      = 3077
            } -MaxEvents 5000 -ErrorAction Stop)
        $suffix = if ($allBlocks.Count -ge 5000) { ' (capped at 5000)' } else { '' }
        Write-Host ("  Total 3077 blocks currently retained in the log: {0}{1}" -f $allBlocks.Count, $suffix)
    } catch {
        Write-Verbose ("total block count unavailable: {0}" -f $_.Exception.Message)
    }
}

Write-Host ''
Write-Host '  Next steps: docs/SIGNING.md' -ForegroundColor Green
Write-Host ''
