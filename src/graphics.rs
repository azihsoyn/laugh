//! Pictures drawn by the terminal itself, at its own resolution, through
//! the kitty graphics protocol — kitty, Ghostty, WezTerm, and herdr (which
//! passes them on to the terminal it runs in). Elsewhere laugh falls back
//! to half blocks; `LAUGH_GRAPHICS=kitty` or `=blocks` picks one by hand.
//!
//! Drawing a frame only collects what should be on screen ([`want`]);
//! [`flush`] then sends the terminal the difference. Images live outside
//! the text cells, so they're placed over a blank area the UI leaves for
//! them, and taken away again when it goes.

use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{self, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};

use ratatui::layout::Rect;

/// An image the terminal can be sent once and then placed by id.
#[derive(Clone)]
pub struct Image {
    id: u32,
    png: Arc<Vec<u8>>,
}

impl Image {
    pub fn new(png: Vec<u8>) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(1);
        Image {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            png: Arc::new(png),
        }
    }
}

/// True when pictures go to the terminal rather than into half blocks.
pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        match var("LAUGH_GRAPHICS").to_ascii_lowercase().as_str() {
            "kitty" => return true,
            "blocks" => return false,
            _ => {}
        }
        let program = var("TERM_PROGRAM").to_ascii_lowercase();
        // herdr can be started from another terminal and pass its
        // TERM_PROGRAM on to the panes, but always sets HERDR_ENV.
        var("HERDR_ENV") == "1"
            || ["herdr", "ghostty", "wezterm", "kitty"].contains(&program.as_str())
            || var("TERM").contains("kitty")
            || std::env::var_os("KITTY_WINDOW_ID").is_some()
    })
}

/// How many times taller than wide a cell is, from the terminal's pixel
/// size when it reports one.
pub fn cell_ratio() -> f64 {
    match crossterm::terminal::window_size() {
        Ok(s) if s.width > 0 && s.height > 0 && s.columns > 0 && s.rows > 0 => {
            (f64::from(s.height) / f64::from(s.rows)) / (f64::from(s.width) / f64::from(s.columns))
        }
        _ => 2.0,
    }
}

#[derive(Default)]
struct State {
    /// Asked for while drawing this frame.
    wanted: Vec<(Image, Rect)>,
    /// On screen now.
    placed: Vec<(u32, Rect)>,
    /// Already sent to the terminal.
    sent: HashSet<u32>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::default();
}

/// Puts `image` over `area` (in cells) for this frame.
pub fn want(image: &Image, area: Rect) {
    STATE.with_borrow_mut(|s| s.wanted.push((image.clone(), area)));
}

/// After a frame: places what was wanted and removes what wasn't. With
/// `show` false (a popup is open over them) nothing is placed.
pub fn flush(out: &mut impl Write, show: bool) -> io::Result<()> {
    STATE.with_borrow_mut(|s| {
        let mut wanted = std::mem::take(&mut s.wanted);
        if !show {
            wanted.clear();
        }
        let keys: Vec<(u32, Rect)> = wanted.iter().map(|(i, a)| (i.id, *a)).collect();
        if keys == s.placed {
            return Ok(());
        }
        if !s.placed.is_empty() {
            // Every placement goes; the images stay sent.
            write!(out, "\x1b_Ga=d,d=a,q=2\x1b\\")?;
        }
        for (image, area) in &wanted {
            if s.sent.insert(image.id) {
                send(out, image)?;
            }
            // Save the cursor, place without moving it, restore.
            write!(
                out,
                "\x1b7\x1b[{};{}H\x1b_Ga=p,i={},c={},r={},C=1,q=2\x1b\\\x1b8",
                area.y + 1,
                area.x + 1,
                image.id,
                area.width,
                area.height
            )?;
        }
        out.flush()?;
        s.placed = keys;
        Ok(())
    })
}

/// Sends an image's PNG in the protocol's 4096-byte chunks.
fn send(out: &mut impl Write, image: &Image) -> io::Result<()> {
    let data = base64(&image.png);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            write!(out, "\x1b_Ga=t,f=100,i={},q=2,m={more};", image.id)?;
        } else {
            write!(out, "\x1b_Gm={more};")?;
        }
        out.write_all(chunk)?;
        write!(out, "\x1b\\")?;
    }
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                ABC[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

/// The screen was redrawn from scratch (a resize, or the terminal handed
/// back): whatever was placed is cleared away and placed again.
pub fn forget_placements() {
    // Something that can't match makes the next flush start over.
    STATE.with_borrow_mut(|s| s.placed = vec![(0, Rect::default())]);
}

/// On the way out: removes every image laugh sent, freeing the terminal's
/// memory for them.
pub fn clear(out: &mut impl Write) -> io::Result<()> {
    STATE.with_borrow_mut(|s| {
        if s.sent.is_empty() {
            return Ok(());
        }
        for id in s.sent.drain() {
            write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
        }
        s.placed.clear();
        out.flush()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
    }

    #[test]
    fn an_image_is_sent_once_placed_and_taken_away() {
        let image = Image::new(vec![1, 2, 3]);
        let area = Rect::new(4, 2, 10, 5);
        let mut out = Vec::new();
        want(&image, area);
        flush(&mut out, true).unwrap();
        let first = String::from_utf8(std::mem::take(&mut out)).unwrap();
        assert!(first.contains(&format!("a=t,f=100,i={},q=2,m=0;AQID", image.id)));
        assert!(first.contains(&format!("\x1b[3;5H\x1b_Ga=p,i={},c=10,r=5,C=1", image.id)));

        // The same frame again sends nothing.
        want(&image, area);
        flush(&mut out, true).unwrap();
        assert!(out.is_empty());

        // Moved: the old placement goes, the image isn't sent again.
        want(&image, Rect::new(4, 3, 10, 5));
        flush(&mut out, true).unwrap();
        let moved = String::from_utf8(std::mem::take(&mut out)).unwrap();
        assert!(moved.starts_with("\x1b_Ga=d,d=a"));
        assert!(!moved.contains("a=t"));

        // A popup over it: taken away.
        want(&image, Rect::new(4, 3, 10, 5));
        flush(&mut out, false).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\x1b_Ga=d,d=a,q=2\x1b\\");
    }
}
