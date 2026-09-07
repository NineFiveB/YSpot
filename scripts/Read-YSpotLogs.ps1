<#
.SYNOPSIS
    Read the shell and service logs as one timeline.

.DESCRIPTION
    SPEC §8.5 gives every YSpot process the same line format and a directory
    that follows its privilege: the shell writes %LOCALAPPDATA%\YSpot\logs,
    the elevated service %ProgramData%\YSpot\logs. That is the right split for
    ACLs and the wrong one for reading, because the question you actually have
    during the dogfood — "the launcher showed nothing, what did the service
    think?" — spans both files.

    This merges them, sorts by timestamp, and prints one readable line each.
    Ordering is at millisecond resolution deliberately: a shell query and the
    service's answer are typically tens of milliseconds apart, so a
    second-resolution merge would shuffle them and invert cause and effect.

    Rotated files (shell.1.log …) are included, so the window is however much
    history §8.5's five-by-ten-megabytes still holds.

.PARAMETER Tail
    Show only the last N lines of the merged timeline. The default, 200, is
    about what fits in a bug report.  0 means everything.

.PARAMETER Level
    Keep only these levels, e.g. -Level error,warn.

.PARAMETER Pattern
    Keep only lines whose message or component matches this regex.

.PARAMETER Raw
    Print the original JSON lines instead of the formatted ones, still merged
    and sorted. Use this when attaching the log to an issue.

.EXAMPLE
    .\scripts\Read-YSpotLogs.ps1
    The last 200 lines from both processes, interleaved.

.EXAMPLE
    .\scripts\Read-YSpotLogs.ps1 -Level error,warn -Tail 0
    Everything that went wrong, for the whole retained history.

.EXAMPLE
    .\scripts\Read-YSpotLogs.ps1 -Pattern 'pipe|reconnect'
    Why the shell and the service stopped talking to each other.
#>
[CmdletBinding()]
param(
    [int]$Tail = 200,
    [string[]]$Level,
    [string]$Pattern,
    [switch]$Raw
)

$ErrorActionPreference = 'Stop'

# Distinct, because the two roots can resolve to one directory — a portable
# install (§9.5), or a machine where ProgramData has been redirected — and
# reading it twice would print every line twice and interleave the copies,
# which reads exactly like the service repeating itself.
$dirs = @(
    (Join-Path $env:LOCALAPPDATA 'YSpot\logs'),
    (Join-Path $env:ProgramData 'YSpot\logs')
) | Select-Object -Unique

$files = foreach ($d in $dirs) {
    if (Test-Path $d) { Get-ChildItem -Path $d -Filter '*.log' -File }
}
$files = $files | Sort-Object FullName -Unique

if (-not $files) {
    Write-Warning "No logs under:`n  $($dirs -join "`n  ")"
    Write-Warning 'Has either process run yet? The service only writes its file once started.'
    return
}

# Parse first, filter second: a malformed line (a half-written record at the
# moment of a crash, which is exactly when you are reading this) must not take
# the whole report down with it.
$records = foreach ($f in $files) {
    $lineNo = 0
    foreach ($line in [System.IO.File]::ReadLines($f.FullName)) {
        $lineNo++
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        try {
            $o = $line | ConvertFrom-Json
        }
        catch {
            Write-Verbose "$($f.Name):$lineNo is not JSON, skipped"
            continue
        }
        [pscustomobject]@{
            Ts        = $o.ts
            Level     = $o.level
            Process   = $o.process
            Component = $o.component
            Message   = $o.message
            Raw       = $line
        }
    }
}

if ($Level) {
    $wanted = $Level | ForEach-Object { $_.ToLowerInvariant() }
    $records = $records | Where-Object { $wanted -contains $_.Level }
}
if ($Pattern) {
    $records = $records | Where-Object { $_.Message -match $Pattern -or $_.Component -match $Pattern }
}

$records = $records | Sort-Object Ts
if ($Tail -gt 0) { $records = $records | Select-Object -Last $Tail }

if ($Raw) {
    $records | ForEach-Object { $_.Raw }
    return
}

$records | ForEach-Object {
    # ConvertFrom-Json turns an ISO-8601 string into a local DateTime, which
    # then prints without milliseconds — losing the only precision that makes
    # the merge worth doing. Back to UTC, and keep the fractional part.
    $t = if ($_.Ts -is [datetime]) { $_.Ts.ToUniversalTime().ToString('HH:mm:ss.fff') } else { [string]$_.Ts }
    '{0}  {1,-5} {2,-6} {3}' -f $t, $_.Level, $_.Process, $_.Message
}
