mod run;
mod wizard;
#[cfg(unix)]
use std::fs;
use std::io;
use std::path::PathBuf;

pub(crate) use run::run;

#[derive(Debug, Clone)]
struct InitOptions {
    yes: bool,
    dry_run: bool,
    force: bool,
    no_hooks: bool,
    config_path: PathBuf,
}

impl InitOptions {
    fn from_args(args: &crate::cli::InitArgs, config_path: &str) -> Self {
        Self {
            yes: args.yes,
            dry_run: args.dry_run,
            force: args.force,
            no_hooks: args.no_hooks,
            config_path: PathBuf::from(config_path),
        }
    }
}

#[cfg(unix)]
struct TerminalResetGuard {
    fd: libc::c_int,
    handlers: Vec<(libc::c_int, libc::sighandler_t)>,
}

#[cfg(unix)]
impl TerminalResetGuard {
    fn install_if_interactive(interactive: bool) -> anyhow::Result<Option<Self>> {
        if !interactive {
            return Ok(None);
        }
        match Self::install() {
            Ok(guard) => Ok(Some(guard)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    fn install() -> io::Result<Self> {
        use std::os::fd::{AsRawFd, IntoRawFd};

        let tty = fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
        let fd = tty.as_raw_fd();
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        c_result(|| unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) })?;
        let termios = unsafe { termios.assume_init() };
        unsafe {
            std::ptr::addr_of_mut!(ORIGINAL_TERMIOS).write(std::mem::MaybeUninit::new(termios));
        }
        TERMINAL_FD.store(fd, std::sync::atomic::Ordering::SeqCst);
        ORIGINAL_TERMIOS_SET.store(true, std::sync::atomic::Ordering::SeqCst);

        let handlers = install_signal_handlers()?;
        Ok(Self { fd: tty.into_raw_fd(), handlers })
    }
}

#[cfg(unix)]
impl Drop for TerminalResetGuard {
    fn drop(&mut self) {
        restore_terminal();
        for (signal, previous) in &self.handlers {
            unsafe {
                libc::signal(*signal, *previous);
            }
        }
        TERMINAL_FD.store(-1, std::sync::atomic::Ordering::SeqCst);
        ORIGINAL_TERMIOS_SET.store(false, std::sync::atomic::Ordering::SeqCst);
        unsafe {
            libc::close(self.fd);
        }
    }
}

#[cfg(unix)]
static TERMINAL_FD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
#[cfg(unix)]
static ORIGINAL_TERMIOS_SET: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
#[cfg(unix)]
static mut ORIGINAL_TERMIOS: std::mem::MaybeUninit<libc::termios> = std::mem::MaybeUninit::uninit();

#[cfg(unix)]
fn install_signal_handlers() -> io::Result<Vec<(libc::c_int, libc::sighandler_t)>> {
    [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGQUIT]
        .into_iter()
        .map(|signal| {
            let previous = unsafe {
                libc::signal(signal, handle_terminal_signal as *const () as libc::sighandler_t)
            };
            if previous == libc::SIG_ERR {
                Err(io::Error::last_os_error())
            } else {
                Ok((signal, previous))
            }
        })
        .collect()
}

#[cfg(unix)]
extern "C" fn handle_terminal_signal(signal: libc::c_int) {
    restore_terminal();
    let reset = b"\x1b[0m\x1b[?25h\r\n";
    let fd = TERMINAL_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd >= 0 {
        unsafe {
            libc::write(fd, reset.as_ptr().cast(), reset.len());
        }
    }
    unsafe {
        libc::_exit(128 + signal);
    }
}

#[cfg(unix)]
fn restore_terminal() {
    if !ORIGINAL_TERMIOS_SET.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let fd = TERMINAL_FD.load(std::sync::atomic::Ordering::SeqCst);
    if fd < 0 {
        return;
    }
    unsafe {
        libc::tcsetattr(
            fd,
            libc::TCSANOW,
            std::ptr::addr_of!(ORIGINAL_TERMIOS).cast::<libc::termios>(),
        );
    }
}

#[cfg(unix)]
fn c_result<F: FnOnce() -> libc::c_int>(f: F) -> io::Result<()> {
    let status = f();
    if status == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(unix))]
struct TerminalResetGuard;

#[cfg(not(unix))]
impl TerminalResetGuard {
    fn install_if_interactive(_interactive: bool) -> anyhow::Result<Option<Self>> {
        Ok(None)
    }
}
