#requires -Version 5.1
<#
.SYNOPSIS
  MIRA bootstrap installer for Windows.

.DESCRIPTION
  Downloads a release zip from the configured Releases API, verifies SHA-256,
  unpacks `mira.exe` onto PATH, runs the guided `mira setup` wizard (admin
  account, LLM provider, security), registers the Windows service, and opens the
  web UI. Use -NoSetup / -Unattended / -NoSupervisor to skip steps.

.PARAMETER Version
  Specific release to install (e.g. "0.146.0"). Defaults to latest.

.PARAMETER InstallDir
  Where to place mira.exe. Defaults to "$HOME\.mira\bin".

.PARAMETER ReleasesUrl
  Override the Releases API endpoint.

.EXAMPLE
  irm https://get.vexillon.ai/install.ps1 | iex
#>
[CmdletBinding()]
param(
    [string]$Version,
    [string]$InstallDir = (Join-Path $env:USERPROFILE ".mira\bin"),
    [string]$ReleasesUrl = "https://api.github.com/repos/Vexillon-ai/MIRA/releases",
    [string]$DownloadBaseUrl = "https://github.com/Vexillon-ai/MIRA/releases/download",
    [switch]$NoBrowser,
    [switch]$NoSetup,        # skip the guided `mira setup` wizard
    [switch]$Unattended,     # `mira setup --unattended` (reads MIRA_SETUP_* env)
    [switch]$NoSupervisor    # don't register the Windows service
)

$ErrorActionPreference = 'Stop'

function Detect-Arch {
    switch ($env:PROCESSOR_ARCHITECTURE) {
        'AMD64'  { return 'x86_64' }
        'ARM64'  { return 'aarch64' }
        default  {
            throw "Unsupported arch: $env:PROCESSOR_ARCHITECTURE. Tarballs ship for x86_64 + aarch64."
        }
    }
}

$arch   = Detect-Arch
# Tarball naming follows the rust target triple convention
# (e.g. `mira-0.146.0-x86_64-pc-windows-msvc.zip`).
$target = "$arch-pc-windows-msvc"
Write-Host "→ detected target: $target"

# ── Resolve version ─────────────────────────────────────────────────────────

if (-not $Version) {
    Write-Host "→ resolving latest release from $ReleasesUrl"
    try {
        $releases = Invoke-RestMethod -Uri $ReleasesUrl -UseBasicParsing
    } catch {
        throw "Couldn't fetch releases from ${ReleasesUrl}: $_"
    }
    if (-not $releases -or $releases.Count -eq 0) {
        throw "Releases API returned no entries. Pin a version with -Version 0.X.Y."
    }
    $Version = $releases[0].tag_name -replace '^v', ''
    Write-Host "✓ latest is $Version"
}

# ── Download + verify + extract ─────────────────────────────────────────────

$asset       = "mira-$Version-$target.zip"
# GitHub Release assets are addressed by the git tag (`v<version>`),
# same layout as install.sh.
$releaseBase = "$DownloadBaseUrl/v$Version"
$assetUrl    = "$releaseBase/$asset"
$sumsUrl     = "$releaseBase/SHA256SUMS"

$tmp = New-Item -Type Directory -Path (Join-Path $env:TEMP "mira-install-$([guid]::NewGuid())")
try {
    $assetPath = Join-Path $tmp $asset
    Write-Host "→ downloading $assetUrl"
    Invoke-WebRequest -Uri $assetUrl -OutFile $assetPath -UseBasicParsing

    # Best-effort checksum verification.
    try {
        $sumsPath = Join-Path $tmp 'SHA256SUMS'
        Invoke-WebRequest -Uri $sumsUrl -OutFile $sumsPath -UseBasicParsing -ErrorAction Stop
        $line = (Get-Content $sumsPath | Where-Object { $_ -match "\s$([regex]::Escape($asset))$" } | Select-Object -First 1)
        if ($line) {
            $expected = ($line -split '\s+')[0]
            $actual   = (Get-FileHash -Algorithm SHA256 $assetPath).Hash.ToLower()
            if ($expected -ne $actual) {
                throw "Checksum mismatch: expected $expected, got $actual"
            }
            Write-Host "✓ checksum ok"
        } else {
            Write-Host "  (no entry for $asset in SHA256SUMS — skipping verify)"
        }
    } catch {
        Write-Host "  (no SHA256SUMS published — skipping verify)"
    }

    Write-Host "→ extracting"
    Expand-Archive -Path $assetPath -DestinationPath $tmp -Force

    $srcBin = Get-ChildItem -Path $tmp -Recurse -File -Filter 'mira.exe' | Select-Object -First 1
    if (-not $srcBin) {
        throw "Tarball did not contain a mira.exe binary."
    }

    if (-not (Test-Path $InstallDir)) {
        New-Item -Type Directory -Path $InstallDir | Out-Null
    }
    Copy-Item -Path $srcBin.FullName -Destination (Join-Path $InstallDir 'mira.exe') -Force
    Write-Host "✓ installed $InstallDir\mira.exe"
} finally {
    Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
}

# ── PATH hint ───────────────────────────────────────────────────────────────

$userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
if (-not ($userPath -split ';' | Where-Object { $_ -ieq $InstallDir })) {
    Write-Host ""
    Write-Host "Add $InstallDir to your PATH so you can run \`mira\` directly:"
    Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"\`$env:Path;$InstallDir`", 'User')"
    Write-Host "(restart your terminal afterwards.)"
}

# ── Guided first-run setup ───────────────────────────────────────────────────
# Writes a validated config + creates the admin BEFORE the service starts.
# (On Windows, `irm | iex` keeps the console as stdin, so dialoguer prompts work
#  directly — no /dev/tty dance needed.)

$miraExe = Join-Path $InstallDir 'mira.exe'
if (-not $NoSetup) {
    if ($Unattended) {
        Write-Host "→ running: mira setup --unattended"
        & $miraExe setup --unattended
    } else {
        Write-Host "→ launching guided setup…"
        & $miraExe setup
    }
}

# ── Register the Windows service (SCM) ───────────────────────────────────────

if (-not $NoSupervisor) {
    Write-Host "→ running: mira install"
    & $miraExe install
}

# ── PATH hint already printed above; open the web UI ─────────────────────────

$port = 8080
$cfg  = Join-Path $env:USERPROFILE ".mira\config\mira_config.json"
if (Test-Path $cfg) { try { $port = (Get-Content $cfg -Raw | ConvertFrom-Json).server.port } catch {} }
$url = "http://localhost:$port/"
if (-not $NoBrowser) {
    Start-Process $url | Out-Null
}

Write-Host ""
Write-Host "────────────────────────────────────────────────────"
Write-Host "MIRA $Version installed."
Write-Host "  binary: $InstallDir\mira.exe"
Write-Host "  open:   $url"
Write-Host ""
Write-Host "Log in with the admin account from setup, then finish voice +"
Write-Host "channels from the web UI."
Write-Host ""
Write-Host "  status:   mira status"
Write-Host "  stop:     mira stop"
Write-Host "  reconfig: mira setup"
Write-Host "────────────────────────────────────────────────────"
