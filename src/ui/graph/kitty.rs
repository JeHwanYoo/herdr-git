use std::io::{self, IsTerminal, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use crossterm::terminal;

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const PROBE_QUERY: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c";
const PROBE_ACCEPTED: &[u8] = b"\x1b_Gi=31;OK\x1b\\";
const CHUNK_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct GraphicsPlacement {
    pub(super) column: u16,
    pub(super) row: u16,
    pub(super) columns: u16,
    pub(super) rows: u16,
}

pub(super) struct GraphicsSurface {
    image: u32,
    cell: (u32, u32),
    shown: bool,
}

impl GraphicsSurface {
    pub(super) fn connect(image: u32) -> Result<Self, String> {
        let mut surface = Self {
            image,
            cell: (0, 0),
            shown: false,
        };
        surface.refresh_cell_size()?;
        Ok(surface)
    }

    pub(super) fn cell_size(&self) -> (u32, u32) {
        self.cell
    }

    pub(super) fn refresh_cell_size(&mut self) -> Result<(), String> {
        let size = terminal::window_size().map_err(|error| error.to_string())?;
        self.cell = cell_size(size.width, size.height, size.columns, size.rows)?;
        Ok(())
    }

    pub(super) fn show_png(
        &mut self,
        png: &[u8],
        placement: GraphicsPlacement,
    ) -> Result<(), String> {
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(&show_sequence(self.image, png, placement))
            .and_then(|_| stdout.flush())
            .map_err(|error| format!("Kitty graphics output failed: {error}"))?;
        self.shown = true;
        Ok(())
    }

    pub(super) fn hide(&mut self) {
        if !self.shown {
            return;
        }
        self.shown = false;
        let mut stdout = io::stdout().lock();
        stdout
            .write_all(delete_sequence(self.image).as_bytes())
            .and_then(|_| stdout.flush())
            .ok();
    }
}

impl Drop for GraphicsSurface {
    fn drop(&mut self) {
        self.hide();
    }
}

pub(in crate::ui) fn kitty_graphics_supported() -> bool {
    let stdin = io::stdin();
    if !stdin.is_terminal() || !io::stdout().is_terminal() {
        return false;
    }
    let mut stdout = io::stdout();
    if stdout
        .write_all(PROBE_QUERY)
        .and_then(|_| stdout.flush())
        .is_err()
    {
        return false;
    }
    let fd = stdin.as_raw_fd();
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut reply = Vec::new();
    while !has_device_attributes(&reply) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() || !readable(fd, remaining) {
            break;
        }
        let mut buffer = [0u8; 256];
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read <= 0 {
            break;
        }
        reply.extend_from_slice(&buffer[..read as usize]);
    }
    contains(&reply, PROBE_ACCEPTED)
}

fn readable(fd: i32, timeout: Duration) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = i32::try_from(timeout.as_millis())
        .unwrap_or(i32::MAX)
        .max(1);
    unsafe { libc::poll(&mut poll, 1, timeout) > 0 }
}

fn has_device_attributes(reply: &[u8]) -> bool {
    reply.windows(3).enumerate().any(|(start, window)| {
        window == b"\x1b[?"
            && reply[start + 3..]
                .iter()
                .find(|byte| !byte.is_ascii_digit() && **byte != b';')
                == Some(&b'c')
    })
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn cell_size(width: u16, height: u16, columns: u16, rows: u16) -> Result<(u32, u32), String> {
    let cell = (
        u32::from(width)
            .checked_div(u32::from(columns))
            .unwrap_or(0),
        u32::from(height).checked_div(u32::from(rows)).unwrap_or(0),
    );
    if (1..=128).contains(&cell.0) && (1..=256).contains(&cell.1) {
        Ok(cell)
    } else {
        Err("The terminal did not report its cell size".to_owned())
    }
}

fn delete_sequence(image: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={image},q=2\x1b\\")
}

fn show_sequence(image: u32, png: &[u8], placement: GraphicsPlacement) -> Vec<u8> {
    let payload = STANDARD.encode(png);
    let chunks: Vec<&[u8]> = payload.as_bytes().chunks(CHUNK_BYTES).collect();
    let mut sequence = format!(
        "\x1b[?2026h\x1b7{}\x1b[{};{}H",
        delete_sequence(image),
        placement.row + 1,
        placement.column + 1
    )
    .into_bytes();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        let control = if index == 0 {
            format!(
                "a=T,f=100,t=d,i={image},q=2,C=1,z=-1,c={},r={},m={more}",
                placement.columns, placement.rows
            )
        } else {
            format!("m={more}")
        };
        sequence.extend_from_slice(format!("\x1b_G{control};").as_bytes());
        sequence.extend_from_slice(chunk);
        sequence.extend_from_slice(b"\x1b\\");
    }
    sequence.extend_from_slice(b"\x1b8\x1b[?2026l");
    sequence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_waits_for_device_attributes_and_requires_kitty_acceptance() {
        assert!(!has_device_attributes(b"\x1b_Gi=31;OK\x1b\\"));
        assert!(!has_device_attributes(b"\x1b[?62;2"));
        assert!(has_device_attributes(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c"));
        assert!(contains(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c", PROBE_ACCEPTED));
        assert!(!contains(
            b"\x1b_Gi=31;ENOTSUPPORTED\x1b\\\x1b[?62c",
            PROBE_ACCEPTED
        ));
    }

    #[test]
    fn cell_size_requires_a_reported_pixel_size() {
        assert_eq!(cell_size(3104, 1728, 194, 54), Ok((16, 32)));
        assert!(cell_size(0, 0, 194, 54).is_err());
        assert!(cell_size(3104, 1728, 0, 0).is_err());
    }

    #[test]
    fn large_images_are_sent_in_chunks_that_replace_the_previous_image() {
        let png = vec![7u8; CHUNK_BYTES * 2];
        let sequence = String::from_utf8(show_sequence(
            9,
            &png,
            GraphicsPlacement {
                column: 2,
                row: 4,
                columns: 5,
                rows: 6,
            },
        ))
        .unwrap();
        let delete = sequence.find("a=d,d=I,i=9").unwrap();
        let first = sequence
            .find("\x1b[5;3H\x1b_Ga=T,f=100,t=d,i=9,q=2,C=1,z=-1,c=5,r=6,m=1;")
            .unwrap();
        assert!(delete < first);
        assert_eq!(sequence.matches("\x1b_Gm=1;").count(), 1);
        assert_eq!(sequence.matches("\x1b_Gm=0;").count(), 1);
        let payload: String = sequence
            .split("\x1b_G")
            .skip(2)
            .filter_map(|command| command.split_once(';'))
            .filter_map(|(_, data)| data.split('\x1b').next())
            .collect();
        assert_eq!(STANDARD.decode(payload).unwrap(), png);
    }
}
