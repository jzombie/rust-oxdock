# Install the oxdock binary from GitHub releases.
#
#   irm https://raw.githubusercontent.com/jzombie/rust-oxdock/main/install.ps1 | iex
#
# Pin explicitly with VERSION (a tag: the API's "latest" skips
# pre-releases, and every release here is `-alpha` until stable):
#
#   $env:VERSION = 'v0.24.1-alpha'; irm ... | iex
#
# Choose the destination with INSTALL_DIR (default $HOME\.local\bin).
$ErrorActionPreference = 'Stop'

$Repo = "jzombie/rust-oxdock"
$Version = $env:VERSION

if (-not $Version) {
  $Version = (Invoke-RestMethod "https://api.github.com/repos/$Repo/releases?per_page=1")[0].tag_name
}

$ArchTarget = switch ($env:PROCESSOR_ARCHITECTURE) {
  'AMD64' { 'x86_64-pc-windows-msvc' }
  'ARM64' { 'aarch64-pc-windows-msvc' }
  default { Write-Error "unsupported platform: $($env:PROCESSOR_ARCHITECTURE)"; return }
}

$InstallDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$tmp = Join-Path ([IO.Path]::GetTempPath()) "oxdock-install-$([Guid]::NewGuid())"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
  $base = "https://github.com/$Repo/releases/download/$Version"
  Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
  Invoke-WebRequest "$base/oxdock-$ArchTarget.tar.gz" -OutFile (Join-Path $tmp 'asset.tar.gz')
  # Fail closed: no checksums file (releases before checksums shipped),
  # no install.
  $expected = ((Get-Content (Join-Path $tmp 'SHA256SUMS')) | Where-Object { $_ -match "oxdock-$ArchTarget.tar.gz" } | Select-Object -First 1) -split '\s+' | Select-Object -First 1
  $actual = (Get-FileHash (Join-Path $tmp 'asset.tar.gz') -Algorithm SHA256).Hash
  if ($expected.ToLowerInvariant() -ne $actual.ToLowerInvariant()) { throw "checksum mismatch for oxdock-$ArchTarget.tar.gz" }
  tar.exe -xzf (Join-Path $tmp 'asset.tar.gz') -C $tmp
  Copy-Item (Join-Path $tmp 'oxdock.exe') (Join-Path $InstallDir 'oxdock.exe') -Force
  Write-Output "installed oxdock $Version to $InstallDir"
  if (($env:Path -split ';') -notcontains $InstallDir) {
    Write-Warning "$InstallDir is not on PATH"
  }
} finally {
  Remove-Item -Recurse -Force $tmp
}
