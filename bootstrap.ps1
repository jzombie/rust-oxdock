# Install the oxdock binary from GitHub releases. Thin fetcher only:
# every install decision lives in install.oxfile, executed below with
# the fetched binary.
#
#   irm https://raw.githubusercontent.com/jzombie/rust-oxdock/main/bootstrap.ps1 | iex
#
# Pin explicitly with VERSION (a tag: the API's "latest" skips
# pre-releases, and every release here is `-alpha` until stable):
#
#   $env:VERSION = 'v0.24.1-alpha'; irm ... | iex
#
# Pass INSTALL_DIR to choose the destination; the installer defaults
# it when absent.
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

$tmp = Join-Path ([IO.Path]::GetTempPath()) "oxdock-install-$([Guid]::NewGuid())"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
  $base = "https://github.com/$Repo/releases/download/$Version"
  Invoke-WebRequest "$base/oxdock-$ArchTarget.tar.gz" -OutFile (Join-Path $tmp 'asset.tar.gz')
  Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
  $Expected = ((Get-Content (Join-Path $tmp 'SHA256SUMS')) | Where-Object { $_ -match "oxdock-$ArchTarget.tar.gz" } | Select-Object -First 1) -split '\s+' | Select-Object -First 1
  # Installer logic rides with the release; tags predating it fall back
  # to main. The logic is version-agnostic (verify, extract, place).
  $raw = "https://raw.githubusercontent.com/$Repo/$Version/install.oxfile"
  try { Invoke-WebRequest $raw -OutFile (Join-Path $tmp 'install.oxfile') } catch { Invoke-WebRequest "https://raw.githubusercontent.com/$Repo/main/install.oxfile" -OutFile (Join-Path $tmp 'install.oxfile') }
  New-Item -ItemType Directory -Force -Path (Join-Path $tmp 'x') | Out-Null
  tar.exe -xzf (Join-Path $tmp 'asset.tar.gz') -C (Join-Path $tmp 'x')
  $env:OXDOCK_ASSET = (Join-Path $tmp 'asset.tar.gz')
  $env:OXDOCK_BIN = (Join-Path $tmp 'x\oxdock.exe')
  # Pass-through only (possibly unset, which removes it): the installer
  # owns the default. The name mapping lives here because only the stub
  # knows both sides.
  $env:OXDOCK_DIR = $env:INSTALL_DIR
  $env:OXDOCK_SHA = $Expected
  $env:OXDOCK_VERSION = $Version
  Push-Location $tmp
  & (Join-Path $tmp 'x\oxdock.exe') 'install.oxfile'
  if ($LASTEXITCODE -ne 0) { throw "installer failed with exit code $LASTEXITCODE" }
  Pop-Location
} finally {
  Remove-Item -Recurse -Force $tmp
}
