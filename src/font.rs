//! Assembling a minimal, valid TrueType (.ttf) font from a set of glyph
//! outlines: table construction (head, hhea, maxp, hmtx, cmap, OS/2, post,
//! name, glyf, loca) via the `write-fonts` crate.

use crate::outline::GlyphOutline;
use kurbo::BezPath;
use write_fonts::tables::cmap::Cmap;
use write_fonts::tables::glyf::{Glyf, GlyfLocaBuilder, Glyph as GlyfGlyph, SimpleGlyph};
use write_fonts::tables::head::{Flags as HeadFlags, Head, MacStyle};
use write_fonts::tables::hhea::Hhea;
use write_fonts::tables::hmtx::{Hmtx, LongMetric};
use write_fonts::tables::loca::{Loca, LocaFormat};
use write_fonts::tables::maxp::Maxp;
use write_fonts::tables::name::{Name, NameRecord};
use write_fonts::tables::os2::{Os2, SelectionFlags};
use write_fonts::tables::post::Post;
use write_fonts::types::{Fixed, GlyphId, LongDateTime, NameId, Tag, Version16Dot16};
use write_fonts::{dump_table, FontBuilder};

/// One glyph to include in the font: its Unicode codepoint and outline.
pub struct FontGlyph {
    pub codepoint: u32,
    pub outline: GlyphOutline,
}

/// Turns a `GlyphOutline`'s contours into a `kurbo::BezPath` - the shape
/// `write-fonts`'s `SimpleGlyph::from_bezpath` expects. Points already
/// alternate on/off-curve the way `outline::build_glyph_outline` built
/// them (every off-curve point is immediately followed by its on-curve
/// end), so this just replays that sequence as path commands.
fn outline_to_bezpath(outline: &GlyphOutline) -> BezPath {
    let mut path = BezPath::new();
    for contour in &outline.contours {
        if contour.is_empty() {
            continue;
        }
        path.move_to((contour[0].x as f64, contour[0].y as f64));
        let mut i = 1;
        while i < contour.len() {
            if contour[i].on_curve {
                path.line_to((contour[i].x as f64, contour[i].y as f64));
                i += 1;
            } else {
                let ctrl = &contour[i];
                let end = &contour[i + 1];
                path.quad_to((ctrl.x as f64, ctrl.y as f64), (end.x as f64, end.y as f64));
                i += 2;
            }
        }
        path.close_path();
    }
    path
}

/// Every top-level table needed for a complete font, before it's either
/// assembled into a .ttf (`build_font`) or dumped to raw bytes for WOFF2
/// (`build_font_tables`).
struct Tables {
    head: Head,
    hhea: Hhea,
    maxp: Maxp,
    hmtx: Hmtx,
    cmap: Cmap,
    name: Name,
    post: Post,
    os2: Os2,
    glyf: Glyf,
    loca: Loca,
}

/// Builds every table for a font from a set of glyphs.
///
/// Glyph ID 0 is always the required, empty ".notdef" glyph; the rest are
/// assigned IDs in the order given and mapped in `cmap` by their codepoint.
fn build_tables(glyphs: &[FontGlyph], upm: u16, family: &str) -> Result<Tables, String> {
    // --- glyf / loca: .notdef (empty) first, then every glyph in order ---
    let mut glyf_loca = GlyfLocaBuilder::new();
    glyf_loca.add_glyph(&GlyfGlyph::Empty).map_err(|e| e.to_string())?;

    // .notdef: zero advance, zero side bearing, no bbox (empty glyph)
    let mut h_metrics: Vec<LongMetric> = vec![LongMetric { advance: 0, side_bearing: 0 }];

    let mut x_min = i16::MAX;
    let mut y_min = i16::MAX;
    let mut x_max = i16::MIN;
    let mut y_max = i16::MIN;
    let mut min_lsb = i16::MAX;
    let mut min_rsb = i16::MAX;
    let mut x_max_extent = i16::MIN;

    for g in glyphs {
        let path = outline_to_bezpath(&g.outline);
        let simple = SimpleGlyph::from_bezpath(&path)
            .map_err(|e| format!("glyph U+{:04X}: {:?}", g.codepoint, e))?;
        let bbox = simple.bbox;
        x_min = x_min.min(bbox.x_min);
        y_min = y_min.min(bbox.y_min);
        x_max = x_max.max(bbox.x_max);
        y_max = y_max.max(bbox.y_max);

        let advance = g.outline.advance_width.clamp(0, u16::MAX as i32) as u16;
        // Standard TrueType convention: left-side-bearing is the glyph's own
        // bbox.x_min (distance from the glyph origin to its leftmost pixel),
        // not a flat 0 - a glyph that doesn't start flush at x=0 needs this
        // to sit correctly next to whatever comes before it.
        let lsb = bbox.x_min;
        let glyph_width = (bbox.x_max - bbox.x_min) as i32;
        let rsb = (advance as i32 - (lsb as i32 + glyph_width)) as i16;
        min_lsb = min_lsb.min(lsb);
        min_rsb = min_rsb.min(rsb);
        x_max_extent = x_max_extent.max((lsb as i32 + glyph_width) as i16);

        h_metrics.push(LongMetric { advance, side_bearing: lsb });
        glyf_loca.add_glyph(&simple).map_err(|e| e.to_string())?;
    }

    if glyphs.is_empty() {
        // avoid MAX/MIN placeholder values leaking into an empty font
        x_min = 0;
        y_min = 0;
        x_max = 0;
        y_max = 0;
        min_lsb = 0;
        min_rsb = 0;
        x_max_extent = 0;
    }

    let (glyf, loca, loca_format): (Glyf, _, _) = glyf_loca.build();

    let num_glyphs = h_metrics.len() as u16;
    // The glyphs themselves were positioned (in outline::build_glyph_outline)
    // to span exactly [0, upm] - SVG bottom on the baseline, SVG top at the
    // em's top edge - so the real ascender/descender are just the computed
    // bbox, not a fabricated split. Clamping to at least [0, upm] keeps an
    // empty font's metrics sane without inventing numbers for a real one.
    let ascender = y_max;
    let descender = y_min.min(0);

    // --- head ---
    let head = Head::new(
        Fixed::from_f64(1.0),
        0, // checksum_adjustment - recomputed by FontBuilder::build()
        HeadFlags::empty(),
        upm,
        LongDateTime::new(0),
        LongDateTime::new(0),
        x_min,
        y_min,
        x_max,
        y_max,
        MacStyle::empty(),
        8, // lowest_rec_ppem
        match loca_format {
            LocaFormat::Short => 0,
            LocaFormat::Long => 1,
        },
    );

    // --- hhea / hmtx ---
    let advance_width_max = h_metrics.iter().map(|m| m.advance).max().unwrap_or(0);
    let hhea = Hhea::new(
        ascender.into(),
        descender.into(),
        0.into(), // line_gap
        advance_width_max.into(),
        min_lsb.into(),
        min_rsb.into(),
        x_max_extent.into(),
        1, // caret_slope_rise: vertical caret
        0, // caret_slope_run
        0, // caret_offset
        num_glyphs, // one explicit LongMetric per glyph, no trailing compression
    );

    let hmtx = Hmtx::new(h_metrics.clone(), Vec::new());

    // --- maxp ---
    let max_points = glyphs
        .iter()
        .map(|g| g.outline.contours.iter().map(|c| c.len()).sum::<usize>())
        .max()
        .unwrap_or(0) as u16;
    let max_contours = glyphs
        .iter()
        .map(|g| g.outline.contours.len())
        .max()
        .unwrap_or(0) as u16;
    let maxp = Maxp {
        num_glyphs,
        max_points: Some(max_points),
        max_contours: Some(max_contours),
        max_composite_points: Some(0),
        max_composite_contours: Some(0),
        max_zones: Some(1),
        max_twilight_points: Some(0),
        max_storage: Some(0),
        max_function_defs: Some(0),
        max_instruction_defs: Some(0),
        max_stack_elements: Some(0),
        max_size_of_instructions: Some(0),
        max_component_elements: Some(0),
        max_component_depth: Some(0),
    };

    // --- cmap: codepoint -> glyph id (glyph id = index + 1, .notdef is 0) ---
    let mappings = glyphs
        .iter()
        .enumerate()
        .filter_map(|(i, g)| char::from_u32(g.codepoint).map(|ch| (ch, GlyphId::new((i + 1) as u32))));
    let cmap = Cmap::from_mappings(mappings).map_err(|e| e.to_string())?;

    // --- name (Windows, Unicode BMP, US English - the widely-supported minimum) ---
    let name_record = |id: u16, s: &str| NameRecord::new(3, 1, 0x0409, NameId::new(id), s.to_string().into());
    let postscript_name: String = family.chars().filter(|c| !c.is_whitespace()).collect();
    let name = Name::new(vec![
        name_record(1, family),
        name_record(2, "Regular"),
        name_record(3, family),
        name_record(4, family),
        name_record(5, "Version 1.0"),
        name_record(6, &postscript_name),
    ]);

    // --- post (version 3.0: no per-glyph name data needed) ---
    let post = Post {
        version: Version16Dot16::VERSION_3_0,
        ..Post::default()
    };

    // --- OS/2 ---
    let first_char = glyphs.iter().map(|g| g.codepoint as u16).min().unwrap_or(0);
    let last_char = glyphs.iter().map(|g| g.codepoint as u16).max().unwrap_or(0);
    let avg_char_width = if h_metrics.is_empty() {
        0
    } else {
        (h_metrics.iter().map(|m| m.advance as u32).sum::<u32>() / h_metrics.len() as u32) as i16
    };
    let os2 = Os2 {
        x_avg_char_width: avg_char_width,
        us_weight_class: 400,
        us_width_class: 5,
        fs_type: 0,
        y_subscript_x_size: 0,
        y_subscript_y_size: 0,
        y_subscript_x_offset: 0,
        y_subscript_y_offset: 0,
        y_superscript_x_size: 0,
        y_superscript_y_size: 0,
        y_superscript_x_offset: 0,
        y_superscript_y_offset: 0,
        y_strikeout_size: 0,
        y_strikeout_position: 0,
        s_family_class: 0,
        panose_10: [0; 10],
        ul_unicode_range_1: 0,
        ul_unicode_range_2: 0,
        ul_unicode_range_3: 0,
        ul_unicode_range_4: 0,
        ach_vend_id: Tag::new(b"NONE"),
        fs_selection: SelectionFlags::REGULAR,
        us_first_char_index: first_char,
        us_last_char_index: last_char,
        s_typo_ascender: ascender,
        s_typo_descender: descender,
        s_typo_line_gap: 0,
        us_win_ascent: ascender.max(0) as u16,
        us_win_descent: descender.unsigned_abs(),
        ..Os2::default()
    };

    Ok(Tables {
        head,
        hhea,
        maxp,
        hmtx,
        cmap,
        name,
        post,
        os2,
        glyf,
        loca,
    })
}

/// Assembles a complete, valid .ttf from a set of glyphs.
pub fn build_font(glyphs: &[FontGlyph], upm: u16, family: &str) -> Result<Vec<u8>, String> {
    let t = build_tables(glyphs, upm, family)?;

    let mut builder = FontBuilder::new();
    builder
        .add_table(&t.head)
        .map_err(|e| e.to_string())?
        .add_table(&t.hhea)
        .map_err(|e| e.to_string())?
        .add_table(&t.maxp)
        .map_err(|e| e.to_string())?
        .add_table(&t.hmtx)
        .map_err(|e| e.to_string())?
        .add_table(&t.cmap)
        .map_err(|e| e.to_string())?
        .add_table(&t.name)
        .map_err(|e| e.to_string())?
        .add_table(&t.post)
        .map_err(|e| e.to_string())?
        .add_table(&t.os2)
        .map_err(|e| e.to_string())?
        .add_table(&t.glyf)
        .map_err(|e| e.to_string())?
        .add_table(&t.loca)
        .map_err(|e| e.to_string())?;

    Ok(builder.build())
}

/// Same tables as `build_font`, but returned as raw (tag, bytes) pairs
/// instead of being assembled into a .ttf. This is what `woff2::build_woff2`
/// consumes - WOFF2 stores each table's raw bytes itself rather than a
/// pre-built sfnt.
pub fn build_font_tables(glyphs: &[FontGlyph], upm: u16, family: &str) -> Result<Vec<(Tag, Vec<u8>)>, String> {
    let t = build_tables(glyphs, upm, family)?;
    Ok(vec![
        (Tag::new(b"head"), dump_table(&t.head).map_err(|e| e.to_string())?),
        (Tag::new(b"hhea"), dump_table(&t.hhea).map_err(|e| e.to_string())?),
        (Tag::new(b"maxp"), dump_table(&t.maxp).map_err(|e| e.to_string())?),
        (Tag::new(b"hmtx"), dump_table(&t.hmtx).map_err(|e| e.to_string())?),
        (Tag::new(b"cmap"), dump_table(&t.cmap).map_err(|e| e.to_string())?),
        (Tag::new(b"name"), dump_table(&t.name).map_err(|e| e.to_string())?),
        (Tag::new(b"post"), dump_table(&t.post).map_err(|e| e.to_string())?),
        (Tag::new(b"OS/2"), dump_table(&t.os2).map_err(|e| e.to_string())?),
        (Tag::new(b"glyf"), dump_table(&t.glyf).map_err(|e| e.to_string())?),
        (Tag::new(b"loca"), dump_table(&t.loca).map_err(|e| e.to_string())?),
    ])
}
