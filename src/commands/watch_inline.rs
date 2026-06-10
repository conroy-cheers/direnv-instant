use nix::sys::select::{FdSet, select};
use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
use nix::sys::time::{TimeVal, TimeValLike};
use nix::unistd::read;
use std::fs::{File, OpenOptions};
use std::io::{IoSliceMut, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};
use vt100_split::{SplitConfig, SplitPane, set_winsize};

fn request_pty_master(socket_path: &Path) -> Option<OwnedFd> {
    let mut socket = UnixStream::connect(socket_path).ok()?;
    socket.write_all(b"WATCH\n").ok()?;

    let mut fds = FdSet::new();
    fds.insert(socket.as_fd());
    let mut timeout = TimeVal::seconds(5);
    match select(None, Some(&mut fds), None, None, Some(&mut timeout)) {
        Ok(_) if fds.contains(socket.as_fd()) => {}
        _ => return None,
    }

    let mut iov = [0u8; 16];
    let mut iov_slice = [IoSliceMut::new(&mut iov)];
    let mut cmsg_space = nix::cmsg_space!([RawFd; 1]);
    let msg = recvmsg::<()>(
        socket.as_raw_fd(),
        &mut iov_slice,
        Some(&mut cmsg_space),
        MsgFlags::empty(),
    )
    .ok()?;

    if let Ok(cmsgs) = msg.cmsgs() {
        for cmsg in cmsgs {
            if let ControlMessageOwned::ScmRights(fds) = cmsg {
                if let Some(&fd) = fds.first() {
                    return Some(unsafe { OwnedFd::from_raw_fd(fd) });
                }
            }
        }
    }

    None
}

pub fn run(log_path: &Path, socket_path: &Path, tty_path: &Path) {
    let tty = match OpenOptions::new().read(true).write(true).open(tty_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "direnv-instant: failed to open TTY {}: {}",
                tty_path.display(),
                e
            );
            return;
        }
    };

    let log_file = {
        let start = Instant::now();
        let timeout = Duration::from_secs(5);
        loop {
            if let Ok(f) = File::open(log_path) {
                break f;
            }
            if start.elapsed() > timeout {
                eprintln!("direnv-instant: timeout waiting for log file");
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    let config = std::env::var("DIRENV_INSTANT_CURSOR_ROW")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .map_or_else(SplitConfig::tmux_like_current_row, SplitConfig::tmux_like);
    let mut state = SplitPane::new(tty, config);
    let _ = state.setup();
    let pty_master = request_pty_master(socket_path);
    if let Some(ref pty) = pty_master {
        let (rows, cols) = state.pane_size();
        set_winsize(pty.as_raw_fd(), rows, cols);
    }
    // The inline viewer shares the shell's foreground TTY. Reading from it races
    // with the shell line editor and steals prompt input.
    let socket = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(_) => return,
    };

    let mut buf = [0u8; 8192];
    loop {
        let _ = state.handle_resize();

        let mut fds = FdSet::new();
        fds.insert(socket.as_fd());
        let mut timeout = TimeVal::milliseconds(50);

        match select(None, Some(&mut fds), None, None, Some(&mut timeout)) {
            Ok(_) => {
                if fds.contains(socket.as_fd()) {
                    match read(&socket, &mut buf) {
                        Ok(0) | Err(_) => {
                            loop {
                                match read(&log_file, &mut buf) {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => {
                                        state.ingest(&buf[..n]);
                                    }
                                }
                            }
                            let _ = state.redraw();

                            break;
                        }
                        Ok(_) => {}
                    }
                }
            }
            _ => {}
        }

        match read(&log_file, &mut buf) {
            Ok(0) => {}
            Ok(n) => {
                state.ingest(&buf[..n]);
            }
            Err(_) => {}
        }

        let _ = state.redraw();
    }

    let _ = state.teardown();
}
