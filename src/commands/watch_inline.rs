use std::io::Read;
use std::os::unix::net::UnixStream;
use std::path::Path;

pub fn run(_log_path: &Path, socket_path: &Path, _tty_path: &Path, _project_name: Option<&str>) {
    let Ok(mut socket) = UnixStream::connect(socket_path) else {
        return;
    };

    let mut buf = [0u8; 1024];
    loop {
        match socket.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}
