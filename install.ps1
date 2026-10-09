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

if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64') {
  Write-Error "unsupported platform: $($env:PROCESSOR_ARCHITECTURE) (only x86_64 Windows ships binaries)"
}

$InstallDir = if ($env:INSTALL_DIR) { $env:INSTALL_DIR } else { Join-Path $HOME '.local\bin' }
New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
$tmp = Join-Path ([IO.Path]::GetTempPath()) "oxdock-install-$([Guid]::NewGuid())"
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
  Invoke-WebRequest "https://github.com/$Repo/releases/download/$Version/oxdock-x86_64-pc-windows-msvc.tar.gz" -OutFile (Join-Path $tmp 'asset.tar.gz')
  tar.exe -xzf (Join-Path $tmp 'asset.tar.gz') -C $tmp
  Copy-Item (Join-Path $tmp 'oxdock.exe') (Join-Path $InstallDir 'oxdock.exe') -Force
  Write-Output "installed oxdock $Version to $InstallDir"
  if (($env:Path -split ';') -notcontains $InstallDir) {
    Write-Warning "$InstallDir is not on PATH"
  }
} finally {
  Remove-Item -Recurse -Force $tmp
}
