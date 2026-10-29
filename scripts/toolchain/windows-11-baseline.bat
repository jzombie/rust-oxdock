@echo off
:: Golden Windows setup: base install to full OxDock toolchain.
::
:: PROVEN ON: Windows 11 ARM64 (VM), 2026-10-28: full run to SUCCESS
:: plus `cargo build -p oxdock` green. Intel x64 path is symmetric by
:: construction (all assets confirmed live) but has NOT executed yet.
:: 32-bit x86 Windows is not covered: 64-bit assumed throughout.
::
:: Installs VS C++ Build Tools (x64 plus ARM64 libs), Rust, standalone
:: LLVM Clang, MinGit on persistent PATH, and clones the repo. Every
:: stage verifies itself and fails loudly: SUCCESS prints only when all
:: probes pass. Safe to run headless (no pause). Re-run safe.
setlocal enableextensions enabledelayedexpansion

:: ---- knobs ----
set REPO_URL=https://github.com/jzombie/rust-oxdock.git
set REPO_BRANCH=245-ci-collapse-setup-oxdock-into-the-installer-remove-powershell-from-the-action
set MINGIT_TAG=v2.56.0.windows.4
set MINGIT_VER=2.56.0.4
:: Pinned LLVM release (standalone tarball: the VS Clang component does
:: not land a `clang` on PATH). The `+` in the asset name stays literal:
:: raw `+` is legal in a path and dodges cmd `%` expansion entirely.
set LLVM_VER=23.1.4
:: ----------------

:: Auto-detect architecture.
set RUST_ARCH=x86_64
set MINGIT_ARCH=64-bit
set LLVM_ARCH=x86_64
if /i "%PROCESSOR_ARCHITECTURE%"=="ARM64" set RUST_ARCH=aarch64
if /i "%PROCESSOR_ARCHITEW6432%"=="ARM64" set RUST_ARCH=aarch64
if /i "%RUST_ARCH%"=="aarch64" set MINGIT_ARCH=arm64
if /i "%RUST_ARCH%"=="aarch64" set LLVM_ARCH=aarch64
echo Architecture: %RUST_ARCH%

echo.
echo [1/7] Visual Studio C++ Build Tools...
curl.exe -L -# "https://aka.ms/vs/17/release/vs_BuildTools.exe" -o "%TEMP%\vs_BuildTools.exe" || exit /b 1
"%TEMP%\vs_BuildTools.exe" --quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --add Microsoft.VisualStudio.Component.VC.Tools.x86.x64 --add Microsoft.VisualStudio.Component.VC.Tools.ARM64 --includeRecommended || exit /b 1
del /f /q "%TEMP%\vs_BuildTools.exe" 2>nul

:: Locate the install via vswhere (either Program Files home).
set VSWHERE=%ProgramFiles%\Microsoft Visual Studio\Installer\vswhere.exe
if not exist "%VSWHERE%" set VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe
if not exist "%VSWHERE%" echo NO vswhere found, Build Tools install failed & exit /b 1
for /f "usebackq delims=" %%i in (`"%VSWHERE%" -products * -property installationPath`) do set "VSBASE=%%i"
if not defined VSBASE echo NO Build Tools instance found & exit /b 1
echo Build Tools at: %VSBASE%
if not exist "%VSBASE%\VC\Tools\MSVC" echo NO MSVC toolchain under %VSBASE% & exit /b 1
echo [1/7] OK

echo.
echo [2/7] Rust toolchain (%RUST_ARCH%)...
curl.exe -L -# "https://win.rustup.rs/%RUST_ARCH%" -o "%TEMP%\rustup-init.exe" || exit /b 1
"%TEMP%\rustup-init.exe" -y --default-toolchain stable || exit /b 1
del /f /q "%TEMP%\rustup-init.exe" 2>nul
if not exist "%USERPROFILE%\.cargo\bin\cargo.exe" echo NO cargo after rustup & exit /b 1
echo [2/7] OK

echo.
echo [3/7] MinGit (%MINGIT_ARCH%) to C:\mingit...
curl.exe -L -# "https://github.com/git-for-windows/git/releases/download/%MINGIT_TAG%/MinGit-%MINGIT_VER%-%MINGIT_ARCH%.zip" -o "%TEMP%\mingit.zip" || exit /b 1
if exist C:\mingit rmdir /s /q C:\mingit
mkdir C:\mingit
tar -xf "%TEMP%\mingit.zip" -C C:\mingit || exit /b 1
del /f /q "%TEMP%\mingit.zip" 2>nul
if not exist "C:\mingit\cmd\git.exe" echo NO git after extract & exit /b 1
echo [3/7] OK

echo.
echo [4/7] Standalone LLVM (%LLVM_ARCH%) to C:\llvm...
curl.exe -L -# "https://github.com/llvm/llvm-project/releases/download/llvmorg-%LLVM_VER%/clang+llvm-%LLVM_VER%-%LLVM_ARCH%-pc-windows-msvc.tar.xz" -o "%TEMP%\llvm.tar.xz" || exit /b 1
if exist C:\llvm rmdir /s /q C:\llvm
mkdir C:\llvm
tar -xf "%TEMP%\llvm.tar.xz" -C C:\llvm --strip-components=1 || exit /b 1
del /f /q "%TEMP%\llvm.tar.xz" 2>nul
if not exist "C:\llvm\bin\clang.exe" echo NO clang after extract & exit /b 1
echo [4/7] OK

echo.
echo [5/7] Persistent PATH for MinGit and LLVM...
powershell -NoProfile -ExecutionPolicy Bypass -Command "$p=[Environment]::GetEnvironmentVariable('Path','User'); if ($p -notlike '*mingit*') { [Environment]::SetEnvironmentVariable('Path', ($p + ';C:\mingit\cmd'), 'User') }" || exit /b 1
powershell -NoProfile -ExecutionPolicy Bypass -Command "$p=[Environment]::GetEnvironmentVariable('Path','User'); if ($p -notlike '*llvm*') { [Environment]::SetEnvironmentVariable('Path', ($p + ';C:\llvm\bin'), 'User') }" || exit /b 1
echo [5/7] OK (new terminals pick it up; this session sets it below)

echo.
echo [6/7] Repo clone...
set "PATH=C:\mingit\cmd;%PATH%"
if exist "%USERPROFILE%\Projects\rust-oxdock" echo Repo dir exists, pulling & git -C "%USERPROFILE%\Projects\rust-oxdock" pull --ff-only || exit /b 1
if not exist "%USERPROFILE%\Projects\rust-oxdock" git clone --branch %REPO_BRANCH% --depth 1 "%REPO_URL%" "%USERPROFILE%\Projects\rust-oxdock" || exit /b 1
echo [6/7] OK

echo.
echo [7/7] Probe the full toolchain...
set "PATH=C:\llvm\bin;%USERPROFILE%\.cargo\bin;%PATH%"
cargo --version || exit /b 1
git --version || exit /b 1
:: No VsDevCmd needed: rustc locates MSVC link on its own and clang
:: rides PATH. Verified with a full `cargo build -p oxdock`, plain
:: session, 2026-10-28.
where clang || (echo NO clang on PATH & exit /b 1)
clang --version || exit /b 1
echo [7/7] OK

echo.
echo SUCCESS: base Windows to full OxDock toolchain.
echo Open a NEW terminal for the persistent PATH, then build with:
echo   cd %USERPROFILE%\Projects\rust-oxdock ^&^& cargo build -p oxdock
