//! Host stdin as a pty input source: lets a bare `WITH_IO [tty]`
//! step hear the user's keys when stdin is a real terminal, and stays
//! out of the way everywhere else.
//!
//! Bare tty runs closed on purpose: a worker parked in a blocking
//! host-stdin read cannot observe child death, so the session would
//! strand. This module lifts that only where a wake exists. Unix
//! polls stdin on a short tick against a teardown flag; Windows
//! unblocks the pending console read with `CancelIoEx`. Either way
//! the worker releases promptly and teardown stays prompt.
//!
//! Raw mode (no echo, no cooking, no signals, no flow control)
//! applies for the life of the reader and restores on drop, so the
//! host terminal is a transparent byte conduit and the child pty
//! owns signal generation. Anything that is not a real terminal,
//! and any setup failure, yields `None`: headless runs keep the
//! closed behavior they have today.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::run::PtyInput;

/// Poll tick for the Unix stdin wait: prompt teardown without
/// spinning.
#[cfg(unix)]
const POLL_TICK_MS: i32 = 50;

/// Offer host stdin as the session input. Returns `None` (closed,
/// today's behavior) when stdin is not a usable terminal or setup
/// fails. Never blocks the caller: the reader runs on the pump's
/// input worker.
pub fn acquire_host_stdin() -> Option<PtyInput> {
    #[cfg(unix)]
    {
        acquire_fd(0)
    }
    #[cfg(windows)]
    {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = Arc::clone(&stop);
        let (handle, guard) = open_console_input()?;
        let keep = Arc::new(guard);
        Some(PtyInput {
            read_chunk: Box::new(move |buf| {
                let _hold = Arc::clone(&keep);
                read_chunk_windows(handle, buf, &stop_reader)
            }),
            cancel: Box::new(move || {
                stop.store(true, Ordering::SeqCst);
                unsafe {
                    windows_sys::Win32::System::IO::CancelIoEx(
                        handle as windows_sys::Win32::Foundation::HANDLE,
                        std::ptr::null(),
                    )
                };
            }),
        })
    }
    #[cfg(not(any(unix, windows)))]
    {
        None
    }
}

/// Offer an arbitrary terminal fd as the session input. Test seam
/// for the live path: production passes fd 0, tests pass a pty
/// slave. Same fail-closed contract as `acquire_host_stdin`.
#[cfg(unix)]
fn acquire_fd(fd: libc::c_int) -> Option<PtyInput> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_reader = Arc::clone(&stop);
    let guard = match RawGuard::acquire_fd(fd) {
        Some(guard) => {
            eprintln!("oxdock-tty: host stdin engaged, raw mode set");
            guard
        }
        None => {
            eprintln!("oxdock-tty: host stdin closed (not a terminal or raw mode failed)");
            return None;
        }
    };
    let keep = Arc::new(guard);
    Some(PtyInput {
        read_chunk: Box::new(move |buf| {
            let _hold = Arc::clone(&keep);
            read_chunk_fd(fd, buf, &stop_reader)
        }),
        cancel: Box::new(move || {
            stop.store(true, Ordering::SeqCst);
        }),
    })
}

/// Restores the host terminal mode when the reader goes away.
struct RawGuard {
    #[cfg(unix)]
    fd: libc::c_int,
    #[cfg(unix)]
    original: libc::termios,
    /// Console input handle as an integer: `HANDLE` is a raw pointer
    /// and can never cross threads, so it only materializes inside
    /// the unsafe calls.
    #[cfg(windows)]
    handle: isize,
    #[cfg(windows)]
    original_mode: u32,
}

#[cfg(unix)]
impl RawGuard {
    /// Put `fd` in raw mode: no echo, no cooking, no signals, no
    /// flow control. Fails closed when the fd is not a terminal.
    fn acquire_fd(fd: libc::c_int) -> Option<Self> {
        if unsafe { libc::isatty(fd) } != 1 {
            return None;
        }
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        raw.c_iflag &= !(libc::ICRNL | libc::INLCR | libc::IGNCR | libc::IXON);
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

/// Read one chunk from `fd` without ever blocking past the tick.
/// Cancellation and real EOF both report `Ok(0)`: the pump ends
/// input cleanly either way instead of failing the step.
#[cfg(unix)]
fn read_chunk_fd(fd: libc::c_int, buf: &mut [u8], stop: &AtomicBool) -> io::Result<usize> {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(0);
        }
        let ready = unsafe { libc::poll(&mut pfd, 1, POLL_TICK_MS) };
        if ready < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if ready == 0 {
            continue;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Ok(0);
        }
        if pfd.revents & (libc::POLLIN | libc::POLLHUP) == 0 {
            continue;
        }
        let count = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if count < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(count as usize);
    }
}

/// Open the console input handle and put it in raw mode (no echo,
/// no line cooking, no processed input: bytes cross unmodified and
/// the child console decides what they mean). Fails closed when
/// stdin is not a console.
#[cfg(windows)]
fn open_console_input() -> Option<(isize, RawGuard)> {
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    acquire_handle(handle as isize).map(|guard| (handle as isize, guard))
}

/// Put a console input handle in raw mode (no echo, no line
/// cooking, no processed input: bytes cross unmodified). Fails
/// closed when the handle is not a console.
#[cfg(windows)]
fn acquire_handle(handle: isize) -> Option<RawGuard> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
        ENABLE_VIRTUAL_TERMINAL_INPUT, GetConsoleMode, SetConsoleMode,
    };
    let raw_handle = handle as HANDLE;
    if raw_handle.is_null() || handle == -1 {
        return None;
    }
    let mut mode = 0;
    if unsafe { GetConsoleMode(raw_handle, &mut mode) } == 0 {
        return None;
    }
    let raw = (mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
        | ENABLE_VIRTUAL_TERMINAL_INPUT;
    if unsafe { SetConsoleMode(raw_handle, raw) } == 0 {
        return None;
    }
    Some(RawGuard {
        handle,
        original_mode: mode,
    })
}

#[cfg(windows)]
impl Drop for RawGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::Console::SetConsoleMode;
        unsafe {
            SetConsoleMode(self.handle as HANDLE, self.original_mode);
        }
    }
}

/// Blocking console read, unblocked by the cancel hook. An aborted
/// read (teardown) reports `Ok(0)` like EOF so the pump ends input
/// cleanly; other failures still fail the step.
#[cfg(windows)]
fn read_chunk_windows(handle: isize, buf: &mut [u8], stop: &AtomicBool) -> io::Result<usize> {
    use windows_sys::Win32::Foundation::{GetLastError, HANDLE};
    use windows_sys::Win32::Storage::FileSystem::ReadFile;
    if stop.load(Ordering::SeqCst) {
        return Ok(0);
    }
    let mut count = 0;
    let ok = unsafe {
        ReadFile(
            handle as HANDLE,
            buf.as_mut_ptr() as *mut _,
            buf.len() as u32,
            &mut count,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        if unsafe { GetLastError() } == windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED {
            return Ok(0);
        }
        return Err(io::Error::last_os_error());
    }
    Ok(count as usize)
}

#[cfg(test)]
mod tests {
    /// Non-terminals stay closed: /dev/null is never a terminal, so
    /// acquisition fails there on every machine, terminal or not.
    #[cfg(unix)]
    #[test]
    fn dev_null_stays_closed() {
        let fd = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY) };
        assert!(fd >= 0);
        assert!(super::RawGuard::acquire_fd(fd).is_none());
        unsafe {
            libc::close(fd);
        }
    }

    /// Live path without a human: bytes written to a pty master
    /// arrive through the acquired reader with raw mode applied,
    /// and dropping the reader restores the slave's mode.
    #[cfg(unix)]
    #[test]
    fn pty_slave_feeds_acquired_reader() {
        let mut master = 0;
        let mut slave = 0;
        let opened = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0);
        let mut input = super::acquire_fd(slave).expect("pty slave is a terminal");
        let mut before: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut before) }, 0);
        assert_eq!(before.c_lflag & libc::ECHO, 0, "reader applies raw mode");
        let sent = b"hi";
        let written = unsafe { libc::write(master, sent.as_ptr() as *const _, sent.len()) };
        assert_eq!(written as usize, sent.len());
        let mut buf = [0u8; 16];
        let count = (input.read_chunk)(&mut buf).expect("pump read succeeds");
        assert_eq!(&buf[..count], sent, "bytes cross unmodified");
        drop(input);
        let mut after: libc::termios = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut after) }, 0);
        assert_ne!(after.c_lflag & libc::ECHO, 0, "drop restores slave mode");
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }

    /// Non-consoles stay closed: NUL is never a console, so
    /// acquisition fails there on every machine, terminal or not.
    #[cfg(windows)]
    #[test]
    fn nul_stays_closed() {
        use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_READ, HANDLE};
        use windows_sys::Win32::Storage::FileSystem::{
            CreateFileA, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        let handle = unsafe {
            CreateFileA(
                c"NUL".as_ptr() as *const u8,
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        assert!(!handle.is_null() && handle as isize != -1);
        assert!(super::acquire_handle(handle as isize).is_none());
        unsafe {
            CloseHandle(handle as HANDLE);
        }
    }
}
