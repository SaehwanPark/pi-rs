#requires -Version 5.1
[CmdletBinding()]
param(
  [string]$Version = 'latest',
  [string]$InstallDir,
  [switch]$AddToPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repository = if ($env:RUPI_REPOSITORY) { $env:RUPI_REPOSITORY } else { 'SaehwanPark/rupi' }
if (-not $PSBoundParameters.ContainsKey('Version') -and $env:RUPI_VERSION) {
  $Version = $env:RUPI_VERSION
}
if (-not $PSBoundParameters.ContainsKey('InstallDir') -and $env:RUPI_INSTALL_DIR) {
  $InstallDir = $env:RUPI_INSTALL_DIR
}
if ([string]::IsNullOrWhiteSpace($InstallDir)) {
  $baseDirectory = if ($env:LOCALAPPDATA) { $env:LOCALAPPDATA } else { $env:USERPROFILE }
  $InstallDir = Join-Path $baseDirectory 'rupi\bin'
}
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)

function Fail([string]$Message) {
  throw "rupi installer: $Message"
}

function Download-File([string]$Uri, [string]$Destination) {
  try {
    Invoke-WebRequest -UseBasicParsing -Uri $Uri -OutFile $Destination
  } catch {
    Fail "could not download $Uri ($($_.Exception.Message))"
  }
}

if ($Version -eq 'latest') {
  try {
    $release = Invoke-RestMethod `
      -Headers @{ Accept = 'application/vnd.github+json' } `
      -Uri "https://api.github.com/repos/$repository/releases/latest"
    $Version = [string]$release.tag_name
  } catch {
    Fail "could not resolve the latest GitHub release ($($_.Exception.Message))"
  }
}

if ($Version -match '^[0-9]') {
  $Version = "v$Version"
}
if ($Version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$') {
  Fail 'release version must look like v0.2.2'
}

$architecture = if ($env:PROCESSOR_ARCHITEW6432) {
  $env:PROCESSOR_ARCHITEW6432
} else {
  $env:PROCESSOR_ARCHITECTURE
}
if ($architecture.ToUpperInvariant() -ne 'AMD64') {
  Fail "no prebuilt release for Windows $architecture; install from source with cargo or choose a supported x86_64 host"
}

$target = 'x86_64-pc-windows-msvc'
$archiveName = "rupi-$Version-$target.zip"
$baseUri = "https://github.com/$repository/releases/download/$Version"
$tempRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("rupi-install-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tempRoot -Force | Out-Null

try {
  $archivePath = Join-Path $tempRoot $archiveName
  $checksumPath = "$archivePath.sha256"
  Write-Host "Downloading rupi $Version for $target..."
  Download-File "$baseUri/$archiveName" $archivePath
  Download-File "$baseUri/$archiveName.sha256" $checksumPath

  $expected = ((Get-Content -LiteralPath $checksumPath -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
  if ($expected -notmatch '^[0-9a-f]{64}$') {
    Fail "the checksum file for $archiveName is invalid"
  }
  $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archivePath).Hash.ToLowerInvariant()
  if ($expected -ne $actual) {
    Fail "checksum mismatch for $archiveName"
  }

  $extracted = Join-Path $tempRoot 'extracted'
  Expand-Archive -LiteralPath $archivePath -DestinationPath $extracted -Force
  $binary = Join-Path $extracted 'rupi.exe'
  if (-not (Test-Path -LiteralPath $binary -PathType Leaf)) {
    Fail 'release archive does not contain rupi.exe'
  }

  New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
  $destination = Join-Path $InstallDir 'rupi.exe'
  $staged = Join-Path $InstallDir ".rupi.exe.tmp-$PID"
  try {
    Copy-Item -LiteralPath $binary -Destination $staged -Force
    Move-Item -LiteralPath $staged -Destination $destination -Force
  } catch {
    Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
    Fail "could not write $destination; choose a writable directory with -InstallDir ($($_.Exception.Message))"
  }

  if ($AddToPath) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $parts = if ($userPath) { @($userPath -split ';' | Where-Object { $_ }) } else { @() }
    $alreadyPresent = $false
    foreach ($part in $parts) {
      if ($part.Trim() -ieq $InstallDir) {
        $alreadyPresent = $true
        break
      }
    }
    if (-not $alreadyPresent) {
      [Environment]::SetEnvironmentVariable('Path', (($parts + $InstallDir) -join ';'), 'User')
    }
    if (-not (($env:Path -split ';') -contains $InstallDir)) {
      $env:Path = "$InstallDir;$env:Path"
    }
    Write-Host "Added $InstallDir to the user PATH. Open a new terminal if this one does not see it."
  } else {
    Write-Host "Open a new PowerShell window, or add it for this session with: `$env:Path += ';$InstallDir'"
  }

  Write-Host "Installed rupi $Version to $destination"
} finally {
  Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
}
