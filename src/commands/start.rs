use crate::daemon::{
    DaemonContext, direnv_export_command, get_socket_path, notify_daemon, start_daemon, stop_daemon,
};
use crate::mux::Multiplexer;
use crate::nushell;
use crate::shell::Shell;
use nix::unistd::getppid;
use std::env;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;

pub fn run() {
    let shell = Shell::from_env();
    let parent_pid = env::var("DIRENV_INSTANT_SHELL_PID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| getppid().as_raw());

    if shell == Shell::Nushell {
        nushell::run(parent_pid, find_envrc);
        return;
    }

    let direnv = "direnv";

    // Find .envrc directory
    let envrc_dir = match find_envrc() {
        Some(dir) => dir,
        None => {
            shell.unset_var("__DIRENV_INSTANT_CURRENT_DIR");
            run_direnv_sync(direnv, shell, false);
            return;
        }
    };

    if let Ok(current) = env::var("__DIRENV_INSTANT_CURRENT_DIR") {
        let current_dir = PathBuf::from(&current);
        if current_dir != envrc_dir {
            stop_daemon(&get_socket_path(&current_dir));
        }
    }
    shell.export_var(
        "__DIRENV_INSTANT_CURRENT_DIR",
        &envrc_dir.display().to_string(),
    );

    if Multiplexer::detect().is_none() {
        run_direnv_sync(direnv, shell, true);
        return;
    }

    let ctx = match DaemonContext::new(parent_pid, envrc_dir, shell) {
        Ok(ctx) => ctx,
        Err(e) => {
            eprintln!("direnv-instant: Failed to create temp files: {}", e);
            run_direnv_sync(direnv, shell, true);
            return;
        }
    };
    shell.export_var(
        "__DIRENV_INSTANT_ENV_FILE",
        &ctx.env_file.display().to_string(),
    );
    shell.export_var(
        "__DIRENV_INSTANT_STDERR_FILE",
        &ctx.stderr_file.display().to_string(),
    );

    if ctx.socket_path.exists() && notify_daemon(&ctx.socket_path, parent_pid) {
        ctx.cleanup_temp_files();
        return;
    }

    start_daemon(direnv, &ctx);
}

fn find_envrc() -> Option<PathBuf> {
    let mut dir = env::current_dir().ok()?;
    loop {
        if dir.join(".envrc").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn run_direnv_sync(direnv: &str, shell: Shell, show_errors: bool) {
    let mut cmd = direnv_export_command(direnv, shell);
    if !show_errors {
        cmd.stderr(Stdio::null());
    }

    let err = cmd.exec();

    eprintln!("direnv-instant: Failed to exec direnv: {}", err);
    std::process::exit(1);
}
