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

/// How one layer of a color glyph is painted: either "whatever CSS `color`
/// currently is" (opaque black in the source SVG is treated as this, so
/// monochrome parts of a color icon still follow the page's text color), or
/// a fixed RGBA color.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Paint {
    Foreground,
    Rgba(u8, u8, u8, u8),
}

/// A run of consecutive same-painted path data from one SVG. Layers keep the
/// SVG's drawing order so overlapping shapes stack the same way they do in
/// the source file.
pub struct RawLayer {
    pub paint: Paint,
    pub segments: Vec<Seg>,
}

/// An SVG split into painted layers instead of one merged outline.
pub struct RawLayers {
    pub width: f32,
    pub height: f32,
    pub layers: Vec<RawLayer>,
    /// Human-readable notes about things that were approximated (gradients).
    pub notes: Vec<String>,
}

impl RawLayers {
    /// Number of distinct paints across all layers.
    pub fn distinct_paints(&self) -> Vec<Paint> {
        let mut seen: Vec<Paint> = Vec::new();
        for layer in &self.layers {
            if !seen.contains(&layer.paint) {
                seen.push(layer.paint);
            }
        }
        seen
    }
}

/// Like `parse_svg_raw`, but keeps each path's fill. Paths without a fill
/// (stroke-only) are skipped, since an icon font can only paint filled
/// outlines. Gradients are approximated by their first stop's color, and a
/// note is recorded so the caller can warn.
pub fn parse_svg_layers(svg_path: &Path) -> Result<RawLayers, String> {
    let text = std::fs::read_to_string(svg_path).map_err(|e| e.to_string())?;
    let tree = usvg::Tree::from_str(&text, &usvg::Options::default()).map_err(|e| e.to_string())?;

    let size = tree.size();
    let mut layers: Vec<RawLayer> = Vec::new();
    let mut notes = Vec::new();
    for child in tree.root().children() {
        visit_layers(child, &mut layers, &mut notes);
    }
    notes.sort();
    notes.dedup();

    Ok(RawLayers {
        width: size.width(),
        height: size.height(),
        layers,
        notes,
    })
}

fn paint_of(fill: &usvg::Fill, notes: &mut Vec<String>) -> Paint {
    let color = match fill.paint() {
        usvg::Paint::Color(c) => *c,
        usvg::Paint::LinearGradient(_) | usvg::Paint::RadialGradient(_) => {
            notes.push("uses a gradient fill, which a font cannot represent (only flat colors); replace it with solid fills".to_string());
            usvg::Color::black()
        }
        usvg::Paint::Pattern(_) => {
            notes.push("uses a pattern fill, which a font cannot represent (only flat colors); replace it with solid fills".to_string());
            usvg::Color::black()
        }
    };
    let alpha = (fill.opacity().get() * 255.0).round() as u8;
    if color == usvg::Color::black() && alpha == 255 {
        Paint::Foreground
    } else {
        Paint::Rgba(color.red, color.green, color.blue, alpha)
    }
}

fn visit_layers(node: &usvg::Node, layers: &mut Vec<RawLayer>, notes: &mut Vec<String>) {
    match node {
        usvg::Node::Group(group) => {
            // Anything a font outline cannot express is reported, never
            // silently dropped: the SVG would render differently in the font
            // than in a browser.
            if group.opacity().get() < 1.0 {
                notes.push("uses group opacity, which cannot be converted; bake the transparency into the fill colors instead".to_string());
            }
            if group.clip_path().is_some() {
                notes.push("uses a clip path, which cannot be converted; convert the clipped shape to a plain path".to_string());
            }
            if group.mask().is_some() {
                notes.push("uses a mask, which cannot be converted; convert the masked shape to a plain path".to_string());
            }
            if !group.filters().is_empty() {
                notes.push("uses a filter (blur, shadow, ...), which cannot be converted; remove it".to_string());
            }
            if !matches!(group.blend_mode(), usvg::BlendMode::Normal) {
                notes.push("uses a blend mode, which cannot be converted; remove it".to_string());
            }
            for child in group.children() {
                visit_layers(child, layers, notes);
            }
        }
        usvg::Node::Path(path) => {
            if !path.is_visible() {
                notes.push("contains a hidden shape (visibility: hidden); delete it from the SVG".to_string());
                return;
            }
            if path.stroke().is_some() {
                notes.push("uses a stroke, which cannot be converted; convert strokes to outlines (in Inkscape: Path > Stroke to Path)".to_string());
            }
            let Some(fill) = path.fill() else { return };
            let paint = paint_of(fill, notes);

            let mut segments = Vec::new();
            visit(node, &mut segments);

            match layers.last_mut() {
                Some(last) if last.paint == paint => last.segments.extend(segments),
                _ => layers.push(RawLayer { paint, segments }),
            }
        }
        usvg::Node::Image(_) => {
            notes.push("contains an embedded image, which cannot be converted; remove it".to_string());
        }
        usvg::Node::Text(_) => {
            notes.push("contains live text, which cannot be converted; convert the text to paths".to_string());
        }
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
