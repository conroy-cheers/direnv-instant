// Each integration test binary links this module but uses only a subset
// of it; dead-code warnings here are noise, not bugs.
#![allow(dead_code)]

//! Shared test harness for direnv-instant integration tests.
//!
//! Each test runs the real `direnv-instant` binary against a real `direnv`,
//! a real (or stubbed) `tmux`, and a real `.envrc`. Nothing is mocked at
//! the process boundary.

use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::io::{BufRead, BufReader, IoSlice, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nix::pty::openpty;
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};

/// Path to the binary under test.
///
/// `CARGO_BIN_EXE_*` is set by cargo for `[[bin]]` targets when running
/// integration tests. The Nix derivation sets `DIRENV_INSTANT_BIN` instead
/// so the tests can run against the installed binary without cargo.
pub fn bin() -> PathBuf {
    if let Ok(p) = env::var("DIRENV_INSTANT_BIN") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_BIN_EXE_direnv-instant"))
}

/// A throwaway working directory with its own `.envrc`, isolated `HOME`,
/// and a clean environment for child processes.
pub struct Sandbox {
    pub dir: PathBuf,
    pub home: PathBuf,
    _tmp: tempdir::TempDir,
}

impl Sandbox {
    pub fn new(envrc: &str) -> io::Result<Self> {
        Self::with_envrc(|_| envrc.to_owned())
    }

    /// Like [`Self::new`], but the `.envrc` body can reference paths inside
    /// the sandbox dir (e.g. marker files used to gate a slow `.envrc`).
    pub fn with_envrc(f: impl FnOnce(&Path) -> String) -> io::Result<Self> {
        let tmp = tempdir::TempDir::new("direnv-instant-test")?;
        let dir = tmp.path().join("work");
        let home = tmp.path().join("home");
        fs::create_dir_all(&dir)?;
        fs::create_dir_all(&home)?;

        let envrc_path = dir.join(".envrc");
        fs::write(&envrc_path, f(&dir))?;
        fs::set_permissions(&envrc_path, fs::Permissions::from_mode(0o755))?;

        let sb = Self {
            dir,
            home,
            _tmp: tmp,
        };
        sb.allow_direnv()?;
        Ok(sb)
    }

    /// `direnv allow` for the sandbox's `.envrc`. Must be re-run after the
    /// `.envrc` is changed.
    pub fn allow_direnv(&self) -> io::Result<()> {
        let out = Command::new("direnv")
            .arg("allow")
            .current_dir(&self.dir)
            .env("HOME", &self.home)
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .output()?;
        assert!(
            out.status.success(),
            "direnv allow failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }

    /// Base environment for child processes: clean except for what direnv
    /// itself needs. Multiplexer detection variables are stripped so the
    /// caller picks the mode explicitly.
    pub fn base_env(&self) -> HashMap<OsString, OsString> {
        let mut e = HashMap::new();
        e.insert("HOME".into(), self.home.clone().into_os_string());
        e.insert(
            "XDG_DATA_HOME".into(),
            self.home.join(".local/share").into_os_string(),
        );
        e.insert("PATH".into(), env::var_os("PATH").unwrap_or_default());
        // direnv needs a TERM in some sandboxes
        e.insert("TERM".into(), "dumb".into());
        e
    }

    /// Environment for stub-tmux async tests: `TMUX` set, sandbox dir on
    /// PATH so `write_stub_tmux` shadows the real one, short mux delay,
    /// and a real shell PID for the daemon to SIGUSR1.
    pub fn async_env(&self, shell_pid: u32, mux_delay: u32) -> HashMap<OsString, OsString> {
        let mut e = self.base_env();
        e.insert("TMUX".into(), "test".into());
        e.insert(
            "DIRENV_INSTANT_MUX_DELAY".into(),
            mux_delay.to_string().into(),
        );
        e.insert(
            "DIRENV_INSTANT_SHELL_PID".into(),
            shell_pid.to_string().into(),
        );
        e.insert("PATH".into(), prepend_path(&[&self.dir]));
        e
    }

    /// Environment for tests using a real [`TmuxServer`].
    pub fn tmux_env(&self, server: &TmuxServer, shell_pid: u32) -> HashMap<OsString, OsString> {
        let mut e = self.base_env();
        e.insert("TMUX".into(), server.tmux_var());
        e.insert("DIRENV_INSTANT_MUX_DELAY".into(), "1".into());
        e.insert(
            "DIRENV_INSTANT_SHELL_PID".into(),
            shell_pid.to_string().into(),
        );
        e
    }

    /// Run `direnv-instant <args>` in the sandbox dir with the given env.
    pub fn run(&self, args: &[&str], env: &HashMap<OsString, OsString>) -> io::Result<Output> {
        let mut cmd = Command::new(bin());
        cmd.args(args).current_dir(&self.dir).env_clear().envs(env);
        cmd.output()
    }

    /// Spawn `direnv-instant <args>` without waiting.
    pub fn spawn(&self, args: &[&str], env: &HashMap<OsString, OsString>) -> io::Result<Child> {
        let mut cmd = Command::new(bin());
        cmd.args(args)
            .current_dir(&self.dir)
            .env_clear()
            .envs(env)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd.spawn()
    }

    /// Drop a stub `tmux` script into the sandbox dir. Callers must add
    /// `self.dir` to PATH so it shadows the real one.
    pub fn write_stub_tmux(&self, body: &str) -> io::Result<PathBuf> {
        let bash = which("bash").expect("bash on PATH");
        let path = self.dir.join("tmux");
        fs::write(&path, format!("#!{}\n{}\n", bash.display(), body))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }
}

/// Long-lived process whose PID the daemon can SIGUSR1 without affecting
/// the test process. Tests poll for the env file rather than catch the
/// signal because cross-process signal delivery is unreliable in macOS
/// nix sandboxes.
pub struct SignalSink {
    child: Child,
}

impl SignalSink {
    pub fn new() -> io::Result<Self> {
        let child = Command::new("sleep")
            .arg("3600")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self { child })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for SignalSink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Isolated tmux server with its own socket. Killed on drop.
pub struct TmuxServer {
    pub socket: PathBuf,
}

static TMUX_SERVER_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl TmuxServer {
    /// Start with a default-sized session.
    pub fn new(dir: &Path) -> io::Result<Self> {
        Self::with_size(dir, None)
    }

    /// Start with an explicit `(cols, rows)` session size.
    pub fn with_size(dir: &Path, size: Option<(u32, u32)>) -> io::Result<Self> {
        let socket = dir.join("tmux-socket");
        let mut cmd = Command::new("tmux");
        cmd.args(["-S", socket.to_str().unwrap(), "new-session", "-d"]);
        if let Some((x, y)) = size {
            cmd.args(["-x", &x.to_string(), "-y", &y.to_string()]);
        }
        let out = cmd.output()?;
        assert!(
            out.status.success(),
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(Self { socket })
    }

    /// Start with the stripped-down tmux config used by terminal-backend
    /// parity tests.
    pub fn with_parity_config(dir: &Path, size: (u32, u32)) -> io::Result<Self> {
        let n = TMUX_SERVER_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let socket = dir.join(format!("tmux-parity-{}-{n}", std::process::id()));
        let conf =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/minimal-parity.tmux.conf");
        let command = "sleep 3600";
        let out = Command::new("tmux")
            .args(["-f", conf.to_str().unwrap()])
            .args(["-S", socket.to_str().unwrap()])
            .args([
                "new-session",
                "-d",
                "-x",
                &size.0.to_string(),
                "-y",
                &size.1.to_string(),
            ])
            .args(["-c", dir.to_str().unwrap(), command])
            .output()?;
        assert!(
            out.status.success(),
            "tmux parity new-session failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = Command::new("tmux")
            .args(["-S", socket.to_str().unwrap()])
            .args(["set-option", "-g", "window-size", "manual"])
            .output()?;
        assert!(
            out.status.success(),
            "tmux parity set window-size failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(Self { socket })
    }

    /// `$TMUX` value pointing at this server.
    pub fn tmux_var(&self) -> OsString {
        format!("{},0,0", self.socket.display()).into()
    }

    pub fn cmd(&self, args: &[&str]) -> io::Result<Output> {
        Command::new("tmux")
            .args(["-S", self.socket.to_str().unwrap()])
            .args(args)
            .output()
    }

    /// Poll until a pane running `direnv-instant watch` appears, return its id.
    pub fn wait_for_watch_pane(&self, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(out) = self.cmd(&[
                "list-panes",
                "-a",
                "-F",
                "#{pane_id} #{pane_current_command}",
            ]) {
                let stdout = String::from_utf8_lossy(&out.stdout);
                for line in stdout.lines() {
                    if line.contains("direnv-instant") || line.contains("watch") {
                        return line.split_whitespace().next().map(str::to_owned);
                    }
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn pane_tty(&self) -> io::Result<PathBuf> {
        let out = self.cmd(&["display-message", "-p", "#{pane_tty}"])?;
        assert!(
            out.status.success(),
            "tmux display-message failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
    }

    pub fn resize_window(&self, cols: u32, rows: u32) -> io::Result<()> {
        let out = self.cmd(&[
            "resize-window",
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
        ])?;
        assert!(
            out.status.success(),
            "tmux resize-window failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Ok(())
    }

    pub fn capture_window_grid(&self, cols: usize, rows: usize) -> io::Result<Vec<String>> {
        let mut grid = vec![vec![' '; cols]; rows];
        let mut pane_cells = vec![vec![false; cols]; rows];
        let panes = self.cmd(&[
            "list-panes",
            "-F",
            "#{pane_id}|#{pane_left}|#{pane_top}|#{pane_width}|#{pane_height}",
        ])?;
        assert!(
            panes.status.success(),
            "tmux list-panes failed: {}",
            String::from_utf8_lossy(&panes.stderr)
        );

        for line in String::from_utf8_lossy(&panes.stdout).lines() {
            let fields: Vec<&str> = line.split('|').collect();
            assert_eq!(fields.len(), 5, "unexpected list-panes row: {line}");
            let pane_id = fields[0];
            let left: usize = fields[1].parse().unwrap();
            let top: usize = fields[2].parse().unwrap();
            let width: usize = fields[3].parse().unwrap();
            let height: usize = fields[4].parse().unwrap();

            let capture = self.cmd(&["capture-pane", "-p", "-t", pane_id, "-S", "0", "-E", "-"])?;
            assert!(
                capture.status.success(),
                "tmux capture-pane failed: {}",
                String::from_utf8_lossy(&capture.stderr)
            );

            for (dy, captured) in String::from_utf8_lossy(&capture.stdout)
                .lines()
                .take(height)
                .enumerate()
            {
                let row = top + dy;
                if row >= rows {
                    continue;
                }
                for col in left..(left + width).min(cols) {
                    pane_cells[row][col] = true;
                }
                for (dx, ch) in captured.chars().take(width).enumerate() {
                    let col = left + dx;
                    if col < cols {
                        grid[row][col] = ch;
                    }
                }
            }
        }

        let pane_rows: Vec<bool> = pane_cells
            .iter()
            .map(|row| row.iter().any(|&cell| cell))
            .collect();
        for row in 1..rows.saturating_sub(1) {
            if !pane_rows[row]
                && pane_rows[..row].iter().any(|&seen| seen)
                && pane_rows[row + 1..].iter().any(|&seen| seen)
            {
                grid[row].fill('─');
            }
        }

        Ok(grid
            .into_iter()
            .map(|row| row.into_iter().collect::<String>())
            .collect())
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-S", self.socket.to_str().unwrap(), "kill-server"])
            .output();
    }
}

pub struct FakeDaemon {
    pub log_path: PathBuf,
    pub socket_path: PathBuf,
    monitors: Arc<Mutex<Vec<UnixStream>>>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    pty_slave: Option<OwnedFd>,
}

impl FakeDaemon {
    pub fn start(dir: &Path, with_pty: bool) -> io::Result<Self> {
        let log_path = dir.join("daemon.log");
        let socket_path = dir.join("daemon.sock");
        fs::write(&log_path, "")?;
        let _ = fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;
        listener.set_nonblocking(true)?;
        let monitors = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_monitors = monitors.clone();
        let thread_stop = stop.clone();
        let pty = if with_pty {
            Some(openpty(None, None).unwrap())
        } else {
            None
        };
        let pty_master = pty.as_ref().map(|p| p.master.try_clone().unwrap());
        let pty_slave = pty.map(|p| p.slave);

        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handle_daemon_connection(stream, &thread_monitors, pty_master.as_ref());
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            log_path,
            socket_path,
            monitors,
            stop,
            thread: Some(thread),
            pty_slave,
        })
    }

    pub fn append_log(&self, data: &str) -> io::Result<()> {
        let mut f = fs::OpenOptions::new().append(true).open(&self.log_path)?;
        f.write_all(data.as_bytes())?;
        f.flush()
    }

    pub fn wait_for_monitor(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.monitors.lock().unwrap().is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn finish(&self, status: i32) {
        let _ = fs::write(
            self.socket_path.with_file_name("exit_status"),
            status.to_string(),
        );
        self.monitors.lock().unwrap().clear();
    }

    pub fn read_pty_until(&self, needle: &str, timeout: Duration) -> String {
        let Some(slave) = &self.pty_slave else {
            return String::new();
        };
        set_nonblocking(slave);
        let deadline = Instant::now() + timeout;
        let mut out = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match nix::unistd::read(slave, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    out.extend_from_slice(&buf[..n]);
                    if String::from_utf8_lossy(&out).contains(needle) {
                        break;
                    }
                }
                Err(nix::errno::Errno::EAGAIN) => thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.monitors.lock().unwrap().clear();
        let _ = fs::remove_file(&self.socket_path);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_daemon_connection(
    stream: UnixStream,
    monitors: &Arc<Mutex<Vec<UnixStream>>>,
    pty_master: Option<&OwnedFd>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(n) if n > 0 && line.starts_with("WATCH") => send_watch_response(&stream, pty_master),
        Ok(n) if n > 0 && line.starts_with("STOP") => {}
        _ => monitors.lock().unwrap().push(stream),
    }
}

fn send_watch_response(stream: &UnixStream, pty_master: Option<&OwnedFd>) {
    if let Some(master) = pty_master {
        let fds = [master.as_raw_fd()];
        let cmsg = [ControlMessage::ScmRights(&fds)];
        sendmsg::<()>(
            stream.as_raw_fd(),
            &[IoSlice::new(b"OK\n")],
            &cmsg,
            MsgFlags::empty(),
            None,
        )
        .unwrap();
    } else {
        sendmsg::<()>(
            stream.as_raw_fd(),
            &[IoSlice::new(b"ERR\n")],
            &[],
            MsgFlags::empty(),
            None,
        )
        .unwrap();
    }
}

/// Parse `export NAME='value'` lines emitted by `direnv-instant start`.
pub fn parse_exports(stdout: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for line in stdout.lines() {
        let rest = match line.strip_prefix("export ") {
            Some(r) => r,
            None => line,
        };
        if let Some((k, v)) = rest.split_once('=') {
            m.insert(
                k.trim().to_owned(),
                v.trim().trim_matches(|c| c == '\'' || c == '"').to_owned(),
            );
        }
    }
    m
}

/// Poll until `path` exists and is non-empty.
pub fn wait_for_file(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() && fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Poll until the daemon socket disappears or stops accepting connections.
pub fn wait_for_daemon_exit(socket_path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !socket_path.exists() {
            return true;
        }
        if UnixStream::connect(socket_path).is_err() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// Read a child's stderr until a substring appears or the timeout hits.
///
/// The pipe must be non-blocking so the deadline can fire between reads;
/// otherwise a stalled child that keeps stderr open hangs the test forever.
pub fn read_stderr_until(child: &mut Child, needle: &str, timeout: Duration) -> String {
    let stderr = child.stderr.as_mut().expect("stderr piped");
    set_nonblocking(&*stderr);
    let mut buf = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut chunk = [0u8; 4096];
    loop {
        match stderr.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if String::from_utf8_lossy(&buf).contains(needle) {
                    break;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break,
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn set_nonblocking(fd: &impl std::os::fd::AsFd) {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL).unwrap());
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).unwrap();
}

/// Build a PATH with `extra` prepended to the inherited one.
pub fn prepend_path(extra: &[&Path]) -> OsString {
    let mut parts: Vec<PathBuf> = extra.iter().map(|p| p.to_path_buf()).collect();
    if let Some(p) = env::var_os("PATH") {
        parts.extend(env::split_paths(&p));
    }
    env::join_paths(parts).unwrap()
}

fn which(name: &str) -> Option<PathBuf> {
    env::var_os("PATH").and_then(|paths| {
        env::split_paths(&paths)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    })
}

/// Skip the test (return early) if a binary isn't available.
#[macro_export]
macro_rules! require {
    ($bin:expr) => {
        if std::env::var_os("PATH")
            .map(|p| {
                std::env::split_paths(&p)
                    .map(|d| d.join($bin))
                    .find(|p| p.is_file())
            })
            .flatten()
            .is_none()
        {
            eprintln!("skipping: {} not on PATH", $bin);
            return;
        }
    };
}

// Tiny TempDir to avoid pulling in the `tempfile` crate.
pub mod tempdir {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(prefix: &str) -> io::Result<Self> {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("{}-{}-{}", prefix, std::process::id(), n));
            std::fs::create_dir_all(&p)?;
            Ok(Self(p))
        }
        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
