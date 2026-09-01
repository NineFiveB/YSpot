<#
.SYNOPSIS
    Verifies that shipped binaries are signed by the expected publisher.

.DESCRIPTION
    YSpot must ship under its organization identity, never a personal one
    (docs/SIGNING.md). Certificate subjects are chosen by Azure identity
    validation rather than by this repo, so the only reliable guard is to
    inspect what actually came out of signing and fail the release if it is
    wrong. Running this after every signing step is what stops a
    misconfigured profile from silently publishing personal details.

    Fails when a file is unsigned, its signature does not validate, or its
    subject CN does not match -ExpectedCn.

.PARAMETER Path
    File, directory or glob to verify. Directories are searched recursively
    for .exe/.dll/.msi.

.PARAMETER ExpectedCn
    Exact CN the certificate must carry. Defaults to $env:YSPOT_EXPECTED_SIGNER_CN.
    When neither is supplied the script reports subjects and warns, but does
    not fail — so it stays usable before the certificate profile exists.

.PARAMETER RequireSigned
    Fail on unsigned files. Off by default so local unsigned builds can be
    inspected; the release workflow turns it on.

.EXITCODE 0  all checks passed (or nothing to check)
.EXITCODE 1  a file failed verification
.EXITCODE 2  no files matched Path
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)]
    [string] $Path,

    [string] $ExpectedCn = $env:YSPOT_EXPECTED_SIGNER_CN,

    [switch] $RequireSigned
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Get-TargetFiles {
    param([string] $Spec)

    if (Test-Path -LiteralPath $Spec -PathType Container) {
        return Get-ChildItem -LiteralPath $Spec -Recurse -File |
            Where-Object { $_.Extension -in '.exe', '.dll', '.msi' }
    }
    # Glob or single file.
    return @(Get-ChildItem -Path $Spec -File -ErrorAction SilentlyContinue)
}

$files = @(Get-TargetFiles -Spec $Path)
if ($files.Count -eq 0) {
    Write-Host "verify-signature: no files matched '$Path'"
    exit 2
}

if ([string]::IsNullOrWhiteSpace($ExpectedCn)) {
    Write-Warning @'
YSPOT_EXPECTED_SIGNER_CN is not set, so publisher identity is NOT enforced.
Set it to the organization's exact certificate CN (see docs/SIGNING.md) —
without it, a misconfigured certificate profile can ship personal details.
'@
}

$failed = 0
foreach ($file in $files) {
    $sig = Get-AuthenticodeSignature -LiteralPath $file.FullName

    if ($sig.Status -eq 'NotSigned' -or $null -eq $sig.SignerCertificate) {
        if ($RequireSigned) {
            Write-Host "::error::UNSIGNED  $($file.Name)"
            $failed++
        } else {
            Write-Host "  unsigned  $($file.Name)"
        }
        continue
    }

    $subject = $sig.SignerCertificate.Subject
    # Subject is a comma-separated RDN list; CN may be quoted when it contains
    # a comma (e.g. 'CN="Example, Inc."').
    $cn = if ($subject -match 'CN=(?:"([^"]*)"|([^,]*))') {
        if ($Matches[1]) { $Matches[1] } else { $Matches[2].Trim() }
    } else { '' }

    $problems = @()
    if ($sig.Status -ne 'Valid') { $problems += "signature status '$($sig.Status)'" }
    if ($ExpectedCn -and $cn -ne $ExpectedCn) {
        $problems += "CN '$cn' != expected '$ExpectedCn'"
    }

    if ($problems.Count -gt 0) {
        Write-Host "::error::$($file.Name): $($problems -join '; ')"
        Write-Host "           subject: $subject"
        $failed++
    } else {
        Write-Host "  ok  $($file.Name)  [$cn]"
    }
}

Write-Host ""
Write-Host "verify-signature: $($files.Count) file(s) checked, $failed failure(s)"
if ($failed -gt 0) { exit 1 }
exit 0
