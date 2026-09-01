#Requires -Version 5.1
<#
.SYNOPSIS
    Signs Windows PE files with Azure Artifact Signing, or cleanly does nothing
    when signing is not configured.

.DESCRIPTION
    This is the script behind `bundle.windows.signCommand` in
    apps/shell/src-tauri/tauri.conf.json. Tauri invokes it once per file it wants
    signed (app exe, bundled DLLs, the WebView2 loader, the NSIS uninstaller and
    the installers themselves), substituting the file path for `%1`.

    It is deliberately a no-op unless signing is switched on:
      * YSPOT_SIGNING_ENABLED must be "1", and
      * every required configuration variable must be present.
    Otherwise it prints why it is skipping and exits 0, so `tauri build` still
    works for contributors with no Azure account. When signing IS enabled and
    something goes wrong, it fails loudly (non-zero exit) so a release build
    cannot silently ship unsigned binaries.

    Signing is performed by `artifact-signing-cli` (crates.io), which drives
    signtool.exe with the Azure Artifact Signing dlib. Note that the older
    `trusted-signing-cli` crate is deprecated - do not switch back to it.

.PARAMETER Path
    One or more files, directories or glob patterns to sign. Directories and
    globs are expanded to signable file types (exe, dll, msi, msix, appx, cab,
    sys, ocx, cat). An explicitly named file is signed as-is.

.PARAMETER Recurse
    Recurse into directories given via -Path.

.EXAMPLE
    # How tauri calls it (see tauri.conf.json):
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/sign-windows.ps1 -Path <file>

.EXAMPLE
    # Standalone, after a release build:
    $env:YSPOT_SIGNING_ENABLED = '1'
    ./scripts/sign-windows.ps1 -Path target/release -Recurse

.NOTES
    Environment variables
      Required to sign (all of them, or the script skips):
        YSPOT_SIGNING_ENABLED   "1" to arm the script
        YSPOT_SIGN_ENDPOINT     e.g. https://eus.codesigning.azure.net
                                MUST match the region of BOTH the signing
                                account and the certificate profile
        YSPOT_SIGN_ACCOUNT      Artifact Signing account name
        YSPOT_SIGN_PROFILE      certificate profile name
        AZURE_TENANT_ID         service principal tenant
        AZURE_CLIENT_ID         service principal app id
        AZURE_CLIENT_SECRET     service principal secret
      Optional:
        YSPOT_SIGN_DESCRIPTION  signature description (default "YSpot")
        YSPOT_SIGN_DESCRIPTION_URL  ignored by the CLI today; kept for parity
        YSPOT_SIGN_TIMESTAMP_URL    RFC3161 URL
                                    (default http://timestamp.acs.microsoft.com)
        YSPOT_SIGN_CLI          full path to artifact-signing-cli.exe
        SIGNTOOL_PATH           full path to signtool.exe (auto-discovered)
        AZURE_CLI_PATH          full path to az.cmd (auto-discovered)

    Timestamping is not optional. Artifact Signing leaf certificates are renewed
    daily and live only ~72 hours, so an un-timestamped signature stops
    validating three days after it is produced.

    See docs/SIGNING.md.
#>
[CmdletBinding()]
param(
    [Parameter(Position = 0, ValueFromRemainingArguments = $true)]
    [string[]] $Path,

    [switch] $Recurse
)

Set-StrictMode -Version 2.0
$ErrorActionPreference = 'Stop'

$SignableExtensions = @('.exe', '.dll', '.msi', '.msix', '.appx', '.cab', '.sys', '.ocx', '.cat')
$DefaultTimestampUrl = 'http://timestamp.acs.microsoft.com'
# artifact-signing-cli accepts 1..=99 positional file arguments.
$MaxFilesPerInvocation = 90

function Write-Note {
    param([Parameter(Mandatory)][string] $Message)
    Write-Host "[sign-windows] $Message"
}

function Write-Skip {
    param([Parameter(Mandatory)][string] $Reason)
    Write-Host "[sign-windows] signing skipped (unconfigured): $Reason" -ForegroundColor Yellow
}

# Write-Error would trip $ErrorActionPreference = 'Stop' and rob us of a
# deterministic exit code, so failures are reported with Write-Host + exit.
function Write-Fail {
    param([Parameter(Mandatory)][string] $Message)
    Write-Host "[sign-windows] $Message" -ForegroundColor Red
}

function Get-EnvValue {
    param([Parameter(Mandatory)][string] $Name)
    $value = [Environment]::GetEnvironmentVariable($Name)
    if ($null -eq $value) { return '' }
    return $value.Trim()
}

function Resolve-InputPaths {
    param([string[]] $Inputs, [bool] $RecurseDirs)

    $resolved = New-Object System.Collections.Generic.List[string]
    foreach ($item in $Inputs) {
        if ([string]::IsNullOrWhiteSpace($item)) { continue }
        $candidate = $item.Trim('"')

        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            $resolved.Add((Resolve-Path -LiteralPath $candidate).ProviderPath)
            continue
        }

        if (Test-Path -LiteralPath $candidate -PathType Container) {
            $children = Get-ChildItem -LiteralPath $candidate -File -Recurse:$RecurseDirs
            foreach ($child in $children) {
                if ($SignableExtensions -contains $child.Extension.ToLowerInvariant()) {
                    $resolved.Add($child.FullName)
                }
            }
            continue
        }

        # Treat anything else as a wildcard pattern.
        $globMatches = @(Get-ChildItem -Path $candidate -File -Recurse:$RecurseDirs -ErrorAction SilentlyContinue)
        if ($globMatches.Count -eq 0) {
            throw "no file, directory or glob match for '$candidate'"
        }
        foreach ($globMatch in $globMatches) {
            if ($SignableExtensions -contains $globMatch.Extension.ToLowerInvariant()) {
                $resolved.Add($globMatch.FullName)
            }
        }
    }

    return @($resolved | Select-Object -Unique)
}

function Find-SignTool {
    $configured = Get-EnvValue 'SIGNTOOL_PATH'
    if ($configured -and (Test-Path -LiteralPath $configured -PathType Leaf)) { return $configured }

    $roots = @(
        (Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'),
        (Join-Path $env:ProgramFiles 'Windows Kits\10\bin')
    ) | Where-Object { $_ -and (Test-Path -LiteralPath $_) }

    $best = $null
    $bestVersion = [version]'0.0.0.0'
    foreach ($root in $roots) {
        foreach ($arch in @('x64', 'arm64', 'x86')) {
            $candidates = Get-ChildItem -Path (Join-Path $root "*\$arch\signtool.exe") -ErrorAction SilentlyContinue
            foreach ($candidate in $candidates) {
                $versionText = $candidate.Directory.Parent.Name
                $parsed = [version]'0.0.0.0'
                if (-not [version]::TryParse($versionText, [ref] $parsed)) { $parsed = [version]'0.0.0.0' }
                if ($parsed -ge $bestVersion) {
                    $bestVersion = $parsed
                    $best = $candidate.FullName
                }
            }
            if ($best) { break }
        }
    }
    return $best
}

function Find-AzureCli {
    $configured = Get-EnvValue 'AZURE_CLI_PATH'
    if ($configured -and (Test-Path -LiteralPath $configured -PathType Leaf)) { return $configured }

    $default = Join-Path $env:ProgramFiles 'Microsoft SDKs\Azure\CLI2\wbin\az.cmd'
    if (Test-Path -LiteralPath $default -PathType Leaf) { return $default }

    $onPath = Get-Command 'az.cmd' -ErrorAction SilentlyContinue
    if (-not $onPath) { $onPath = Get-Command 'az' -ErrorAction SilentlyContinue }
    if ($onPath) { return $onPath.Source }

    return $null
}

function Find-SigningCli {
    $configured = Get-EnvValue 'YSPOT_SIGN_CLI'
    if ($configured -and (Test-Path -LiteralPath $configured -PathType Leaf)) { return $configured }

    $onPath = Get-Command 'artifact-signing-cli' -ErrorAction SilentlyContinue
    if ($onPath) { return $onPath.Source }

    $cargoHome = Get-EnvValue 'CARGO_HOME'
    if (-not $cargoHome) { $cargoHome = Join-Path $env:USERPROFILE '.cargo' }
    $cargoBin = Join-Path $cargoHome 'bin\artifact-signing-cli.exe'
    if (Test-Path -LiteralPath $cargoBin -PathType Leaf) { return $cargoBin }

    return $null
}

# --------------------------------------------------------------------------
# 1. Is signing armed at all?
# --------------------------------------------------------------------------
if ((Get-EnvValue 'YSPOT_SIGNING_ENABLED') -ne '1') {
    Write-Skip 'YSPOT_SIGNING_ENABLED is not "1". Binaries will be left unsigned.'
    exit 0
}

$requiredVars = @(
    'YSPOT_SIGN_ENDPOINT',
    'YSPOT_SIGN_ACCOUNT',
    'YSPOT_SIGN_PROFILE',
    'AZURE_TENANT_ID',
    'AZURE_CLIENT_ID',
    'AZURE_CLIENT_SECRET'
)
$missing = @($requiredVars | Where-Object { -not (Get-EnvValue $_) })
if ($missing.Count -gt 0) {
    Write-Skip ("missing " + ($missing -join ', ') + '. See docs/SIGNING.md.')
    exit 0
}

# --------------------------------------------------------------------------
# 2. From here on, every failure is fatal.
# --------------------------------------------------------------------------
if (-not $Path -or @($Path).Count -eq 0) {
    Write-Fail 'no -Path given. Pass a file, directory or glob.'
    exit 2
}

try {
    # @() so a single match stays an array (Set-StrictMode would trip on .Count).
    $files = @(Resolve-InputPaths -Inputs $Path -RecurseDirs:$Recurse.IsPresent)
} catch {
    Write-Fail $_.Exception.Message
    exit 2
}

if ($files.Count -eq 0) {
    Write-Fail ("nothing signable matched: {0}" -f ($Path -join ', '))
    exit 2
}

$cli = Find-SigningCli
if (-not $cli) {
    Write-Fail 'artifact-signing-cli was not found.'
    Write-Fail 'Install it with:  cargo install artifact-signing-cli'
    Write-Fail '(or set YSPOT_SIGN_CLI to its full path). Do NOT use the deprecated trusted-signing-cli crate.'
    exit 3
}

$signtool = Find-SignTool
if ($signtool) {
    # artifact-signing-cli defaults SIGNTOOL_PATH to one hard-coded Windows Kit
    # version; pin it to whatever is actually installed here.
    $env:SIGNTOOL_PATH = $signtool
    Write-Note "signtool: $signtool"
} else {
    Write-Note 'signtool.exe not found under the Windows Kits; artifact-signing-cli will use its own default.'
}

$azureCli = Find-AzureCli
if ($azureCli) {
    $env:AZURE_CLI_PATH = $azureCli
} else {
    Write-Note 'az CLI not found; artifact-signing-cli will use its own default path and may fail.'
}

$endpoint = Get-EnvValue 'YSPOT_SIGN_ENDPOINT'
$account = Get-EnvValue 'YSPOT_SIGN_ACCOUNT'
$profileName = Get-EnvValue 'YSPOT_SIGN_PROFILE'
$description = Get-EnvValue 'YSPOT_SIGN_DESCRIPTION'
if (-not $description) { $description = 'YSpot' }
$timestampUrl = Get-EnvValue 'YSPOT_SIGN_TIMESTAMP_URL'
if (-not $timestampUrl) { $timestampUrl = $DefaultTimestampUrl }

Write-Note ("signing {0} file(s) via {1} (account '{2}', profile '{3}')" -f `
        $files.Count, $endpoint, $account, $profileName)

$batchStart = 0
while ($batchStart -lt $files.Count) {
    $batch = @($files | Select-Object -Skip $batchStart -First $MaxFilesPerInvocation)
    $batchStart += $MaxFilesPerInvocation

    foreach ($file in $batch) { Write-Note ("  -> {0}" -f $file) }

    $arguments = @(
        '--endpoint', $endpoint,
        '--account', $account,
        '--certificate', $profileName,
        '--fd', 'SHA256',
        '--tr', $timestampUrl,
        '--td', 'SHA256',
        '--description', $description
    ) + $batch

    # Credentials travel via AZURE_TENANT_ID / AZURE_CLIENT_ID /
    # AZURE_CLIENT_SECRET, which artifact-signing-cli reads itself. Never put
    # them on the command line - they would show up in process listings and logs.
    & $cli @arguments
    $exit = $LASTEXITCODE

    if ($exit -ne 0) {
        Write-Host ''
        Write-Host "[sign-windows] artifact-signing-cli failed with exit code $exit." -ForegroundColor Red
        Write-Host '[sign-windows] Common causes:' -ForegroundColor Red
        Write-Host '  * endpoint region does not match the account/profile region (403 / SignerSign() failure)'
        Write-Host '  * the service principal lacks the "Artifact Signing Certificate Profile Signer" role'
        Write-Host '  * identity validation has expired, which halts all signing'
        Write-Host '  * the certificate profile was deleted/recreated and the name no longer resolves'
        Write-Host '  See docs/SIGNING.md.'
        exit $exit
    }
}

Write-Note ("done - {0} file(s) signed and timestamped ({1})" -f $files.Count, $timestampUrl)
exit 0
