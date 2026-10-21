//! Pictures in the diff pane. GitHub has no diff for an image, so laugh
//! fetches the file from both sides of the PR and shows them before beside
//! after: drawn by the terminal itself where it can (see [`graphics`]),
//! otherwise with half blocks (`▀`: a cell is two pixels, one above the
//! other). `c` dims everything that stayed the same,
//! so what changed stands out.

use std::cell::RefCell;
use std::io::Cursor;
use std::path::PathBuf;

use image::imageops::{self, FilterType};
use image::{DynamicImage, GrayImage, ImageFormat, Luma, Rgba, RgbaImage};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "ico"];

/// Pictures are kept no bigger than this: a pane never shows more, and a
/// PR full of screenshots shouldn't fill the memory.
const KEEP: u32 = 320;

/// The most kept for a terminal that draws pictures itself: plenty for a
/// pane. What it's sent is shrunk further to the pixels it's shown at.
const SHARP: u32 = 1600;

/// Sent widths go up in steps of this, so dragging a border doesn't send a
/// new picture for every column.
const STEP: u32 = 128;

use crate::graphics;

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
    /// For a terminal that draws pictures itself.
    sharp: Option<Sharp>,
}

struct Sharp {
    plain: RgbaImage,
    /// With what stayed the same dimmed, when there's something to compare.
    changes: Option<RgbaImage>,
    /// What the terminal was last sent: whether it was `changes`, its
    /// size, and the picture. herdr hands pictures on as raw pixels, so
    /// sending no more than are shown keeps them quick.
    sent: RefCell<Option<(SentKey, graphics::Image)>>,
}

/// Dimmed or not, width, height.
type SentKey = (bool, u32, u32);

impl Sharp {
    /// The kept picture for `changes`.
    fn pixels(&self, changes: bool) -> &RgbaImage {
        match &self.changes {
            Some(dimmed) if changes => dimmed,
            _ => &self.plain,
        }
    }

    /// The picture to send for `changes`, about `width` pixels wide.
    fn image(&self, changes: bool, width: Option<u32>) -> graphics::Image {
        let source = self.pixels(changes);
        let (w, h) = match width {
            Some(width) if width.div_ceil(STEP) * STEP < source.width() => {
                let w = width.div_ceil(STEP) * STEP;
                let h = (u64::from(source.height()) * u64::from(w) / u64::from(source.width()))
                    .max(1) as u32;
                (w, h)
            }
            _ => source.dimensions(),
        };
        let key = (changes && self.changes.is_some(), w, h);
        let mut sent = self.sent.borrow_mut();
        if let Some((k, image)) = sent.as_ref()
            && *k == key
        {
            return image.clone();
        }
        let image = if (w, h) == source.dimensions() {
            png(source.clone())
        } else {
            png(imageops::resize(source, w, h, FilterType::Triangle))
        };
        if let Some((_, old)) = sent.replace((key, image.clone())) {
            graphics::release(&old);
        }
        image
    }
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
    /// Both sides in one picture, as last sent: see [`together`].
    together: RefCell<Option<(TogetherKey, graphics::Image)>>,
}

/// Dimmed or not, where each side goes, and a cell's size in pixels.
type TogetherKey = (bool, Rect, Rect, (u32, u32));

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

fn shrink(img: &RgbaImage, max: u32) -> RgbaImage {
    let (w, h) = fit(img.width(), img.height(), max);
    if (w, h) == img.dimensions() {
        img.clone()
    } else {
        imageops::resize(img, w, h, FilterType::Triangle)
    }
}

/// A full-size change mask at `max`: shrinking blurs a lone changed pixel
/// into its neighbours, and any trace of one keeps the spot lit, so small
/// changes don't vanish.
fn shrink_mask(mask: &GrayImage, max: u32) -> GrayImage {
    let (w, h) = fit(mask.width(), mask.height(), max);
    let small = imageops::resize(mask, w, h, FilterType::Triangle);
    GrayImage::from_fn(w, h, |x, y| {
        Luma([if small.get_pixel(x, y).0[0] > 0 {
            255
        } else {
            0
        }])
    })
}

fn png(img: RgbaImage) -> graphics::Image {
    let mut out = Cursor::new(Vec::new());
    // Encoding pixels already in memory can't fail.
    let _ = DynamicImage::ImageRgba8(img).write_to(&mut out, ImageFormat::Png);
    graphics::Image::new(out.into_inner())
}

/// The picture to keep for the terminal, and with `mask` the same with what
/// didn't change dimmed.
fn sharpen(img: &RgbaImage, mask: Option<&GrayImage>) -> Sharp {
    let big = shrink(img, SHARP);
    let changes = mask.map(|mask| {
        let mask = shrink_mask(mask, SHARP);
        let mut dimmed = big.clone();
        for (x, y, p) in dimmed.enumerate_pixels_mut() {
            if mask.get_pixel(x, y).0[0] == 0 {
                let [r, g, b, a] = p.0;
                *p = Rgba([r / 4, g / 4, b / 4, a]);
            }
        }
        dimmed
    });
    Sharp {
        plain: big,
        changes,
        sent: RefCell::new(None),
    }
}

/// Decodes both sides (either may be missing: an added or removed file)
/// and works out where they differ. With `sharp`, also makes what a
/// terminal that draws pictures itself is sent.
pub fn compare(
    before: Option<&[u8]>,
    after: Option<&[u8]>,
    sharp: bool,
) -> Result<ImageDiff, String> {
    let before = before.map(decode).transpose()?;
    let after = after.map(decode).transpose()?;
    let (mut changed, mut changed_share, mut full_mask) = (None, None, None);
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
        changed = Some(shrink_mask(&mask, KEEP));
        full_mask = Some(mask);
    }
    let picture = |side: Option<(RgbaImage, usize)>| {
        side.map(|(img, bytes)| Picture {
            width: img.width(),
            height: img.height(),
            bytes,
            small: shrink(&img, KEEP),
            sharp: sharp.then(|| sharpen(&img, full_mask.as_ref())),
        })
    };
    Ok(ImageDiff {
        before: picture(before),
        after: picture(after),
        changed,
        changed_share,
        files: Vec::new(),
        together: RefCell::new(None),
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

/// How much one side gets scaled to fit `cols × rows` cells, `ratio`
/// times taller than wide, counting a cell's width as one.
fn scale(picture: &Picture, cols: u16, rows: u16, ratio: f64) -> f64 {
    let (w, h) = (f64::from(picture.width), f64::from(picture.height));
    (f64::from(cols) / w).min(f64::from(rows) * ratio / h)
}

/// True when the two sides come out bigger side by side in `cols × rows`
/// than one above the other.
pub fn side_by_side(diff: &ImageDiff, cols: u16, rows: u16, ratio: f64) -> bool {
    let smallest = |cols: u16, rows: u16| {
        [&diff.before, &diff.after]
            .into_iter()
            .flatten()
            .map(|p| scale(p, cols, rows, ratio))
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

/// For a terminal that draws pictures itself: the cells one side covers
/// within `cols × rows`, centred and not stretched.
pub fn placement(diff: &ImageDiff, after: bool, cols: u16, rows: u16, ratio: f64) -> Option<Rect> {
    let picture = if after { &diff.after } else { &diff.before };
    let picture = picture.as_ref().filter(|p| p.sharp.is_some())?;
    let s = scale(picture, cols, rows, ratio);
    let w = ((f64::from(picture.width) * s).round() as u16).clamp(1, cols.max(1));
    let h = ((f64::from(picture.height) * s / ratio).round() as u16).clamp(1, rows.max(1));
    Some(Rect::new((cols - w) / 2, (rows - h) / 2, w, h))
}

/// One side's picture to send, shrunk to about `width` pixels when known.
pub fn side(
    diff: &ImageDiff,
    after: bool,
    changes: bool,
    width: Option<u32>,
) -> Option<graphics::Image> {
    let picture = if after { &diff.after } else { &diff.before };
    Some(picture.as_ref()?.sharp.as_ref()?.image(changes, width))
}

/// Both sides as one picture over `before` and `after` (cells, as
/// [`placement`] gives them) with the space around them left clear. herdr
/// hands pictures on to its terminal one at a time, each waiting for the
/// last, so one picture shows both sides at once and sooner.
pub fn together(
    diff: &ImageDiff,
    changes: bool,
    before: Rect,
    after: Rect,
    cell: (u32, u32),
) -> Option<(Rect, graphics::Image)> {
    let sides = [
        (diff.before.as_ref()?.sharp.as_ref()?, before),
        (diff.after.as_ref()?.sharp.as_ref()?, after),
    ];
    let area = before.union(after);
    let key = (changes, before, after, cell);
    let mut sent = diff.together.borrow_mut();
    if let Some((k, image)) = sent.as_ref()
        && *k == key
    {
        return Some((area, image.clone()));
    }
    let (cw, ch) = cell;
    let mut canvas = RgbaImage::new(u32::from(area.width) * cw, u32::from(area.height) * ch);
    for (sharp, at) in sides {
        let side = imageops::resize(
            sharp.pixels(changes),
            u32::from(at.width) * cw,
            u32::from(at.height) * ch,
            FilterType::Triangle,
        );
        imageops::replace(
            &mut canvas,
            &side,
            i64::from((at.x - area.x) as u32 * cw),
            i64::from((at.y - area.y) as u32 * ch),
        );
    }
    let image = png(canvas);
    if let Some((_, old)) = sent.replace((key, image.clone())) {
        graphics::release(&old);
    }
    Some((area, image))
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
        let diff = compare(Some(&png(before)), Some(&png(after)), false).unwrap();
        assert!(diff.comparable());
        assert_eq!(diff.changed_share, Some(0.04));
        let mask = diff.changed.as_ref().unwrap();
        assert_eq!(mask.get_pixel(0, 0).0[0], 255);
        assert_eq!(mask.get_pixel(9, 9).0[0], 0);
    }

    #[test]
    fn an_added_image_has_no_before_and_nothing_to_compare() {
        let after = RgbaImage::from_pixel(4, 2, Rgba([0, 0, 255, 255]));
        let diff = compare(None, Some(&png(after)), false).unwrap();
        assert!(diff.before.is_none());
        assert!(!diff.comparable());
        let after = diff.after.as_ref().unwrap();
        assert_eq!((after.width, after.height), (4, 2));
    }

    #[test]
    fn a_picture_fills_its_box_without_stretching() {
        let wide = RgbaImage::from_pixel(400, 100, Rgba([0, 255, 0, 255]));
        let diff = compare(None, Some(&png(wide)), false).unwrap();
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
    fn the_terminal_is_sent_about_the_pixels_shown() {
        let img = RgbaImage::from_pixel(1000, 500, Rgba([0, 0, 255, 255]));
        let diff = compare(None, Some(&png(img)), true).unwrap();
        assert_eq!(
            placement(&diff, true, 20, 20, 2.0),
            Some(Rect::new(0, 7, 20, 5))
        );
        let sent = |width| {
            let image = side(&diff, true, false, width).unwrap();
            image::load_from_memory(image.png())
                .unwrap()
                .to_rgba8()
                .dimensions()
        };
        // 200 pixels shown: up to the next step.
        assert_eq!(sent(Some(200)), (256, 128));
        // Unknown, or wider than kept: all of it.
        assert_eq!(sent(None), (1000, 500));
        assert_eq!(sent(Some(2000)), (1000, 500));
    }

    #[test]
    fn both_sides_go_as_one_picture_with_the_gap_clear() {
        let red = RgbaImage::from_pixel(8, 4, Rgba([255, 0, 0, 255]));
        let blue = RgbaImage::from_pixel(8, 4, Rgba([0, 0, 255, 255]));
        let diff = compare(Some(&png(red)), Some(&png(blue)), true).unwrap();
        // One above the other, a label row between them.
        let (before, after) = (Rect::new(2, 3, 4, 2), Rect::new(2, 6, 4, 2));
        let (area, image) = together(&diff, false, before, after, (2, 4)).unwrap();
        assert_eq!(area, Rect::new(2, 3, 4, 5));
        let one = image::load_from_memory(image.png()).unwrap().to_rgba8();
        assert_eq!(one.dimensions(), (8, 20));
        assert_eq!(one.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(one.get_pixel(0, 10).0[3], 0);
        assert_eq!(one.get_pixel(7, 19).0, [0, 0, 255, 255]);
    }

    #[test]
    fn changes_only_dims_what_stayed_the_same() {
        let before = RgbaImage::from_pixel(2, 2, Rgba([200, 200, 200, 255]));
        let mut after = before.clone();
        after.put_pixel(0, 0, Rgba([200, 0, 0, 255]));
        let diff = compare(Some(&png(before)), Some(&png(after)), false).unwrap();
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
        let diff = compare(Some(&png(big.clone())), Some(&png(big)), false).unwrap();
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
