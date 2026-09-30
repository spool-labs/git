//! Git-style progress lines on stderr
//!
//! Git tells the helper whether to show progress (`option progress`), and only
//! does so when its own stderr is a terminal, so a disabled meter prints nothing.

use std::io::Write;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::AsyncWrite;

/// Redraws come at most this often
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

const KIB: f64 = 1024.0;
const MIB: f64 = KIB * 1024.0;
const GIB: f64 = MIB * 1024.0;

/// One progress line over a count of items and their bytes, redrawn in place
pub struct Progress {
    title: String,
    items: usize,
    items_done: usize,
    bytes: u64,
    bytes_done: u64,
    started: Instant,
    drawn: Option<Instant>,
    /// Length of the last line drawn, so a shorter one can blank what it leaves behind
    drawn_len: usize,
    is_enabled: bool,
}

impl Progress {
    /// A meter over `items` totalling `bytes`, drawn at once so a slow first chunk still shows a line
    pub fn start(title: &str, items: usize, bytes: u64, is_enabled: bool) -> Self {
        let mut progress = Self {
            title: title.to_string(),
            items,
            items_done: 0,
            bytes,
            bytes_done: 0,
            started: Instant::now(),
            drawn: None,
            drawn_len: 0,
            is_enabled,
        };
        progress.draw("");
        progress
    }

    /// Count bytes received, redrawing when the last draw is old enough
    pub fn advance(&mut self, bytes: u64) {
        self.bytes_done += bytes;
        let is_due = match self.drawn {
            Some(drawn) => drawn.elapsed() >= REDRAW_INTERVAL,
            None => true,
        };
        if is_due {
            self.draw("");
        }
    }

    /// Count one item complete
    pub fn complete_item(&mut self) {
        self.items_done += 1;
        self.draw("");
    }

    /// Draw the final line
    pub fn finish(&mut self) {
        self.draw(", done.\n");
    }

    fn draw(&mut self, end: &str) {
        if !self.is_enabled {
            return;
        }
        let line = render(
            &self.title,
            (self.items_done, self.items),
            (self.bytes_done, self.bytes),
            self.started.elapsed(),
        );
        let pad = self.drawn_len.saturating_sub(line.len());
        let mut stderr = std::io::stderr();
        let _ = write!(stderr, "\r{line}{:pad$}{end}", "");
        let _ = stderr.flush();
        self.drawn = Some(Instant::now());
        self.drawn_len = line.len();
    }
}

/// `Downloading packs:  37% (3/13), 172.50 MiB | 12.10 MiB/s`, the percentage by bytes
fn render(title: &str, items: (usize, usize), bytes: (u64, u64), elapsed: Duration) -> String {
    let (items_done, items_total) = items;
    let (bytes_done, bytes_total) = bytes;
    let percent = match bytes_total {
        0 => 100,
        total => (bytes_done.saturating_mul(100) / total).min(100),
    };
    let seconds = elapsed.as_secs_f64();
    let rate = if seconds > 0.0 {
        bytes_done as f64 / seconds
    } else {
        0.0
    };

    format!(
        "{title}: {percent:>3}% ({items_done}/{items_total}), {} | {}/s",
        human(bytes_done as f64),
        human(rate),
    )
}

/// Bytes in the units git prints them in
fn human(bytes: f64) -> String {
    if bytes < KIB {
        format!("{bytes:.0} bytes")
    } else if bytes < MIB {
        format!("{:.2} KiB", bytes / KIB)
    } else if bytes < GIB {
        format!("{:.2} MiB", bytes / MIB)
    } else {
        format!("{:.2} GiB", bytes / GIB)
    }
}

/// Collects a stream's bytes while advancing its meter
pub struct MeteredBuffer<'meter> {
    pub bytes: Vec<u8>,
    pub progress: &'meter mut Progress,
}

impl AsyncWrite for MeteredBuffer<'_> {
    fn poll_write(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        this.bytes.extend_from_slice(buf);
        this.progress.advance(buf.len() as u64);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // the line shows the byte percentage, the item count, the size and the rate
    #[test]
    fn line_format() {
        let mib = 1024 * 1024;

        let line = render("Downloading packs", (3, 13), (100 * mib, 400 * mib), Duration::from_secs(10));

        assert_eq!(line, "Downloading packs:  25% (3/13), 100.00 MiB | 10.00 MiB/s");
    }

    // nothing to download reads as complete
    #[test]
    fn empty_total() {
        let line = render("Downloading packs", (0, 0), (0, 0), Duration::from_secs(1));

        assert!(line.starts_with("Downloading packs: 100% (0/0), 0 bytes"));
    }

    // sizes scale through the units git uses
    #[test]
    fn human_units() {
        assert_eq!(human(512.0), "512 bytes");
        assert_eq!(human(1536.0), "1.50 KiB");
        assert_eq!(human(3.0 * MIB), "3.00 MiB");
        assert_eq!(human(2.5 * GIB), "2.50 GiB");
    }

    // a metered buffer keeps every byte and counts them
    #[test]
    fn metered_write() {
        let mut progress = Progress::start("test", 1, 6, false);
        let mut buffer = MeteredBuffer { bytes: Vec::new(), progress: &mut progress };
        let mut context = Context::from_waker(std::task::Waker::noop());

        let first = Pin::new(&mut buffer).poll_write(&mut context, b"abc");
        let second = Pin::new(&mut buffer).poll_write(&mut context, b"def");

        assert!(matches!(first, Poll::Ready(Ok(3))));
        assert!(matches!(second, Poll::Ready(Ok(3))));
        assert_eq!(buffer.bytes, b"abcdef");
        assert_eq!(progress.bytes_done, 6);
    }
}
