//! Ring icon rendered as ARGB32 pixmaps (StatusNotifierItem `IconPixmap`).
//!
//! The ring shows a grey track and a coloured arc that starts at 12 o'clock
//! and runs clockwise for `pct` percent of the circle. Pure: no IO.

/// Sizes offered to the tray host; it picks the closest one.
pub const SIZES: [u32; 6] = [16, 22, 24, 32, 48, 64];

/// Outer radius as a fraction of the icon size.
const OUTER: f64 = 0.47;
/// Ring thickness as a fraction of the icon size.
const THICKNESS: f64 = 0.20;
/// Track alpha (0..=1) under the arc.
const TRACK_ALPHA: f64 = 0.35;
/// Supersampling grid per pixel axis (anti-aliasing).
const SS: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

pub const GREEN: Rgb = Rgb(0x2e, 0xc2, 0x7e);
pub const AMBER: Rgb = Rgb(0xe5, 0xa5, 0x0a);
pub const RED: Rgb = Rgb(0xe0, 0x1b, 0x24);
pub const GREY: Rgb = Rgb(0x9a, 0x99, 0x96);
const TRACK: Rgb = Rgb(0x9a, 0x99, 0x96);

/// Threshold level of the 5h window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Crit,
    /// Stale or missing data.
    Unknown,
}

impl Level {
    /// Green below 75%, amber from 75%, red from 90%.
    pub fn from_pct(pct: f64) -> Self {
        if pct >= 90.0 {
            Level::Crit
        } else if pct >= 75.0 {
            Level::Warn
        } else {
            Level::Ok
        }
    }

    pub fn color(self) -> Rgb {
        match self {
            Level::Ok => GREEN,
            Level::Warn => AMBER,
            Level::Crit => RED,
            Level::Unknown => GREY,
        }
    }
}

/// What a sample point of the icon falls on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    Outside,
    Track,
    Arc,
}

/// Clockwise angle from 12 o'clock in turns (`0..1`), for screen
/// coordinates (y grows downwards) relative to the centre.
pub fn turns(dx: f64, dy: f64) -> f64 {
    let a = dx.atan2(-dy) / std::f64::consts::TAU;
    if a < 0.0 { a + 1.0 } else { a }
}

/// Classifies the point `(x, y)` (pixel units, origin at the top-left
/// corner of a `size`-pixel icon) for an arc filled to `frac` (`0..=1`).
pub fn hit(size: u32, frac: f64, x: f64, y: f64) -> Hit {
    let s = f64::from(size);
    let c = s / 2.0;
    let (dx, dy) = (x - c, y - c);
    let r = (dx * dx + dy * dy).sqrt();
    let outer = s * OUTER;
    let inner = outer - s * THICKNESS;
    if r > outer || r < inner {
        Hit::Outside
    } else if frac >= 1.0 || turns(dx, dy) < frac {
        Hit::Arc
    } else {
        Hit::Track
    }
}

/// `pct` clamped to `0..=1` turns; `None`/NaN draws no arc.
pub fn fraction(pct: Option<f64>) -> f64 {
    match pct {
        Some(p) if p.is_finite() => (p / 100.0).clamp(0.0, 1.0),
        _ => 0.0,
    }
}

/// One ARGB32 pixmap (network byte order: A, R, G, B per pixel).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pixmap {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

#[cfg(test)]
impl Pixmap {
    /// `[a, r, g, b]` at pixel `(x, y)`.
    pub fn argb(&self, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * self.width + x) * 4) as usize;
        [
            self.data[i],
            self.data[i + 1],
            self.data[i + 2],
            self.data[i + 3],
        ]
    }
}

/// Renders the ring at `size` pixels with the arc in `color`.
pub fn render(size: u32, pct: Option<f64>, color: Rgb) -> Pixmap {
    let frac = fraction(pct);
    let n = f64::from(SS * SS);
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut arc, mut track) = (0u32, 0u32);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = f64::from(px) + (f64::from(sx) + 0.5) / f64::from(SS);
                    let y = f64::from(py) + (f64::from(sy) + 0.5) / f64::from(SS);
                    match hit(size, frac, x, y) {
                        Hit::Arc => arc += 1,
                        Hit::Track => track += 1,
                        Hit::Outside => {}
                    }
                }
            }
            let a_arc = f64::from(arc) / n;
            let a_track = f64::from(track) / n * TRACK_ALPHA;
            // Arc and track samples are disjoint, so their alphas add.
            let a = a_arc + a_track;
            let mix = |c: u8, t: u8| {
                if a == 0.0 {
                    0
                } else {
                    ((f64::from(c) * a_arc + f64::from(t) * a_track) / a).round() as u8
                }
            };
            data.extend_from_slice(&[
                (a * 255.0).round() as u8,
                mix(color.0, TRACK.0),
                mix(color.1, TRACK.1),
                mix(color.2, TRACK.2),
            ]);
        }
    }
    Pixmap {
        width: size,
        height: size,
        data,
    }
}

/// The ring at every size in [`SIZES`].
pub fn render_all(pct: Option<f64>, level: Level) -> Vec<Pixmap> {
    SIZES
        .iter()
        .map(|&s| render(s, pct, level.color()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Centre of the ring's band, at `t` turns clockwise from 12 o'clock.
    fn band_point(size: u32, t: f64) -> (f64, f64) {
        let s = f64::from(size);
        let r = s * OUTER - s * THICKNESS / 2.0;
        let a = t * std::f64::consts::TAU;
        (s / 2.0 + r * a.sin(), s / 2.0 - r * a.cos())
    }

    #[test]
    fn levels_by_threshold() {
        assert_eq!(Level::from_pct(0.0), Level::Ok);
        assert_eq!(Level::from_pct(74.9), Level::Ok);
        assert_eq!(Level::from_pct(75.0), Level::Warn);
        assert_eq!(Level::from_pct(89.9), Level::Warn);
        assert_eq!(Level::from_pct(90.0), Level::Crit);
        assert_eq!(Level::from_pct(100.0), Level::Crit);
        assert_eq!(Level::Unknown.color(), GREY);
    }

    #[test]
    fn turns_run_clockwise_from_top() {
        let eps = 1e-9;
        assert!((turns(0.0, -1.0) - 0.0).abs() < eps); // 12 o'clock
        assert!((turns(1.0, 0.0) - 0.25).abs() < eps); // 3 o'clock
        assert!((turns(0.0, 1.0) - 0.5).abs() < eps); // 6 o'clock
        assert!((turns(-1.0, 0.0) - 0.75).abs() < eps); // 9 o'clock
    }

    #[test]
    fn fraction_clamps() {
        assert_eq!(fraction(None), 0.0);
        assert_eq!(fraction(Some(f64::NAN)), 0.0);
        assert_eq!(fraction(Some(-5.0)), 0.0);
        assert_eq!(fraction(Some(62.0)), 0.62);
        assert_eq!(fraction(Some(140.0)), 1.0);
    }

    #[test]
    fn hit_regions() {
        let size = 64;
        // Centre and corners are outside the band.
        assert_eq!(hit(size, 0.5, 32.0, 32.0), Hit::Outside);
        assert_eq!(hit(size, 0.5, 0.0, 0.0), Hit::Outside);
        // 62%: 1/4/7 o'clock filled, 8/10 o'clock track.
        for t in [0.02, 0.25, 0.45, 0.6] {
            let (x, y) = band_point(size, t);
            assert_eq!(hit(size, 0.62, x, y), Hit::Arc, "t={t}");
        }
        for t in [0.64, 0.8, 0.98] {
            let (x, y) = band_point(size, t);
            assert_eq!(hit(size, 0.62, x, y), Hit::Track, "t={t}");
        }
        // 0% has no arc, 100% is a full ring.
        let (x, y) = band_point(size, 0.5);
        assert_eq!(hit(size, 0.0, x, y), Hit::Track);
        let (x, y) = band_point(size, 0.999);
        assert_eq!(hit(size, 1.0, x, y), Hit::Arc);
    }

    #[test]
    fn pixmap_layout_and_colours() {
        for &size in &SIZES {
            let p = render(size, Some(62.0), GREEN);
            assert_eq!((p.width, p.height), (size, size));
            assert_eq!(p.data.len(), (size * size * 4) as usize);
            assert_eq!(p.argb(0, 0)[0], 0, "corner transparent at {size}");
            assert_eq!(p.argb(size / 2, size / 2)[0], 0, "centre transparent");
        }
        let size = 64;
        let p = render(size, Some(62.0), RED);
        let px = |t| {
            let (x, y) = band_point(size, t);
            p.argb(x as u32, y as u32)
        };
        // Inside the arc: opaque arc colour.
        assert_eq!(px(0.25), [255, RED.0, RED.1, RED.2]);
        // On the track: translucent grey.
        let [a, r, g, b] = px(0.8);
        assert_eq!(a, (TRACK_ALPHA * 255.0).round() as u8);
        assert_eq!(Rgb(r, g, b), TRACK);
    }

    #[test]
    fn arc_coverage_grows_with_pct() {
        let opaque = |pct: Option<f64>| {
            render(32, pct, GREEN)
                .data
                .chunks(4)
                .filter(|p| p[0] == 255)
                .count()
        };
        assert_eq!(opaque(None), 0);
        assert_eq!(opaque(Some(0.0)), 0);
        let (q, h, f) = (opaque(Some(25.0)), opaque(Some(50.0)), opaque(Some(100.0)));
        assert!(0 < q && q < h && h < f, "{q} {h} {f}");
        // Roughly proportional (anti-aliased edges aside).
        let ratio = h as f64 / f as f64;
        assert!((0.4..0.6).contains(&ratio), "{ratio}");
    }

    #[test]
    fn render_all_offers_every_size() {
        let all = render_all(Some(50.0), Level::Warn);
        let sizes: Vec<u32> = all.iter().map(|p| p.width).collect();
        assert_eq!(sizes, SIZES);
    }
}
