//! Sacrificial console probe for headless CI: detaches any inherited
//! console, allocates its own, injects one key event, and proves
//! `acquire_host_stdin` delivers the byte. Detaching is process-local to
//! this sacrificial child, so the test harness never touches its own
//! process-global console state.
//!
//! Exit codes: 0 proves the live path, `EXIT_ENV_SKIP` (77, the Automake
//! skip convention) means the environment cannot host a console, anything
//! else fails with diagnostics on stdout.

#[cfg(windows)]
const EXIT_ENV_SKIP: i32 = 77;

#[cfg(windows)]
fn main() {
    std::process::exit(probe());
}

#[cfg(windows)]
fn probe() -> i32 {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AllocConsole, FreeConsole, GetStdHandle, INPUT_RECORD, INPUT_RECORD_0, KEY_EVENT,
        KEY_EVENT_RECORD, KEY_EVENT_RECORD_0, STD_INPUT_HANDLE, SetStdHandle, WriteConsoleInputW,
    };
    use windows_sys::w;

    // Detach any inherited console first: a windowless console is often
    // attached (CI runners hold one without a window, which reads null
    // from the parent yet blocks allocation). Detaching is process-local
    // to this sacrificial child; the harness keeps its own attachment.
    unsafe {
        FreeConsole();
    }
    if unsafe { AllocConsole() } == 0 {
        println!("SKIP: console allocation denied in this environment");
        return EXIT_ENV_SKIP;
    }
    struct ConsoleGuard;
    impl Drop for ConsoleGuard {
        fn drop(&mut self) {
            unsafe {
                FreeConsole();
            }
        }
    }
    let _guard = ConsoleGuard;

    // Open the fresh console input explicitly and install it as this
    // process's stdin: with redirected stdio the inherited handles stay
    // pipes, so neither injection nor acquisition would see the console.
    let conin = unsafe {
        CreateFileW(
            w!("CONIN$"),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if conin.is_null() || conin == INVALID_HANDLE_VALUE {
        println!("FAIL: fresh console input is not openable");
        return 1;
    }
    unsafe {
        SetStdHandle(STD_INPUT_HANDLE, conin);
    }
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle.is_null() {
        println!("FAIL: detached console has no stdin handle");
        return 1;
    }
    let record = INPUT_RECORD {
        EventType: KEY_EVENT as u16,
        Event: INPUT_RECORD_0 {
            KeyEvent: KEY_EVENT_RECORD {
                bKeyDown: 1,
                wRepeatCount: 1,
                wVirtualKeyCode: 0x41,
                wVirtualScanCode: 0,
                uChar: KEY_EVENT_RECORD_0 {
                    UnicodeChar: b'A' as u16,
                },
                dwControlKeyState: 0,
            },
        },
    };
    let mut written: u32 = 0;
    if unsafe { WriteConsoleInputW(handle, &record, 1, &mut written) } == 0 || written != 1 {
        println!("FAIL: key event did not land in the console buffer");
        return 1;
    }
    let mut input = match oxdock_pty::acquire_host_stdin() {
        Some(input) => input,
        None => {
            println!("FAIL: fresh console is not usable");
            return 1;
        }
    };
    let mut buf = [0u8; 16];
    match (input.read_chunk)(&mut buf) {
        Ok(count) if buf.get(..count) == Some(b"A".as_slice()) => {}
        other => {
            println!("FAIL: unexpected pump read: {other:?}");
            return 1;
        }
    }
    drop(input);
    let _ = handle as HANDLE;
    unsafe {
        CloseHandle(conin);
    }
    println!("PROBE-OK: console keystroke crosses the acquired reader");
    0
}

#[cfg(not(windows))]
fn main() {
    println!("SKIP: console probe is Windows-only");
}
