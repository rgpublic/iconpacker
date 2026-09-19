//! Converting raw SVG-space geometry (`svg::RawOutline`) into glyph outline
//! data shaped the way a TrueType `glyf` table wants it: quadratic Bezier
//! curves only, integer coordinates, y-axis flipped (SVG is y-down, fonts
//! are y-up with the baseline at y=0).

use crate::svg::{Pt, RawOutline, Seg};
use kurbo::{CubicBez, Point};

/// One point of a glyph contour. `on_curve` points sit exactly on the
/// outline; the rest are quadratic control points that pull the curve
/// toward them but aren't touched by it (standard TrueType convention).
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub struct GlyphPoint {
    pub x: i32,
    pub y: i32,
    pub on_curve: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GlyphOutline {
    pub contours: Vec<Vec<GlyphPoint>>,
    pub advance_width: i32,
}

impl GlyphOutline {
    pub fn point_count(&self) -> usize {
        self.contours.iter().map(|c| c.len()).sum()
    }

    pub fn on_curve_count(&self) -> usize {
        self.contours
            .iter()
            .flat_map(|c| c.iter())
            .filter(|p| p.on_curve)
            .count()
    }
}

/// Turns raw SVG geometry into a glyph outline.
///
/// `upm` is the font's units-per-em (1000 and 2048 are the common choices).
/// The SVG is scaled so its height maps to the full em, with the SVG's
/// bottom edge landing on the baseline (y=0 in font space) - the usual
/// convention for icon fonts, same as fontforge's auto-sized import.
///
/// `accuracy` is the max allowed deviation (in font units) when refitting
/// each cubic Bezier as one or more quadratics; 1.0 is a reasonable default.
pub fn build_glyph_outline(raw: &RawOutline, upm: u16, accuracy: f64) -> GlyphOutline {
    let scale = if raw.height > 0.0 {
        upm as f64 / raw.height as f64
    } else {
        1.0
    };

    let to_font = |p: Pt| -> Point { Point::new(p.x as f64 * scale, upm as f64 - p.y as f64 * scale) };

    let mut contours: Vec<Vec<GlyphPoint>> = Vec::new();
    let mut current: Vec<GlyphPoint> = Vec::new();
    let mut cursor = Point::ZERO;

    let push_point = |points: &mut Vec<GlyphPoint>, p: Point, on_curve: bool| {
        points.push(GlyphPoint {
            x: p.x.round() as i32,
            y: p.y.round() as i32,
            on_curve,
        });
    };

    for seg in &raw.segments {
        match seg {
            Seg::MoveTo(p) => {
                if !current.is_empty() {
                    contours.push(std::mem::take(&mut current));
                }
                cursor = to_font(*p);
                push_point(&mut current, cursor, true);
            }
            Seg::LineTo(p) => {
                cursor = to_font(*p);
                push_point(&mut current, cursor, true);
            }
            Seg::QuadTo(c, p) => {
                let fc = to_font(*c);
                cursor = to_font(*p);
                push_point(&mut current, fc, false);
                push_point(&mut current, cursor, true);
            }
            Seg::CubicTo(c1, c2, p) => {
                let fc1 = to_font(*c1);
                let fc2 = to_font(*c2);
                let fp = to_font(*p);
                let cubic = CubicBez::new(cursor, fc1, fc2, fp);
                for (_t0, _t1, quad) in cubic.to_quads(accuracy) {
                    push_point(&mut current, quad.p1, false);
                    push_point(&mut current, quad.p2, true);
                }
                cursor = fp;
            }
            Seg::Close => {
                // TrueType contours implicitly close back to their first
                // point, so there's nothing to append here.
            }
        }
    }

    if !current.is_empty() {
        contours.push(current);
    }

    GlyphOutline {
        contours,
        advance_width: (raw.width as f64 * scale).round() as i32,
    }
}
