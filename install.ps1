# Install the OxDock binary from GitHub releases. Thin fetcher only:
# every install decision lives in install.oxfile, executed below with
# the fetched binary.
#
# Piped stdin is saved to a scratch file and bridged to install.oxfile
# as OXDOCK_PIPED_SCRIPT.
#
# Content-addressed local cache: the first run downloads and verifies,
# later runs reuse the cached tarball after re-verifying its hash, so
# reruns need no network once warm (pin VERSION to stay fully offline).
# Override the cache root with OXDOCK_CACHE_DIR.

#
#   (irm 'https://raw.githubusercontent.com/jzombie/rust-oxdock/main/install.ps1') | iex
#
# Pin explicitly with VERSION (a tag: the API's "latest" skips
# pre-releases, and every release here is `-alpha` until stable):
#
#   $env:VERSION = 'v0.33.0-alpha'; (irm 'https://raw.githubusercontent.com/jzombie/rust-oxdock/main/install.ps1') | iex
#
# Pass INSTALL_DIR to choose the destination; the installer defaults
# it when absent.
#
# Pass OXDOCK_BINARY_NAME to choose the installed stem (default
# `oxdock`; the installer appends `.exe` on Windows). CI passes
# `oxdock-engine` to free the bare name for a binary under test.
#
# Point OXDOCK_ENGINE at a branch-built binary to run the installer
# with it instead of the downloaded release (CI smoke proves behavior
# fixes before any release ships them). The asset still downloads and
# verifies; only the execution engine swaps.
#
# Point OXDOCK_RELEASE_BASE at a mirror root (no trailing slash) to
# fetch the asset and checksums from elsewhere; CI smoke serves a
# staged branch build over loopback through it.

$ErrorActionPreference = 'Stop'

$Repo = "jzombie/rust-oxdock"

$Version = $env:VERSION

$CacheRoot = if ($env:OXDOCK_CACHE_DIR) { $env:OXDOCK_CACHE_DIR } else { Join-Path $HOME '.cache\oxdock' }

# Shared runner IPs burn the anonymous API quota: authenticate reads
# when a token is present (CI bridges GITHUB_TOKEN; users never need
# one). An empty table sends no headers.
$ApiHeaders = @{}
if ($env:GITHUB_TOKEN) { $ApiHeaders['Authorization'] = "Bearer $($env:GITHUB_TOKEN)" }

if (-not $Version -or $Version -eq 'latest') {
  $Version = (Invoke-RestMethod "https://api.github.com/repos/$REPO/releases?per_page=1" -Headers $ApiHeaders)[0].tag_name

}

# Install name and expected version ride through untouched: the
# single strip lives in install.oxfile, never here.

$ArchTarget = switch ($env:PROCESSOR_ARCHITECTURE) {
  'AMD64' { 'x86_64-pc-windows-msvc' }

  'ARM64' { 'aarch64-pc-windows-msvc' }

  default { Write-Error "unsupported platform: $($env:PROCESSOR_ARCHITECTURE)"; return }

}

$Asset = "oxdock-$ArchTarget.tar.gz"

$VDir = Join-Path $CacheRoot $Version

New-Item -ItemType Directory -Force -Path $VDir | Out-Null

function Test-CachedAsset {
  $sums = Join-Path $VDir 'SHA256SUMS'

  $path = Join-Path $VDir $Asset

  $logic = Join-Path $VDir 'install.oxfile'

  $stub = Join-Path $VDir 'install.ps1'

  # The executing stub itself is part of the warm contract: a cache
  # warmed by the other stub, or by a release predating bundled
  # installers, repopulates instead of handing off to a file that is
  # not there.
  if (-not (Test-Path $path) -or -not (Test-Path $sums) -or -not (Test-Path $logic) -or -not (Test-Path $stub)) { return $false }
  $expected = ((Get-Content $sums) | Where-Object { $_ -match "$Asset" } | Select-Object -First 1) -split '\s+' | Select-Object -First 1
  if (-not $expected) { return $false }
  $actual = (Get-FileHash $path -Algorithm SHA256).Hash

  return $expected.ToLowerInvariant() -eq $actual.ToLowerInvariant()

}

# Same hash as last time means same bytes: reuse the cached tarball
# after re-verifying it. Anything missing or mismatched falls through
# to a fresh download, so a corrupt cache heals itself. Direct fd
# writes: Write-Warning follows stdout when stdout is redirected, so
# pipe captures would miss the announcement.
if (Test-CachedAsset) {
  [Console]::Error.WriteLine("using cached oxdock $VERSION")
} else {
  [Console]::Error.WriteLine("downloading oxdock $VERSION")

  $tmp = Join-Path ([IO.Path]::GetTempPath()) "oxdock-install-$([Guid]::NewGuid())"

  New-Item -ItemType Directory -Force -Path $tmp | Out-Null
  try {
    $base = if ($env:OXDOCK_RELEASE_BASE) { $env:OXDOCK_RELEASE_BASE.TrimEnd('/') } else { "https://github.com/$REPO/releases/download/$VERSION" }

    Invoke-WebRequest "$base/$Asset" -Headers $ApiHeaders -OutFile (Join-Path $tmp 'asset.tar.gz')

    Invoke-WebRequest "$base/SHA256SUMS" -Headers $ApiHeaders -OutFile (Join-Path $tmp 'SHA256SUMS')

    $line = (Get-Content (Join-Path $tmp 'SHA256SUMS')) | Where-Object { $_ -match "$Asset" } | Select-Object -First 1
    if (-not $line) { throw "asset $Asset missing from SHA256SUMS" }
    $expected = ($line -split '\s+')[0]

    $actual = (Get-FileHash (Join-Path $tmp 'asset.tar.gz') -Algorithm SHA256).Hash
    if ($expected.ToLowerInvariant() -ne $actual.ToLowerInvariant()) { throw "checksum mismatch for $Asset" }

    # Installer logic rides with the release bundle. An explicit
    # OXDOCK_OXFILE_URL wins over the bundle (pre-merge testing,
    # mirrors). Fetched here, inside population, so warm runs never
    # touch network.
    if ($env:OXDOCK_OXFILE_URL) {
      Invoke-WebRequest $env:OXDOCK_OXFILE_URL -Headers $ApiHeaders -OutFile (Join-Path $tmp 'install.oxfile')
    } else {
      Invoke-WebRequest "$base/install.oxfile" -Headers $ApiHeaders -OutFile (Join-Path $tmp 'install.oxfile')

    }

    # The bundled stubs travel beside everything else so execution
    # below hands off to logic matching the engine version, and so a
    # cache warmed by one stub serves the other: every supported release
    # lists both. A missing entry fails closed at verification.
    Invoke-WebRequest "$base/install.ps1" -Headers $ApiHeaders -OutFile (Join-Path $tmp 'install.ps1')

    Invoke-WebRequest "$base/install.sh" -Headers $ApiHeaders -OutFile (Join-Path $tmp 'install.sh')

    Move-Item (Join-Path $tmp 'asset.tar.gz') (Join-Path $VDir $Asset) -Force

    Move-Item (Join-Path $tmp 'SHA256SUMS') (Join-Path $VDir 'SHA256SUMS') -Force

    Move-Item (Join-Path $tmp 'install.oxfile') (Join-Path $VDir 'install.oxfile') -Force

    Move-Item (Join-Path $tmp 'install.ps1') (Join-Path $VDir 'install.ps1') -Force

    Move-Item (Join-Path $tmp 'install.sh') (Join-Path $VDir 'install.sh') -Force
  } finally {
    Remove-Item -Recurse -Force $tmp

  }
}

# Hand off to the release-bundled installer: the executed logic always
# matches the engine version, and warm runs keep working because the
# bundled copy lives in the cache beside everything else. The flag
# prevents re-entry: the re-executed copy skips this block. Digest
# verification fails closed when the entry is missing or mismatched.
if (-not $env:OXDOCK_STUB_ACTIVE) {
  $stubWant = (((Get-Content (Join-Path $VDir 'SHA256SUMS')) | Where-Object { $_ -match " install.ps1" } | Select-Object -First 1) -split '\s+')[0]

  $stubGot = (Get-FileHash (Join-Path $VDir 'install.ps1') -Algorithm SHA256).Hash
  if ($stubWant.ToLowerInvariant() -ne $stubGot.ToLowerInvariant()) { throw "installer stub failed verification" }

  # Stdin capture, no decisions: collect any piped input and bridge that
  # file for the oxfile below as OXDOCK_PIPED_SCRIPT, which alone decides
  # run vs install. Captured here, after verification, so a failed handoff
  # leaves nothing behind; the re-executed copy below inherits the bridge
  # instead of draining again. Pipeline input belongs to this invocation:
  # it never crosses an `&` file call, so only this process can read it
  # (`irm ... | iex` lands here with none and installs as before).
  # Pipeline objects arrive via $input, which console-stdin redirection
  # never sees: a piped string lives in $input while [Console]::In sits
  # at EOF, so $input is read first and redirected bytes only as
  # fallback. A TTY is never drained.
  $env:OXDOCK_PIPED_SCRIPT = $null

  $stdinText = [string]::Join("`n", @($input))
  if ([string]::IsNullOrWhiteSpace($stdinText) -and [Console]::IsInputRedirected) {
    $stdinText = [Console]::In.ReadToEnd()

  }

  if (-not [string]::IsNullOrWhiteSpace($stdinText)) {
    $pipedDir = Join-Path ([IO.Path]::GetTempPath()) "oxdock-piped-$([Guid]::NewGuid())"

    New-Item -ItemType Directory -Force -Path $pipedDir | Out-Null

    $stdinFile = Join-Path $pipedDir 'piped.oxfile'

    [IO.File]::WriteAllText($stdinFile, $stdinText)

    $env:OXDOCK_PIPED_SCRIPT = $stdinFile

  }

  # Interactive consoles confirm the destination in the oxfile: export
  # the flag only with a console behind stdin. Re-probed, never
  # inherited: under ConPTY the session is interactive with no
  # redirection, under pipes the stdin drain above already decided
  # piped vs install, so the answer matches on re-entry.
  if ([Environment]::UserInteractive -and -not [Console]::IsInputRedirected) {
    $env:OXDOCK_INTERACTIVE = 'true'
  }

  $env:OXDOCK_STUB_ACTIVE = '1'

  # In-memory handoff: the outer stub arrived via iex, and invoking the
  # cached file as a path would trip Restricted execution policy on
  # vanilla Windows (plus any GPO-enforced policy a process-scope
  # bypass cannot relax). Compiling the verified content to a
  # scriptblock keeps every stage in memory, so no session mutation is
  # needed. Stdin was already bridged to a file above and travels via
  # env, so no pipeline state crosses this call. Native binaries below
  # are unaffected by script policy either way.
  & ([scriptblock]::Create((Get-Content -Raw -LiteralPath (Join-Path $VDir 'install.ps1'))))
  if ($LASTEXITCODE -ne 0) { throw "installer failed with exit code $LASTEXITCODE" }
  return

}

# Provenance for smoke forensics: which oxfile payload executes below
# and its digest. A version-skewed payload names its source here
# instead of surfacing as a bare parse error downstream.
$oxfileSrc = if ($env:OXDOCK_OXFILE_URL) { $env:OXDOCK_OXFILE_URL } else { "release $Version" }

$oxfileDigest = (Get-FileHash (Join-Path $VDir 'install.oxfile') -Algorithm SHA256).Hash

[Console]::Error.WriteLine("installer oxfile: $oxfileSrc $oxfileDigest")

$Expected = ((Get-Content (Join-Path $VDir 'SHA256SUMS')) | Where-Object { $_ -match "$Asset" } | Select-Object -First 1) -split '\s+' | Select-Object -First 1

# The installer runs from the cache, never the network.
$tmp = Join-Path ([IO.Path]::GetTempPath()) "oxdock-install-$([Guid]::NewGuid())"

New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
  Copy-Item (Join-Path $VDir 'install.oxfile') (Join-Path $tmp 'install.oxfile') -Force

  New-Item -ItemType Directory -Force -Path (Join-Path $tmp 'x') | Out-Null

  tar.exe -xzf (Join-Path $VDir $Asset) -C (Join-Path $tmp 'x')

  $env:OXDOCK_ASSET = (Join-Path $VDir $Asset)

  $env:OXDOCK_BIN = (Join-Path $tmp 'x\oxdock.exe')

  # Destination default lives here in native shell; empty keeps the
  # installer's loud gate. The engine normalizes separators itself.
  if ($env:INSTALL_DIR) { $env:OXDOCK_DIR = $env:INSTALL_DIR } elseif ($env:LOCALAPPDATA) { $env:OXDOCK_DIR = Join-Path $env:LOCALAPPDATA 'Microsoft\WindowsApps' }

  $env:OXDOCK_SHA = $Expected

  $env:OXDOCK_VERSION = $Version

  $env:OXDOCK_EXPECTED_VERSION = $Version

  $env:OXDOCK_SKIP_DOWNLOAD = 'false'

  # An explicit OXDOCK_ENGINE runs the installer instead of the
  # unpacked release; it must name an existing file, and fails closed
  # otherwise.
  # Resolve relative to the caller's directory: execution below runs
  # from the scratch dir, where a relative path would no longer point
  # at the engine.
  $Engine = if ($env:OXDOCK_ENGINE) { $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($env:OXDOCK_ENGINE) } else { (Join-Path $tmp 'x\oxdock.exe') }

  if (-not (Test-Path $Engine)) { throw "installer engine missing: $Engine" }

  # Interactive consoles confirm the destination in the oxfile: export
  # the flag only with a console behind stdin. Either condition alone
  # misfires in CI-without-console, so both must hold.
  $env:OXDOCK_INTERPRETER = $Engine

  Push-Location $tmp
  try {

    # Absolute script path: the engine resolves relative script names
    # against the workspace root, which would run the checkout's copy
    # instead of the fetched payload above.
    & $Engine (Join-Path $tmp 'install.oxfile')

    $code = $LASTEXITCODE

    if ($code -ne 0) { throw "installer failed with exit code $code" }

    # CI tail under the fresh binary: the oxfile above verified and
    # placed under the old engine; re-running the same file with SKIP=1
    # proves the fresh binary runs DSL before its directory lands on
    # PATH. Gated here on the PATH file alone (the oxfile owns the
    # version gate), and never in piped-run mode: nothing was placed
    # there, so no fresh binary exists. Off-runner installs skip
    # silently either way.
    if (-not $env:OXDOCK_PIPED_SCRIPT -and $env:GITHUB_PATH -and (Test-Path $env:GITHUB_PATH -PathType Leaf)) {
      $RawName = if ($env:OXDOCK_BINARY_NAME) { $env:OXDOCK_BINARY_NAME } else { 'oxdock' }

      if ($RawName -like '*.exe') {
        $Fresh = Join-Path $env:OXDOCK_DIR $RawName
      } else {
        $Fresh = Join-Path $env:OXDOCK_DIR "$RawName.exe"
      }

      $env:OXDOCK_EXPECTED_VERSION = $Version
      $env:OXDOCK_SKIP_DOWNLOAD = 'true'

      try {
        & $Fresh (Join-Path $tmp 'install.oxfile')

        $freshCode = $LASTEXITCODE

        if ($freshCode -ne 0) { throw "installer failed with exit code $freshCode (fresh tail)" }
      } finally {
        $env:OXDOCK_SKIP_DOWNLOAD = $null

      }
    }
  } finally {

    # Restore first: deleting the working directory while standing
    # inside it fails as in use and would mask the real error.
    Pop-Location

    # The handoff bridged a piped script through its own directory
    # outside the scratch space above: unlink it here, on success and
    # failure alike. The empty-parent check keeps this from touching
    # anything it did not create.
    if ($env:OXDOCK_PIPED_SCRIPT -and (Test-Path $env:OXDOCK_PIPED_SCRIPT -PathType Leaf)) {
      Remove-Item -Force $env:OXDOCK_PIPED_SCRIPT

      $pipedParent = Split-Path $env:OXDOCK_PIPED_SCRIPT -Parent
      if ($pipedParent -and (Test-Path $pipedParent) -and @(Get-ChildItem $pipedParent -Force).Count -eq 0) {
        Remove-Item -Force $pipedParent

      }
    }
  }
} finally {
  Remove-Item -Recurse -Force $tmp

}
