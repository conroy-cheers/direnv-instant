//! Parity harness for the tmux split-pane backend and the inline
//! scroll-region backend.

mod common;
use common::*;

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const COLS: usize = 60;
const ROWS: usize = 40;

#[derive(Clone, Copy)]
enum Backend {
    Tmux,
    Inline,
}

fn parity_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct BackendRun {
    _tmp: tempdir::TempDir,
    _sink: SignalSink,
    server: TmuxServer,
    input: Option<File>,
    status_path: PathBuf,
    done_path: PathBuf,
    input_path: PathBuf,
    start_out: PathBuf,
}

impl BackendRun {
    fn capture(&self) -> Vec<String> {
        self.server.capture_window_grid(COLS, ROWS).unwrap()
    }

    fn append_log(&mut self, data: &str) {
        let input = self.input.as_mut().expect("input FIFO is still open");
        input.write_all(data.as_bytes()).unwrap();
        input.flush().unwrap();
    }

    fn finish(&mut self, status: i32) {
        fs::write(&self.status_path, status.to_string()).unwrap();
        fs::write(&self.done_path, "").unwrap();
        drop(self.input.take());
    }
}

impl Drop for BackendRun {
    fn drop(&mut self) {
        if self.input.is_some() {
            self.finish(0);
            thread::sleep(Duration::from_millis(100));
        }
        let _ = self.server.cmd(&["kill-server"]);
    }
}

fn start_reference() -> BackendRun {
    start_backend(Backend::Tmux, ROWS, "", "sleep 3600")
}

fn start_inline() -> BackendRun {
    start_backend(Backend::Inline, ROWS, "", "sleep 3600")
}

fn start_typing_backend(backend: Backend) -> BackendRun {
    let pre_start = format!("printf %s {}; ", sh_quote_str(&"\n".repeat(ROWS - 1)));
    start_backend(
        backend,
        ROWS,
        &pre_start,
        "old=$(stty -g); trap 'stty \"$old\"' EXIT; stty raw -echo; printf 'prompt$ '; while true; do sleep 0.03; IFS= read -r -n 1 ch || break; printf %s \"$ch\"; done",
    )
}

fn start_prompt_backend(backend: Backend, prompt_row: usize, prompt: &str) -> BackendRun {
    assert!((1..=ROWS).contains(&prompt_row));
    let pre_start = if prompt_row == 1 {
        String::new()
    } else {
        let blank_lines = prompt_row.saturating_sub(2);
        format!(
            "printf 'previous command output\\n'; printf %s {}; ",
            sh_quote_str(&"\n".repeat(blank_lines))
        )
    };
    start_backend(
        backend,
        prompt_row,
        &pre_start,
        &format!("printf {}; sleep 3600", sh_quote_str(prompt)),
    )
}

fn start_backend(
    backend: Backend,
    cursor_row: usize,
    pre_start: &str,
    post_start: &str,
) -> BackendRun {
    let tmp = tempdir::TempDir::new(match backend {
        Backend::Tmux => "direnv-instant-parity-tmux",
        Backend::Inline => "direnv-instant-parity-inline",
    })
    .unwrap();
    let root = tmp.path();
    let work = root.join("work");
    let home = root.join("home");
    let bin_dir = root.join("bin");
    fs::create_dir_all(&work).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(work.join(".envrc"), "true\n").unwrap();

    let input_path = root.join("direnv-stderr.log");
    let status_path = root.join("direnv-status");
    let done_path = root.join("direnv-done");
    fs::write(&input_path, "").unwrap();
    fs::write(&status_path, "0").unwrap();
    write_fake_direnv(&bin_dir.join("direnv")).unwrap();

    let server = TmuxServer::with_parity_config(root, (COLS as u32, ROWS as u32)).unwrap();
    let pane_tty = server.pane_tty().unwrap();
    let sink = SignalSink::new().unwrap();
    let path = prepend_path(&[&bin_dir]);
    let start_out = root.join("start.out");

    let env_parts = vec![
        shell_env("HOME", &home),
        shell_env("XDG_DATA_HOME", &home.join(".local/share")),
        shell_env_os("PATH", &path),
        shell_env_str("TERM", "xterm-256color"),
        shell_env_str("DIRENV_INSTANT_MUX_DELAY", "0"),
        shell_env_str("DIRENV_INSTANT_CURSOR_ROW", &cursor_row.to_string()),
        shell_env_str("DIRENV_INSTANT_SHELL_PID", &sink.pid().to_string()),
        shell_env("DIRENV_INSTANT_TTY", &pane_tty),
        shell_env("DIRENV_PARITY_INPUT", &input_path),
        shell_env("DIRENV_PARITY_STATUS_FILE", &status_path),
        shell_env("DIRENV_PARITY_DONE_FILE", &done_path),
    ];

    let env_prefix = if matches!(backend, Backend::Inline) {
        format!("env -u TMUX {}", env_parts.join(" "))
    } else {
        format!("env {}", env_parts.join(" "))
    };
    let command = format!(
        "cd {} && {} {} {} start > {}; {}",
        sh_quote(&work),
        pre_start,
        env_prefix,
        sh_quote(&bin()),
        sh_quote(&start_out),
        post_start,
    );
    let out = server.cmd(&["respawn-pane", "-k", &command]).unwrap();
    assert!(
        out.status.success(),
        "tmux respawn-pane failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let input = OpenOptions::new().append(true).open(&input_path).unwrap();

    BackendRun {
        _tmp: tmp,
        _sink: sink,
        server,
        input: Some(input),
        status_path,
        done_path,
        input_path,
        start_out,
    }
}

fn write_fake_direnv(path: &Path) -> std::io::Result<()> {
    let bash = which("bash").expect("bash on PATH");
    fs::write(
        path,
        format!(
            r#"#!{}
set -euo pipefail

if [[ "${{1:-}}" == allow ]]; then
  exit 0
fi

if [[ "${{1:-}}" != export ]]; then
  exit 0
fi

pos=0
while true; do
  size=$(wc -c < "$DIRENV_PARITY_INPUT")
  if (( size > pos )); then
    count=$((size - pos))
    dd if="$DIRENV_PARITY_INPUT" bs=1 skip="$pos" count="$count" status=none >&2
    pos=$size
  fi
  if [[ -e "$DIRENV_PARITY_DONE_FILE" ]]; then
    break
  fi
  sleep 0.05
done

status=$(cat "$DIRENV_PARITY_STATUS_FILE" 2>/dev/null || printf '0')
if [[ "$status" == 0 ]]; then
  printf 'export DIRENV_PARITY_DONE=1\n'
fi
exit "$status"
"#,
            bash.display()
        ),
    )?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|path| path.is_file())
    })
}

fn shell_env(key: &str, value: &Path) -> String {
    shell_env_str(key, &value.to_string_lossy())
}

fn shell_env_os(key: &str, value: &std::ffi::OsStr) -> String {
    shell_env_str(key, &value.to_string_lossy())
}

fn shell_env_str(key: &str, value: &str) -> String {
    format!("{key}={}", sh_quote_str(value))
}

fn sh_quote(path: &Path) -> String {
    sh_quote_str(&path.to_string_lossy())
}

fn sh_quote_str(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn wait_for_grid(run: &BackendRun, needle: &str) -> Vec<String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let grid = run.capture();
        if grid.iter().any(|row| row.contains(needle)) {
            return grid;
        }
        if Instant::now() >= deadline {
            let panes = run
                .server
                .cmd(&[
                    "list-panes",
                    "-F",
                    "#{pane_id} #{pane_current_command} #{pane_width}x#{pane_height}",
                ])
                .ok()
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
                .unwrap_or_default();
            let start_out = fs::read_to_string(&run.start_out).unwrap_or_default();
            panic!(
                "timed out waiting for {needle:?}; panes:\n{panes}\nstart.out:\n{start_out}\ngrid:\n{}",
                grid.join("\n")
            );
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn assert_grids_eq(reference: Vec<String>, inline: Vec<String>) {
    assert_eq!(
        reference,
        inline,
        "\nreference:\n{}\n\ninline:\n{}",
        reference.join("\n"),
        inline.join("\n")
    );
}

fn row_containing(grid: &[String], needle: &str) -> Option<usize> {
    grid.iter().position(|row| row.contains(needle))
}

fn assert_prompt_position_matches(prompt_row: usize) {
    let prompt = format!("prompt$ row {prompt_row:02}");
    let log_line = format!("position row {prompt_row:02}\n");
    let log_text = log_line.trim_end();

    let mut reference = start_prompt_backend(Backend::Tmux, prompt_row, &prompt);
    let _ = wait_for_grid(&reference, &prompt);
    reference.append_log(&log_line);
    let reference_grid = wait_for_grid(&reference, log_text);
    let reference_prompt_row = row_containing(&reference_grid, &prompt).unwrap_or_else(|| {
        panic!(
            "reference lost prompt for initial row {prompt_row}:\n{}",
            reference_grid.join("\n")
        )
    });

    let mut inline = start_prompt_backend(Backend::Inline, prompt_row, &prompt);
    let _ = wait_for_grid(&inline, &prompt);
    inline.append_log(&log_line);
    let inline_grid = wait_for_grid(&inline, log_text);
    assert_eq!(
        row_containing(&inline_grid, &prompt),
        Some(reference_prompt_row),
        "prompt row mismatch for initial row {prompt_row}\nreference:\n{}\n\ninline:\n{}",
        reference_grid.join("\n"),
        inline_grid.join("\n")
    );

    reference.finish(0);
    thread::sleep(Duration::from_millis(300));
    let reference_closed_grid = reference.capture();

    inline.finish(0);
    thread::sleep(Duration::from_millis(300));
    let inline_closed_grid = inline.capture();

    assert_grids_eq(reference_closed_grid, inline_closed_grid);
}

fn type_during_log_updates(run: &BackendRun, text: &str) -> Vec<String> {
    let input_path = run.input_path.clone();
    let writer = thread::spawn(move || {
        let mut input = OpenOptions::new().append(true).open(input_path).unwrap();
        for i in 1..=120 {
            writeln!(input, "typing L{i:03} redraw race").unwrap();
            input.flush().unwrap();
            thread::sleep(Duration::from_millis(5));
        }
    });

    for ch in text.chars() {
        thread::sleep(Duration::from_millis(20));
        run.server
            .cmd(&["send-keys", "-l", &ch.to_string()])
            .unwrap();
    }

    writer.join().unwrap();
    let _ = wait_for_grid(run, "typing L120");
    wait_for_grid(run, &format!("prompt$ {text}"))
}

#[test]
fn running_log_and_ansi_output_match() {
    let _lock = parity_lock();
    require!("tmux");
    let mut reference = start_reference();

    let mut lines = Vec::new();
    for i in 1..=4 {
        let line = if i == 4 {
            "L04 \x1b[31mcolored\x1b[0m text\n".to_owned()
        } else {
            format!("L{i:02} parity line\n")
        };
        lines.push(line);
    }

    for line in &lines {
        reference.append_log(&line);
    }

    let reference_grid = wait_for_grid(&reference, "L04 colored text");

    let mut inline = start_inline();
    for line in &lines {
        inline.append_log(&line);
    }
    let inline_grid = wait_for_grid(&inline, "L04 colored text");

    assert_grids_eq(reference_grid, inline_grid);
}

#[test]
fn in_progress_shell_input_survives_log_updates() {
    let _lock = parity_lock();
    require!("tmux");
    let typed = "draft command while direnv renders";
    let prompt = format!("prompt$ {typed}");

    let reference = start_typing_backend(Backend::Tmux);
    let _ = wait_for_grid(&reference, "prompt$");
    let reference_grid = type_during_log_updates(&reference, typed);
    assert!(
        reference_grid.iter().any(|row| row.contains(&prompt)),
        "reference lost the in-progress shell input:\n{}",
        reference_grid.join("\n")
    );

    let inline = start_typing_backend(Backend::Inline);
    let _ = wait_for_grid(&inline, "prompt$");
    let inline_grid = type_during_log_updates(&inline, typed);
    assert!(
        inline_grid.iter().any(|row| row.contains(&prompt)),
        "inline backend lost the in-progress shell input:\n{}",
        inline_grid.join("\n")
    );

    assert_grids_eq(reference_grid, inline_grid);
}

#[test]
fn prompt_position_matches_for_every_initial_row() {
    let _lock = parity_lock();
    require!("tmux");

    for prompt_row in 1..=ROWS {
        assert_prompt_position_matches(prompt_row);
    }
}

#[test]
fn resize_layout_and_recent_lines_match() {
    let _lock = parity_lock();
    require!("tmux");
    let mut reference = start_reference();

    for i in 1..=8 {
        let line = format!("R{i:02}\n");
        reference.append_log(&line);
    }
    let _ = wait_for_grid(&reference, "R08");

    reference.server.resize_window(COLS as u32, 24).unwrap();
    let reference_grid = reference.server.capture_window_grid(COLS, 24).unwrap();

    let mut inline = start_inline();
    for i in 1..=8 {
        let line = format!("R{i:02}\n");
        inline.append_log(&line);
    }
    let _ = wait_for_grid(&inline, "R08");
    inline.server.resize_window(COLS as u32, 24).unwrap();
    let pane = inline
        .server
        .cmd(&["display-message", "-p", "#{pane_pid}"])
        .unwrap();
    let pane_pid: i32 = String::from_utf8_lossy(&pane.stdout)
        .trim()
        .parse()
        .unwrap();
    kill(Pid::from_raw(pane_pid), Signal::SIGWINCH).unwrap();
    thread::sleep(Duration::from_millis(300));

    let inline_grid = inline.server.capture_window_grid(COLS, 24).unwrap();
    assert_eq!(reference_grid, inline_grid);
}

#[test]
fn success_teardown_matches() {
    let _lock = parity_lock();
    require!("tmux");
    let mut reference = start_reference();

    reference.append_log("done soon\n");
    let _ = wait_for_grid(&reference, "done soon");

    reference.finish(0);
    thread::sleep(Duration::from_millis(300));
    let reference_grid = reference.capture();

    let mut inline = start_inline();
    inline.append_log("done soon\n");
    let _ = wait_for_grid(&inline, "done soon");
    inline.finish(0);
    thread::sleep(Duration::from_millis(300));
    let inline_grid = inline.capture();
    assert_grids_eq(reference_grid, inline_grid);
}

#[test]
fn failure_teardown_matches() {
    let _lock = parity_lock();
    require!("tmux");
    let mut reference = start_reference();

    reference.append_log("error: failed\ncontext\n");
    let _ = wait_for_grid(&reference, "context");

    reference.finish(2);
    thread::sleep(Duration::from_millis(300));
    let reference_grid = reference.capture();

    let mut inline = start_inline();
    inline.append_log("error: failed\ncontext\n");
    let _ = wait_for_grid(&inline, "context");
    inline.finish(2);
    thread::sleep(Duration::from_millis(300));
    let inline_grid = inline.capture();
    assert_grids_eq(reference_grid, inline_grid);
}
