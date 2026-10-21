//! Pictures in the diff pane. GitHub has no diff for an image, so laugh
//! fetches the file from both sides of the PR and draws them in the
//! terminal with half blocks (`▀`: a cell is two pixels, one above the
//! other), before beside after. `c` dims everything that stayed the same,
//! so what changed stands out.

use std::path::PathBuf;

use image::imageops::{self, FilterType};
use image::{GrayImage, Luma, Rgba, RgbaImage};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico"];

/// Pictures are kept no bigger than this: a pane never shows more, and a
/// PR full of screenshots shouldn't fill the memory.
const KEEP: u32 = 320;

/// How far a channel may move before a pixel counts as changed, so lossy
/// re-encoding alone doesn't light everything up.
const NOISE: u8 = 24;

pub fn is_image(path: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// One side of the change.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    small: RgbaImage,
}

pub struct ImageDiff {
    pub before: Option<Picture>,
    pub after: Option<Picture>,
    /// Where the two differ, at the kept size; only when they're the same
    /// size.
    changed: Option<GrayImage>,
    /// The share of pixels that differ, 0.0–1.0.
    pub changed_share: Option<f64>,
    /// Both sides saved full size, for an image viewer.
    pub files: Vec<PathBuf>,
}

impl ImageDiff {
    pub fn comparable(&self) -> bool {
        self.changed.is_some()
    }
}

fn decode(bytes: &[u8]) -> Result<(RgbaImage, usize), String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("can't read the image: {e}"))?;
    Ok((img.to_rgba8(), bytes.len()))
}

/// The size `width × height` takes when fitted inside `max × max`.
fn fit(width: u32, height: u32, max: u32) -> (u32, u32) {
    if width <= max && height <= max {
        return (width.max(1), height.max(1));
    }
    let scale = f64::from(max) / f64::from(width.max(height));
    (
        ((f64::from(width) * scale).round() as u32).max(1),
        ((f64::from(height) * scale).round() as u32).max(1),
    )
}

fn shrink(img: &RgbaImage) -> RgbaImage {
    let (w, h) = fit(img.width(), img.height(), KEEP);
    if (w, h) == img.dimensions() {
        img.clone()
    } else {
        imageops::resize(img, w, h, FilterType::Triangle)
    }
}

/// Decodes both sides (either may be missing: an added or removed file)
/// and works out where they differ.
pub fn compare(before: Option<&[u8]>, after: Option<&[u8]>) -> Result<ImageDiff, String> {
    let before = before.map(decode).transpose()?;
    let after = after.map(decode).transpose()?;
    let (mut changed, mut changed_share) = (None, None);
    if let (Some((a, _)), Some((b, _))) = (&before, &after)
        && a.dimensions() == b.dimensions()
    {
        let mut mask = GrayImage::new(a.width(), a.height());
        let mut count = 0u64;
        for (x, y, pa) in a.enumerate_pixels() {
            let pb = b.get_pixel(x, y);
            if pa.0.iter().zip(pb.0).any(|(&p, q)| p.abs_diff(q) > NOISE) {
                mask.put_pixel(x, y, Luma([255]));
                count += 1;
            }
        }
        changed_share = Some(count as f64 / f64::from((a.width() * a.height()).max(1)));
        let (w, h) = fit(a.width(), a.height(), KEEP);
        // Shrinking blurs a lone changed pixel into its neighbours; any
        // trace of one keeps the spot lit, so small changes don't vanish.
        let small = imageops::resize(&mask, w, h, FilterType::Triangle);
        changed = Some(GrayImage::from_fn(w, h, |x, y| {
            Luma([if small.get_pixel(x, y).0[0] > 0 {
                255
            } else {
                0
            }])
        }));
    }
    let picture = |side: Option<(RgbaImage, usize)>| {
        side.map(|(img, bytes)| Picture {
            width: img.width(),
            height: img.height(),
            bytes,
            small: shrink(&img),
        })
    };
    Ok(ImageDiff {
        before: picture(before),
        after: picture(after),
        changed,
        changed_share,
        files: Vec::new(),
    })
}

/// Saves one side's bytes where an image viewer can open them, as
/// `<tmp>/laugh/<owner>-<repo>-<pr>/<before|after>-<name>`.
pub fn save(bytes: &[u8], pr: &str, side: &str, path: &str) -> Option<PathBuf> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let dir = std::env::temp_dir().join("laugh").join(pr);
    std::fs::create_dir_all(&dir).ok()?;
    let file = dir.join(format!("{side}-{name}"));
    std::fs::write(&file, bytes).ok()?;
    Some(file)
}

/// How much one side gets scaled to fit `cols × rows` cells.
fn scale(picture: &Picture, cols: u16, rows: u16) -> f64 {
    let (w, h) = (
        f64::from(picture.small.width()),
        f64::from(picture.small.height()),
    );
    (f64::from(cols) / w).min(f64::from(rows) * 2.0 / h)
}

/// True when the two sides come out bigger side by side in `cols × rows`
/// than one above the other.
pub fn side_by_side(diff: &ImageDiff, cols: u16, rows: u16) -> bool {
    let smallest = |cols: u16, rows: u16| {
        [&diff.before, &diff.after]
            .into_iter()
            .flatten()
            .map(|p| scale(p, cols, rows))
            .fold(f64::INFINITY, f64::min)
    };
    cols >= 2 * 16 + 3
        && smallest((cols - 3) / 2, rows) >= smallest(cols, rows.saturating_sub(1) / 2)
}

/// A transparent pixel shows a dark checkerboard through it.
fn backdrop(x: u32, y: u32) -> [u8; 3] {
    if (x / 4 + y / 4).is_multiple_of(2) {
        [0x2a; 3]
    } else {
        [0x3a; 3]
    }
}

fn pixel_color(p: Rgba<u8>, x: u32, y: u32, dim: bool) -> Color {
    let [r, g, b, a] = p.0;
    let under = backdrop(x, y);
    let mix = |c: u8, u: u8| {
        ((u16::from(c) * u16::from(a) + u16::from(u) * (255 - u16::from(a))) / 255) as u8
    };
    let (mut r, mut g, mut b) = (mix(r, under[0]), mix(g, under[1]), mix(b, under[2]));
    if dim {
        (r, g, b) = (r / 4, g / 4, b / 4);
    }
    Color::Rgb(r, g, b)
}

/// The picture fitted into `cols × rows` cells and centred, every line
/// exactly `cols` wide. With `changes`, what stayed the same is dimmed.
pub fn draw(
    diff: &ImageDiff,
    after: bool,
    changes: bool,
    cols: u16,
    rows: u16,
) -> Vec<Line<'static>> {
    let blank = || Line::from(" ".repeat(cols as usize));
    let picture = if after { &diff.after } else { &diff.before };
    let Some(picture) = picture else {
        return vec![blank(); rows as usize];
    };
    let src = &picture.small;
    if cols == 0 || rows == 0 {
        return Vec::new();
    }
    // Pixels come out square: a cell is about twice as tall as it is
    // wide, and holds two pixels stacked.
    let (iw, ih) = (f64::from(src.width()), f64::from(src.height()));
    let scale = (f64::from(cols) / iw).min(f64::from(rows) * 2.0 / ih);
    let w = ((iw * scale).round() as u32).clamp(1, u32::from(cols));
    let h = ((ih * scale).round() as u32).clamp(1, u32::from(rows) * 2);
    let filter = if scale > 1.0 {
        FilterType::Nearest
    } else {
        FilterType::Triangle
    };
    let img = imageops::resize(src, w, h, filter);
    let mask = diff
        .changed
        .as_ref()
        .filter(|_| changes)
        .map(|m| imageops::resize(m, w, h, FilterType::Nearest));
    let at = |x: u32, y: u32| {
        if y >= h {
            return Color::Reset;
        }
        let dim = mask.as_ref().is_some_and(|m| m.get_pixel(x, y).0[0] == 0);
        pixel_color(*img.get_pixel(x, y), x, y, dim)
    };
    let left = (u32::from(cols) - w) / 2;
    let right = u32::from(cols) - w - left;
    let used = h.div_ceil(2) as u16;
    let top = (rows - used) / 2;
    let mut lines = vec![blank(); top as usize];
    for row in 0..u32::from(used) {
        let mut spans = vec![Span::raw(" ".repeat(left as usize))];
        for x in 0..w {
            spans.push(Span::styled(
                "▀",
                Style::default().fg(at(x, row * 2)).bg(at(x, row * 2 + 1)),
            ));
        }
        spans.push(Span::raw(" ".repeat(right as usize)));
        lines.push(Line::from(spans));
    }
    lines.resize(rows as usize, blank());
    lines
}

/// `84 KB`, `1.2 MB`.
pub fn size(bytes: usize) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.0} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat};
    use std::io::Cursor;

    fn png(img: RgbaImage) -> Vec<u8> {
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    #[test]
    fn images_are_told_by_extension() {
        assert!(is_image("static/img/a.webp"));
        assert!(is_image("Logo.PNG"));
        assert!(!is_image("icon.svg"));
        assert!(!is_image("README"));
    }

    #[test]
    fn a_changed_corner_is_found_and_measured() {
        let before = RgbaImage::from_pixel(10, 10, Rgba([255, 255, 255, 255]));
        let mut after = before.clone();
        for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            after.put_pixel(x, y, Rgba([255, 0, 0, 255]));
        }
        // A whisker of difference is noise, not change.
        after.put_pixel(9, 9, Rgba([250, 250, 250, 255]));
        let diff = compare(Some(&png(before)), Some(&png(after))).unwrap();
        assert!(diff.comparable());
        assert_eq!(diff.changed_share, Some(0.04));
        let mask = diff.changed.as_ref().unwrap();
        assert_eq!(mask.get_pixel(0, 0).0[0], 255);
        assert_eq!(mask.get_pixel(9, 9).0[0], 0);
    }

    #[test]
    fn an_added_image_has_no_before_and_nothing_to_compare() {
        let after = RgbaImage::from_pixel(4, 2, Rgba([0, 0, 255, 255]));
        let diff = compare(None, Some(&png(after))).unwrap();
        assert!(diff.before.is_none());
        assert!(!diff.comparable());
        let after = diff.after.as_ref().unwrap();
        assert_eq!((after.width, after.height), (4, 2));
    }

    #[test]
    fn a_picture_fills_its_box_without_stretching() {
        let wide = RgbaImage::from_pixel(400, 100, Rgba([0, 255, 0, 255]));
        let diff = compare(None, Some(&png(wide))).unwrap();
        // 40 columns wide → 10 pixels tall → 5 rows, centred in 11.
        let lines = draw(&diff, true, false, 40, 11);
        assert_eq!(lines.len(), 11);
        assert!(lines.iter().all(|l| l.width() == 40));
        let drawn = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content == "▀"))
            .count();
        assert_eq!(drawn, 5);
        assert!(!lines[2].spans.iter().any(|s| s.content == "▀"));
        assert!(lines[3].spans.iter().any(|s| s.content == "▀"));
    }

    #[test]
    fn changes_only_dims_what_stayed_the_same() {
        let before = RgbaImage::from_pixel(2, 2, Rgba([200, 200, 200, 255]));
        let mut after = before.clone();
        after.put_pixel(0, 0, Rgba([200, 0, 0, 255]));
        let diff = compare(Some(&png(before)), Some(&png(after))).unwrap();
        let cell = |changes: bool| {
            let lines = draw(&diff, true, changes, 2, 1);
            let spans = &lines[0].spans;
            (spans[1].style.fg, spans[2].style.fg)
        };
        // The changed pixel keeps its colour; its unchanged neighbour fades.
        assert_eq!(
            cell(false),
            (Some(Color::Rgb(200, 0, 0)), Some(Color::Rgb(200, 200, 200)))
        );
        assert_eq!(
            cell(true),
            (Some(Color::Rgb(200, 0, 0)), Some(Color::Rgb(50, 50, 50)))
        );
    }

    #[test]
    fn big_pictures_are_kept_small() {
        let big = RgbaImage::from_pixel(1600, 900, Rgba([0, 0, 0, 255]));
        let diff = compare(Some(&png(big.clone())), Some(&png(big))).unwrap();
        let after = diff.after.as_ref().unwrap();
        assert_eq!((after.width, after.height), (1600, 900));
        assert_eq!(after.small.dimensions(), (320, 180));
        assert_eq!(diff.changed_share, Some(0.0));
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(size(512), "512 B");
        assert_eq!(size(86_000), "84 KB");
        assert_eq!(size(1_300_000), "1.2 MB");
    }
}
