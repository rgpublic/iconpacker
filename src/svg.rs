//! Parsing a single SVG file's geometry with `usvg`.
//!
//! `parse_svg_raw` gives you the flattened, absolute-transform-applied path
//! data as a plain `RawOutline` (no usvg/tiny_skia types leak out of this
//! module). `parse_svg_outline` is the older stats-only view used by the CLI
//! table, now just a summary computed over the same raw data.

use std::path::Path;
use usvg::tiny_skia_path::PathSegment;

/// A 2D point in SVG user-space (y-down, whatever units the viewBox uses).
#[derive(Clone, Copy, Debug)]
pub struct Pt {
    pub x: f32,
    pub y: f32,
}

/// One path command, already flattened out of usvg's shape/group/transform
/// tree into plain SVG-space coordinates.
#[derive(Clone, Debug)]
pub enum Seg {
    MoveTo(Pt),
    LineTo(Pt),
    QuadTo(Pt, Pt),
    CubicTo(Pt, Pt, Pt),
    Close,
}

/// The full flattened geometry of one SVG file: its resolved size (from
/// viewBox/width/height) plus every path command across every <path>,
/// <circle>, <rect>, etc. it contained, in drawing order.
pub struct RawOutline {
    pub width: f32,
    pub height: f32,
    pub segments: Vec<Seg>,
}

/// Walks a usvg tree, flattening every shape into `Seg`s with each path's
/// absolute transform already applied.
pub fn parse_svg_raw(svg_path: &Path) -> Result<RawOutline, String> {
    let text = std::fs::read_to_string(svg_path).map_err(|e| e.to_string())?;
    let tree = usvg::Tree::from_str(&text, &usvg::Options::default()).map_err(|e| e.to_string())?;

    let size = tree.size();
    let mut segments = Vec::new();
    for child in tree.root().children() {
        visit(child, &mut segments);
    }

    Ok(RawOutline {
        width: size.width(),
        height: size.height(),
        segments,
    })
}

fn visit(node: &usvg::Node, segments: &mut Vec<Seg>) {
    match node {
        usvg::Node::Group(group) => {
            for child in group.children() {
                visit(child, segments);
            }
        }
        usvg::Node::Path(path) => {
            let transform = path.abs_transform();
            let map = |mut p: usvg::tiny_skia_path::Point| -> Pt {
                transform.map_point(&mut p);
                Pt { x: p.x, y: p.y }
            };
            for segment in path.data().segments() {
                segments.push(match segment {
                    PathSegment::MoveTo(p) => Seg::MoveTo(map(p)),
                    PathSegment::LineTo(p) => Seg::LineTo(map(p)),
                    PathSegment::QuadTo(c, p) => Seg::QuadTo(map(c), map(p)),
                    PathSegment::CubicTo(c1, c2, p) => Seg::CubicTo(map(c1), map(c2), map(p)),
                    PathSegment::Close => Seg::Close,
                });
            }
        }
        // Images and text aren't expected in icon SVGs; skip rather than error.
        usvg::Node::Image(_) | usvg::Node::Text(_) => {}
    }
}

#[derive(serde::Serialize, Debug, Default)]
pub struct OutlineSummary {
    /// SVG's resolved viewBox/width-height size
    pub svg_width: f32,
    pub svg_height: f32,
    /// number of separate subpaths (contours), e.g. "O" has 2: outer + inner
    pub contours: usize,
    pub points: usize,
    pub line_segments: usize,
    pub quad_segments: usize,
    pub cubic_segments: usize,
    pub bbox_min_x: f32,
    pub bbox_min_y: f32,
    pub bbox_max_x: f32,
    pub bbox_max_y: f32,
}

/// Summarizes raw SVG-space geometry for the human/--json table (stats only,
/// no font-unit conversion).
pub fn summarize(raw: &RawOutline) -> OutlineSummary {
    let mut s = OutlineSummary {
        svg_width: raw.width,
        svg_height: raw.height,
        bbox_min_x: f32::INFINITY,
        bbox_min_y: f32::INFINITY,
        bbox_max_x: f32::NEG_INFINITY,
        bbox_max_y: f32::NEG_INFINITY,
        ..Default::default()
    };

    let track = |p: Pt, s: &mut OutlineSummary| {
        s.bbox_min_x = s.bbox_min_x.min(p.x);
        s.bbox_min_y = s.bbox_min_y.min(p.y);
        s.bbox_max_x = s.bbox_max_x.max(p.x);
        s.bbox_max_y = s.bbox_max_y.max(p.y);
    };

    for seg in &raw.segments {
        match seg {
            Seg::MoveTo(p) => {
                s.contours += 1;
                s.points += 1;
                track(*p, &mut s);
            }
            Seg::LineTo(p) => {
                s.line_segments += 1;
                s.points += 1;
                track(*p, &mut s);
            }
            Seg::QuadTo(c, p) => {
                s.quad_segments += 1;
                s.points += 2;
                track(*c, &mut s);
                track(*p, &mut s);
            }
            Seg::CubicTo(c1, c2, p) => {
                s.cubic_segments += 1;
                s.points += 3;
                track(*c1, &mut s);
                track(*c2, &mut s);
                track(*p, &mut s);
            }
            Seg::Close => {}
        }
    }

    s
}

/// Convenience wrapper: parse + summarize in one call. Not currently used by
/// the CLI (which needs the raw geometry too, for `outline::build_glyph_outline`)
/// but kept as the simple entry point for anything that only wants stats.
#[allow(dead_code)]
pub fn parse_svg_outline(svg_path: &Path) -> Result<OutlineSummary, String> {
    parse_svg_raw(svg_path).map(|raw| summarize(&raw))
}
