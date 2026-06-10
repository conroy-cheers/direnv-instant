use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};

const DEFAULT_MIN_PANE_ROWS: u16 = 2;
const DEFAULT_MAX_PANE_ROWS: u16 = 10;
const DEFAULT_PANE_ROWS: u16 = 10;
const DEFAULT_MIN_TOP_ROWS: u16 = 22;

#[derive(Debug, Clone, Copy)]
pub struct SplitConfig {
    pub min_pane_rows: u16,
    pub max_pane_rows: u16,
    pub preferred_pane_rows: u16,
    pub min_top_rows: u16,
    pub cursor_row: Option<u16>,
}

impl SplitConfig {
    pub fn tmux_like(cursor_row: u16) -> Self {
        Self {
            min_pane_rows: DEFAULT_MIN_PANE_ROWS,
            max_pane_rows: DEFAULT_MAX_PANE_ROWS,
            preferred_pane_rows: DEFAULT_PANE_ROWS,
            min_top_rows: DEFAULT_MIN_TOP_ROWS,
            cursor_row: Some(cursor_row),
        }
    }

    pub fn tmux_like_current_row() -> Self {
        Self {
            min_pane_rows: DEFAULT_MIN_PANE_ROWS,
            max_pane_rows: DEFAULT_MAX_PANE_ROWS,
            preferred_pane_rows: DEFAULT_PANE_ROWS,
            min_top_rows: DEFAULT_MIN_TOP_ROWS,
            cursor_row: None,
        }
    }
}

pub struct SplitPane {
    tty: File,
    config: SplitConfig,
    total_rows: u16,
    cols: u16,
    pane_rows: u16,
    ring: Vec<Vec<u8>>,
    cursor_row: usize,
    cursor_col: usize,
    in_esc: bool,
    separator: Vec<u8>,
    buf: Vec<u8>,
    overflow: u16,
}

impl SplitPane {
    pub fn new(tty: File, config: SplitConfig) -> Self {
        let (total_rows, cols) = get_winsize(tty.as_raw_fd()).unwrap_or((24, 80));
        let pane_rows = pane_rows_for_height(total_rows, config);
        Self {
            tty,
            config,
            total_rows,
            cols,
            pane_rows,
            ring: vec![Vec::new(); pane_rows as usize],
            cursor_row: 0,
            cursor_col: 0,
            in_esc: false,
            separator: build_separator(cols),
            buf: Vec::with_capacity(1024),
            overflow: 0,
        }
    }

    pub fn pane_size(&self) -> (u16, u16) {
        (self.pane_rows, self.cols)
    }

    pub fn setup(&mut self) -> io::Result<()> {
        let total = self.total_rows;
        let pane_space = self.pane_rows + 1;
        let cursor_row = self.config.cursor_row.unwrap_or(total);
        let overflow = (cursor_row + pane_space).saturating_sub(total);
        self.overflow = overflow;

        self.buf.clear();
        self.buf.extend_from_slice(b"\x1b7");

        if overflow > 0 {
            write!(
                self.buf,
                "\x1b[{};1H\x1b[{}M",
                self.shell_bottom(),
                overflow
            )?;
        }

        self.write_shell_scroll_region()?;
        write!(self.buf, "\x1b[{};1H", self.separator_row())?;
        self.buf.extend_from_slice(&self.separator);
        for row in self.pane_start()..=total {
            write!(self.buf, "\x1b[{};1H\x1b[2K", row)?;
        }

        self.buf.extend_from_slice(b"\x1b8");
        if overflow > 0 {
            write!(self.buf, "\x1b[{}A", overflow)?;
        }

        self.flush_buf()
    }

    pub fn teardown(&mut self) -> io::Result<()> {
        let total = self.total_rows;

        self.buf.clear();
        self.buf.extend_from_slice(b"\x1b7");
        self.write_full_scroll_region()?;
        for row in self.separator_row()..=total {
            write!(self.buf, "\x1b[{};1H\x1b[2K", row)?;
        }
        if self.overflow > 0 {
            write!(
                self.buf,
                "\x1b[{};1H\x1b[{}L",
                self.shell_bottom(),
                self.overflow
            )?;
        }
        self.buf.extend_from_slice(b"\x1b8");
        if self.overflow > 0 {
            write!(self.buf, "\x1b[{}B", self.overflow)?;
        }

        self.flush_buf()
    }

    pub fn handle_resize(&mut self) -> io::Result<()> {
        if let Some((rows, cols)) = get_winsize(self.tty.as_raw_fd()) {
            if rows == self.total_rows && cols == self.cols {
                return Ok(());
            }
            self.total_rows = rows;
            self.cols = cols;
            let new_pane_rows = pane_rows_for_height(rows, self.config);
            if new_pane_rows != self.pane_rows {
                self.pane_rows = new_pane_rows;
                self.resize_ring();
            }
            self.separator = build_separator(cols);

            self.buf.clear();
            self.write_shell_scroll_region()?;
            write!(self.buf, "\x1b[{};1H\x1b[2K", self.separator_row())?;
            self.buf.extend_from_slice(&self.separator);
            self.flush_buf()?;
        }
        Ok(())
    }

    pub fn ingest(&mut self, data: &[u8]) {
        for &b in data {
            match b {
                b'\r' => {
                    self.cursor_col = 0;
                }
                b'\n' => self.newline(),
                b'\x1b' => {
                    self.in_esc = true;
                    self.ring[self.cursor_row].push(b);
                }
                _ => {
                    self.ring[self.cursor_row].push(b);
                    if self.in_esc {
                        if b.is_ascii_alphabetic() {
                            self.in_esc = false;
                        }
                    } else {
                        self.cursor_col += 1;
                        if self.cursor_col >= self.cols as usize {
                            self.newline();
                        }
                    }
                }
            }
        }
    }

    pub fn redraw(&mut self) -> io::Result<()> {
        let pane_rows = self.pane_rows as usize;
        let pane_start = self.pane_start() as usize;

        self.buf.clear();
        self.buf.extend_from_slice(b"\x1b7");
        self.write_shell_scroll_region()?;

        write!(self.buf, "\x1b[{};1H\x1b[2K", self.separator_row())?;
        self.buf.extend_from_slice(&self.separator);

        for i in 0..pane_rows {
            let row = pane_start + i;
            write!(self.buf, "\x1b[{};1H\x1b[2K", row)?;
            self.buf.extend_from_slice(&self.ring[i]);
        }

        self.buf.extend_from_slice(b"\x1b8");
        self.flush_buf()
    }

    fn separator_row(&self) -> u16 {
        self.total_rows - self.pane_rows
    }

    fn pane_start(&self) -> u16 {
        self.separator_row() + 1
    }

    fn shell_bottom(&self) -> u16 {
        self.separator_row() - 1
    }

    fn write_shell_scroll_region(&mut self) -> io::Result<()> {
        write!(self.buf, "\x1b[1;{}r", self.shell_bottom())
    }

    fn write_full_scroll_region(&mut self) -> io::Result<()> {
        write!(self.buf, "\x1b[1;{}r", self.total_rows)
    }

    fn resize_ring(&mut self) {
        let new_cap = self.pane_rows as usize;
        let old_count = (self.cursor_row + 1).min(self.ring.len());
        let keep = old_count.min(new_cap);
        let mut new_ring = vec![Vec::new(); new_cap];
        let skip = old_count.saturating_sub(keep);
        for (i, slot) in new_ring.iter_mut().take(keep).enumerate() {
            let old_idx = skip + i;
            *slot = std::mem::take(&mut self.ring[old_idx]);
        }
        self.ring = new_ring;
        self.cursor_row = keep.saturating_sub(1).min(new_cap.saturating_sub(1));
    }

    fn newline(&mut self) {
        if self.cursor_row + 1 < self.ring.len() {
            self.cursor_row += 1;
        } else {
            self.ring.rotate_left(1);
            self.ring.last_mut().unwrap().clear();
        }
        self.cursor_col = 0;
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        self.tty.write_all(&self.buf)?;
        self.tty.flush()
    }
}

pub fn set_winsize(fd: RawFd, rows: u16, cols: u16) {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &ws) };
}

fn pane_rows_for_height(total_rows: u16, config: SplitConfig) -> u16 {
    total_rows
        .saturating_sub(config.min_top_rows)
        .min(config.preferred_pane_rows)
        .clamp(config.min_pane_rows, config.max_pane_rows)
}

fn get_winsize(fd: RawFd) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if ret == 0 {
        Some((ws.ws_row, ws.ws_col))
    } else {
        None
    }
}

fn build_separator(cols: u16) -> Vec<u8> {
    "─".repeat(cols as usize).into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, OpenOptions};
    use std::io::Read;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_output() -> (File, File, std::path::PathBuf) {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "vt100-split-test-{}-{n}",
            std::process::id()
        ));
        let writer = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let reader = OpenOptions::new().read(true).open(&path).unwrap();
        (writer, reader, path)
    }

    fn read_new(reader: &mut File) -> String {
        let mut output = Vec::new();
        reader.read_to_end(&mut output).unwrap();
        String::from_utf8(output).unwrap()
    }

    fn pane_with_rows(rows: u16) -> (SplitPane, File, std::path::PathBuf) {
        let (writer, reader, path) = temp_output();
        let pane = SplitPane::new(
            writer,
            SplitConfig {
                min_pane_rows: rows,
                max_pane_rows: rows,
                preferred_pane_rows: rows,
                min_top_rows: 0,
                cursor_row: Some(24),
            },
        );
        (pane, reader, path)
    }

    fn row_text(pane: &SplitPane, row: usize) -> String {
        String::from_utf8(pane.ring[row].clone()).unwrap()
    }

    fn cleanup(pane: SplitPane, reader: File, path: std::path::PathBuf) {
        drop(pane);
        drop(reader);
        let _ = fs::remove_file(path);
    }

    #[test]
    fn partial_line_stays_on_current_row() {
        let (mut pane, reader, path) = pane_with_rows(3);

        pane.ingest(b"partial");

        assert_eq!(row_text(&pane, 0), "partial");
        assert_eq!(pane.cursor_row, 0);
        assert_eq!(pane.cursor_col, "partial".len());

        cleanup(pane, reader, path);
    }

    #[test]
    fn ring_scrolls_when_log_exceeds_pane_height() {
        let (mut pane, reader, path) = pane_with_rows(3);

        pane.ingest(b"L1\nL2\nL3\nL4");

        assert_eq!(row_text(&pane, 0), "L2");
        assert_eq!(row_text(&pane, 1), "L3");
        assert_eq!(row_text(&pane, 2), "L4");

        cleanup(pane, reader, path);
    }

    #[test]
    fn long_lines_wrap_at_visible_width() {
        let (mut pane, reader, path) = pane_with_rows(3);
        let mut line = "x".repeat(80).into_bytes();
        line.push(b'y');

        pane.ingest(&line);

        assert_eq!(pane.ring[0], vec![b'x'; 80]);
        assert_eq!(row_text(&pane, 1), "y");
        assert_eq!(pane.cursor_row, 1);
        assert_eq!(pane.cursor_col, 1);

        cleanup(pane, reader, path);
    }

    #[test]
    fn ansi_escape_bytes_do_not_advance_visible_cursor() {
        let (mut pane, reader, path) = pane_with_rows(3);

        pane.ingest(b"\x1b[31mcolored\x1b[0m text");

        assert_eq!(pane.cursor_col, "colored text".len());
        assert_eq!(row_text(&pane, 0), "\x1b[31mcolored\x1b[0m text");

        cleanup(pane, reader, path);
    }

    #[test]
    fn setup_reserves_shell_region_and_draws_separator() {
        let (mut pane, mut reader, path) = pane_with_rows(2);

        pane.setup().unwrap();
        let output = read_new(&mut reader);

        assert!(output.contains("\x1b[21;1H\x1b[3M"));
        assert!(output.contains("\x1b[1;21r"));
        assert!(output.contains("\x1b[23;1H"));
        assert!(output.contains(&"─".repeat(80)));

        cleanup(pane, reader, path);
    }

    #[test]
    fn teardown_restores_full_scroll_region_and_clears_pane() {
        let (mut pane, mut reader, path) = pane_with_rows(2);

        pane.setup().unwrap();
        let _ = read_new(&mut reader);
        pane.teardown().unwrap();
        let output = read_new(&mut reader);

        assert!(output.contains("\x1b[1;24r"));
        assert!(output.contains("\x1b[23;1H\x1b[2K"));
        assert!(output.contains("\x1b[24;1H\x1b[2K"));
        assert!(output.contains("\x1b[21;1H\x1b[3L"));

        cleanup(pane, reader, path);
    }

    #[test]
    fn redraw_reasserts_shell_scroll_region() {
        let (mut pane, mut reader, path) = pane_with_rows(2);

        pane.setup().unwrap();
        let _ = read_new(&mut reader);
        pane.ingest(b"visible");
        pane.redraw().unwrap();
        let output = read_new(&mut reader);

        assert!(output.contains("\x1b[1;21r"));
        assert!(output.contains("visible"));

        cleanup(pane, reader, path);
    }

    #[test]
    fn redraw_repaints_current_pane_without_new_input() {
        let (mut pane, mut reader, path) = pane_with_rows(2);

        pane.ingest(b"first line\nsecond line");
        pane.redraw().unwrap();
        let first = read_new(&mut reader);
        assert!(first.contains("first line"));
        assert!(first.contains("second line"));

        pane.redraw().unwrap();
        let second = read_new(&mut reader);
        assert!(
            second.contains("first line") && second.contains("second line"),
            "redraw must repair pane contents even when no log bytes changed; got {second:?}"
        );

        cleanup(pane, reader, path);
    }
}
