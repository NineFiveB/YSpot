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
    Rotated files (shell.1.log ...) are included, so the window is however much
    history §8.5's five-by-ten-megabytes still holds.

    Two things it has to get right, because both were wrong first time and both
    fail silently:

    It opens the logs with FileShare.ReadWrite, so it works WHILE YSpot is
    running. The obvious [IO.File]::ReadLines cannot: Windows refuses a
    FileShare.Read open of a file someone holds for writing, which the shell
    and the service both do for their whole lifetime. That is every moment you
    would actually want to read them.

    It sorts by timestamp AND by position, because Sort-Object is not stable
    and same-millisecond lines came out reversed roughly one time in six. A
    query and its answer land in the same millisecond often enough that an
    unstable sort inverts cause and effect exactly where it matters most.

.PARAMETER Tail
    Show only the last N lines of the merged timeline. The default, 200, is
    about what fits in a bug report. 0 means everything.

.PARAMETER Level
    Keep only these levels, e.g. -Level error,warn.

.PARAMETER Pattern
    Keep only lines whose message or component matches. A regular expression
    unless -Simple is given. A bad expression is reported rather than thrown.

.PARAMETER Simple
    Treat -Pattern as literal text. Use this for anything containing \ [ ] + .
    or ( — a Windows path pasted from the log, most of all.

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
    .\scripts\Read-YSpotLogs.ps1 -Simple -Pattern 'C:\Users\me\Documents'
    A path, matched literally. Without -Simple this is an invalid regex.
#>
[CmdletBinding()]
param(
    [ValidateRange(0, [int]::MaxValue)]
    [int]$Tail = 200,

    [ValidateSet('error', 'warn', 'info', 'debug', 'trace')]
    [string[]]$Level,

    [string]$Pattern,
    [switch]$Simple,
    [switch]$Raw
)

$ErrorActionPreference = 'Stop'

# Distinct, because the two roots can resolve to one directory — a machine
# where ProgramData has been redirected, say — and reading it twice would
# print every line twice, interleaved with its own copy, which reads exactly
# like the service repeating itself.
$dirs = @(
    (Join-Path $env:LOCALAPPDATA 'YSpot\logs'),
    (Join-Path $env:ProgramData 'YSpot\logs')
) | Select-Object -Unique

# -LiteralPath throughout: a '[' in a redirected profile path is a wildcard to
# Test-Path, which then reports a directory that plainly exists as missing.
$files = foreach ($d in $dirs) {
    if (Test-Path -LiteralPath $d) {
        Get-ChildItem -LiteralPath $d -File | Where-Object { $_.Name -like '*.log' }
    }
}
$files = @($files | Sort-Object FullName -Unique)

if (-not $files) {
    Write-Warning "No logs under:`n  $($dirs -join "`n  ")"
    Write-Warning 'Has either process run yet? The service only writes its file once started.'
    return
}

if ($Pattern -and -not $Simple) {
    try { $null = [regex]::new($Pattern) }
    catch {
        Write-Warning "-Pattern '$Pattern' is not a valid regular expression: $($_.Exception.InnerException.Message)"
        Write-Warning 'Add -Simple to match it as literal text (needed for any Windows path).'
        return
    }
}

$skipped = [System.Collections.Generic.List[string]]::new()
$unreadable = [System.Collections.Generic.List[string]]::new()
$records = [System.Collections.Generic.List[object]]::new()
$perDir = @{}

for ($fi = 0; $fi -lt $files.Count; $fi++) {
    $f = $files[$fi]
    $lineNo = 0
    try {
        # FileShare.ReadWrite + Delete, matching what the writer holds. Delete
        # matters too: rotation renames the live file, and a reader without it
        # would block the very rotation §8.5 requires.
        $stream = [System.IO.FileStream]::new(
            $f.FullName, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read,
            ([System.IO.FileShare]::ReadWrite -bor [System.IO.FileShare]::Delete))
        $reader = [System.IO.StreamReader]::new($stream)
    }
    catch {
        # One unreadable file must not cost the report. It used to: the read
        # was outside the try, so an ACL or a mid-run rotation discarded every
        # record already parsed and printed a .NET exception instead.
        $unreadable.Add("$($f.Name) ($($_.Exception.Message))")
        continue
    }

    try {
        while ($null -ne ($line = $reader.ReadLine())) {
            $lineNo++
            if ([string]::IsNullOrWhiteSpace($line)) { continue }
            try { $o = $line | ConvertFrom-Json }
            catch {
                # Counted, never silent. A diagnostic tool that quietly drops
                # records is worse than no tool: it shows you a log with a hole
                # in it and no reason to suspect one, and the dropped record is
                # disproportionately the interesting one — the half-written
                # line a crash leaves behind is written AT the crash.
                $skipped.Add("$($f.Name):$lineNo")
                continue
            }
            # Normalised here, once. PowerShell 7 turns an ISO-8601 string into
            # a [datetime]; 5.1 leaves it a [string]. Sorting a mix of the two
            # is meaningless, and so is formatting it.
            $ts = $null
            if ($o.ts -is [datetime]) { $ts = $o.ts.ToUniversalTime() }
            elseif ($o.ts) {
                [datetime]$parsed = [datetime]::MinValue
                if ([datetime]::TryParse([string]$o.ts, [cultureinfo]::InvariantCulture,
                        [System.Globalization.DateTimeStyles]::RoundtripKind -bor
                        [System.Globalization.DateTimeStyles]::AdjustToUniversal, [ref]$parsed)) {
                    $ts = $parsed
                }
            }
            $records.Add([pscustomobject]@{
                    Ts        = $ts
                    RawTs     = [string]$o.ts
                    FileIdx   = $fi
                    LineNo    = $lineNo
                    Level     = $o.level
                    Process   = $o.process
                    Component = $o.component
                    Message   = $o.message
                    Raw       = $line
                })
        }
    }
    finally { $reader.Dispose() }

    $perDir[$f.DirectoryName] = ($perDir[$f.DirectoryName] + 1)
}

$selected = $records
if ($Level) {
    $wanted = $Level | ForEach-Object { $_.ToLowerInvariant() }
    $selected = $selected | Where-Object { $wanted -contains $_.Level }
}
if ($Pattern) {
    $selected = if ($Simple) {
        $selected | Where-Object {
            ([string]$_.Message).Contains($Pattern) -or ([string]$_.Component).Contains($Pattern)
        }
    }
    else {
        $selected | Where-Object { $_.Message -match $Pattern -or $_.Component -match $Pattern }
    }
}

# FileIdx and LineNo are the tiebreakers Sort-Object does not provide. Without
# them, lines sharing a millisecond come out in an arbitrary order that changes
# between runs — measured at about one same-ms pair in six.
$selected = @($selected | Sort-Object Ts, FileIdx, LineNo)
if ($Tail -gt 0 -and $selected.Count -gt $Tail) {
    $selected = $selected[($selected.Count - $Tail)..($selected.Count - 1)]
}

function Format-Record($r) {
    # The date, not just the time. -Tail 0 spans a fortnight, and a bare
    # HH:mm:ss makes a line from last Tuesday indistinguishable from one from
    # this morning, with nothing marking the day boundary.
    $t = if ($r.Ts) { $r.Ts.ToString('MM-dd HH:mm:ss.fff') } else { '?? ' + $r.RawTs }
    $c = ([string]$r.Component) -replace '^yspot[_-]?(shell|indexd)?::?', ''
    if ($c.Length -gt 18) { $c = $c.Substring(0, 18) }
    # One record stays one line: yspot-log deliberately keeps an embedded
    # newline inside the JSON string, and printing it raw would split the
    # record into a second line with no timestamp, level, or process.
    $m = ([string]$r.Message) -replace "`r", '' -replace "`n", '\n'
    '{0}  {1,-5} {2,-6} {3,-18} {4}' -f $t, $r.Level, $r.Process, $c, $m
}

if ($Raw) {
    $selected | ForEach-Object { $_.Raw }
    # On stdout, not the warning stream: the documented workflow is
    # `-Raw > logs.txt`, and '>' does not capture warnings, so an attachment
    # with holes in it would arrive looking complete.
    foreach ($s in $skipped) { "# skipped (not valid JSON): $s" }
    foreach ($u in $unreadable) { "# unreadable: $u" }
}
else {
    $selected | ForEach-Object { Format-Record $_ }
}

if ($skipped.Count -gt 0 -and -not $Raw) {
    $where = if ($skipped.Count -le 5) { ": $($skipped -join ', ')" } else { ", first: $($skipped[0])" }
    Write-Warning "$($skipped.Count) line(s) were not valid JSON and are missing from the above$where"
}
if ($unreadable.Count -gt 0 -and -not $Raw) {
    Write-Warning "could not read: $($unreadable -join '; ')"
}

# A half-read timeline presented as a whole one is its own kind of wrong
# answer. This fires when one side contributed nothing — running elevated as
# another admin points LOCALAPPDATA at that account's profile, and the shell's
# half simply vanishes with no other sign.
foreach ($d in $dirs) {
    if (-not $perDir.ContainsKey($d)) {
        Write-Warning "no log lines came from $d — this timeline is one process only"
    }
}
