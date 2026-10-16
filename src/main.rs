mod config;
mod font;
mod outline;
mod svg;
mod woff2;

use clap::Parser;
use config::{parse_codepoint, Config};
use font::{build_font, build_font_tables, FontGlyph, FontLayer};
use svg::{parse_svg_layers, parse_svg_raw, summarize, OutlineSummary, Paint, RawOutline};
use outline::{build_glyph_outline, GlyphOutline};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use woff2::build_woff2;

/// iconpacker - step 3: load a font.json-style config, resolve every glyph's
/// SVG source, parse its outline geometry, and convert it into a glyph
/// outline (quadratic curves, integer font-unit coordinates). No font file
/// is assembled yet.
#[derive(Parser, Debug)]
#[command(name = "iconpacker", version, about)]
struct Args {
    /// Path to the config JSON (same shape as your existing font.json). Pass "-" to read it from stdin.
    config: PathBuf,

    /// Override the config's "input" folder (glyph "src" paths are resolved against this)
    #[arg(short, long)]
    input: Option<PathBuf>,

    /// Units per em for the generated glyph outlines
    #[arg(long, default_value_t = 1000)]
    upm: u16,

    /// Max error (in font units) allowed when refitting cubic curves as quadratics
    #[arg(long, default_value_t = 1.0)]
    accuracy: f64,

    /// Emit JSON instead of a human-readable table
    #[arg(long, default_value_t = false)]
    json: bool,

    /// Write a .ttf here once every glyph resolves and parses cleanly
    #[arg(short, long)]
    out: Option<PathBuf>,

    /// Write a .woff2 here once every glyph resolves and parses cleanly
    #[arg(long)]
    woff2: Option<PathBuf>,
}

#[derive(serde::Serialize, Debug)]
struct ResolvedGlyph {
    key: String,
    codepoint: Option<u32>,
    codepoint_hex: Option<String>,
    src: String,
    resolved_path: String,
    exists: bool,
    outline: Option<OutlineSummary>,
    glyph: Option<GlyphOutline>,
    parse_error: Option<String>,
    color: bool,
    #[serde(skip)]
    layers: Vec<FontLayer>,
}

/// Default cap on COLR layers per color glyph. Every layer costs extra glyph
/// data in the font (roughly 25-30 bytes compressed), and layers can
/// outnumber colors when same-colored shapes are interleaved in the SVG, so
/// this is the limit that actually tracks font size. It also bounds the
/// number of distinct colors, since each color needs at least one layer.
/// Override with `"max_layers"` in the config's `props`.
const DEFAULT_MAX_LAYERS: usize = 128;

fn describe_paint(p: &Paint) -> String {
    match p {
        Paint::Foreground => "black/currentColor".to_string(),
        Paint::Rgba(r, g, b, 255) => format!("#{:02x}{:02x}{:02x}", r, g, b),
        Paint::Rgba(r, g, b, a) => format!("#{:02x}{:02x}{:02x}/{}", r, g, b, a),
    }
}

fn main() -> ExitCode {
    let args = Args::parse();

    let reading_stdin = args.config == Path::new("-");

    let raw_config = if reading_stdin {
        use std::io::Read;
        let mut buf = String::new();
        match std::io::stdin().read_to_string(&mut buf) {
            Ok(_) => buf,
            Err(e) => {
                eprintln!("error: could not read config from stdin: {}", e);
                return ExitCode::FAILURE;
            }
        }
    } else {
        match std::fs::read_to_string(&args.config) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: could not read config '{}': {}", args.config.display(), e);
                return ExitCode::FAILURE;
            }
        }
    };

    let config: Config = match serde_json::from_str(&raw_config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: invalid config JSON: {}", e);
            return ExitCode::FAILURE;
        }
    };

    // Base dir for resolving relative glyph "src" paths:
    // config file's own directory, joined with config.input (or "." if absent),
    // unless --input overrides it outright. When the config came from stdin
    // there's no file location to anchor to, so config.input (expected to be
    // an absolute path in that case) is used as-is.
    let config_dir = if reading_stdin {
        PathBuf::from(".")
    } else {
        args.config
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    };

    let base_dir: PathBuf = if let Some(over) = &args.input {
        over.clone()
    } else {
        config_dir.join(config.input.as_deref().unwrap_or("."))
    };

    if !base_dir.is_dir() {
        eprintln!("error: input folder '{}' does not exist or is not a directory", base_dir.display());
        return ExitCode::FAILURE;
    }

    let family = config
        .props
        .get("family")
        .and_then(Value::as_str)
        .unwrap_or("(no family set)");

    let max_layers = config
        .props
        .get("max_layers")
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_LAYERS);

    let mut resolved = Vec::new();
    let mut missing = 0usize;

    let mut color_errors = 0usize;

    for (key, entry) in &config.glyphs {
        let codepoint = parse_codepoint(key);
        let resolved_path = base_dir.join(&entry.src);
        let exists = resolved_path.is_file();
        if !exists {
            missing += 1;
        }

        let mut outline = None;
        let mut glyph = None;
        let mut parse_error = None;
        let mut layers: Vec<FontLayer> = Vec::new();

        if exists {
            match parse_svg_raw(&resolved_path) {
                Ok(raw) => {
                    outline = Some(summarize(&raw));
                    glyph = Some(build_glyph_outline(&raw, args.upm, args.accuracy));
                }
                Err(e) => parse_error = Some(e),
            }

            // Layer analysis runs for every glyph: color glyphs need it to be
            // built, and monochrome ones use it to warn when an SVG carries
            // colors that are about to be flattened away.
            if let Ok(raw_layers) = parse_svg_layers(&resolved_path) {
                let paints = raw_layers.distinct_paints();
                let label = &entry.src;

                for note in &raw_layers.notes {
                    eprintln!("error: {}: {}", label, note);
                }
                if !raw_layers.notes.is_empty() {
                    color_errors += 1;
                }

                if entry.color {
                    if paints.is_empty() {
                        eprintln!("warning: {}: marked color but has no filled paths; treated as monochrome", label);
                    } else {
                        let mut built: Vec<FontLayer> = Vec::new();
                        for l in &raw_layers.layers {
                            let layer_raw = RawOutline {
                                width: raw_layers.width,
                                height: raw_layers.height,
                                segments: l.segments.clone(),
                            };
                            let outline = build_glyph_outline(&layer_raw, args.upm, args.accuracy);
                            if !outline.contours.is_empty() {
                                built.push(FontLayer { paint: l.paint, outline });
                            }
                        }

                        if built.len() > max_layers {
                            eprintln!(
                                "error: {}: {} layers ({} distinct colors), but the limit is {} (set \"max_layers\" in props to raise it; if the same colors alternate shape by shape, group same-colored shapes together in the SVG)",
                                label,
                                built.len(),
                                paints.len(),
                                max_layers
                            );
                            color_errors += 1;
                        } else {
                            if paints.len() == 1 {
                                if paints[0] == Paint::Foreground {
                                    eprintln!(
                                        "warning: {}: marked color but contains only black/currentColor, so it behaves like a monochrome icon; remove \"color\" to keep it simple",
                                        label
                                    );
                                } else {
                                    eprintln!(
                                        "warning: {}: marked color but contains only one color ({}); if it should follow CSS color, remove \"color\"",
                                        label,
                                        describe_paint(&paints[0])
                                    );
                                }
                            }
                            layers = built;
                        }
                    }
                } else if paints.len() > 1 {
                    let list: Vec<String> = paints.iter().map(describe_paint).collect();
                    eprintln!(
                        "error: {}: contains {} distinct colors ({}) but \"color\" is not set, so it would be flattened to one monochrome shape (set \"color\": true to keep the colors, or simplify the SVG to a single color)",
                        label,
                        paints.len(),
                        list.join(", ")
                    );
                    color_errors += 1;
                }
            }
        }

        resolved.push(ResolvedGlyph {
            key: key.clone(),
            codepoint,
            codepoint_hex: codepoint.map(|cp| format!("U+{:04X}", cp)),
            src: entry.src.clone(),
            resolved_path: resolved_path.to_string_lossy().to_string(),
            exists,
            outline,
            glyph,
            parse_error,
            color: entry.color,
            layers
        });
    }

    if args.json {
        println!("{}", serde_json::to_string_pretty(&resolved).unwrap());
    } else {
        println!(
            "font: {}   glyphs: {}   input: {}   output: {:?}   upm: {}\n",
            family,
            resolved.len(),
            base_dir.display(),
            config.output,
            args.upm
        );
        for g in &resolved {
            let cp = g.codepoint_hex.clone().unwrap_or_else(|| format!("? ({})", g.key));
            let status = if g.exists { "ok" } else { "MISSING" };
            println!("{:<10} {:<8} {:<7} {}", cp, status, g.key, g.resolved_path);

            if let Some(o) = &g.outline {
                println!(
                    "           svg:   viewBox {:.0}x{:.0}   contours: {}   points: {}   (line: {}, quad: {}, cubic: {})",
                    o.svg_width, o.svg_height, o.contours, o.points, o.line_segments, o.quad_segments, o.cubic_segments
                );
            }
            if let Some(gl) = &g.glyph {
                println!(
                    "           glyph: contours: {}   points: {} (on-curve: {})   advance: {}",
                    gl.contours.len(),
                    gl.point_count(),
                    gl.on_curve_count(),
                    gl.advance_width
                );
            }
            if let Some(err) = &g.parse_error {
                println!("           parse error: {}", err);
            }
        }
    }

    if missing > 0 {
        eprintln!("\n{} glyph(s) reference a missing SVG file", missing);
        return ExitCode::FAILURE;
    }

    if color_errors > 0 {
        eprintln!("\n{} glyph(s) have color or layer errors", color_errors);
        return ExitCode::FAILURE;
    }

    if args.out.is_some() || args.woff2.is_some() {
        let font_glyphs: Vec<FontGlyph> = resolved
            .iter_mut()
            .filter_map(|g| {
                let codepoint = g.codepoint?;
                // glyph field is only None if outline parsing failed above,
                // which would already have set `missing`-style errors
                Some(FontGlyph {
                    codepoint,
                    outline: g.glyph.as_ref()?.clone(),
                    layers: std::mem::take(&mut g.layers)
                })
            })
            .collect();

        if let Some(out_path) = &args.out {
            match build_font(&font_glyphs, args.upm, family) {
                Ok(bytes) => match std::fs::write(out_path, &bytes) {
                    Ok(()) => eprintln!("\nwrote {} ({} bytes, {} glyphs)", out_path.display(), bytes.len(), font_glyphs.len()),
                    Err(e) => {
                        eprintln!("\nerror: could not write '{}': {}", out_path.display(), e);
                        return ExitCode::FAILURE;
                    }
                },
                Err(e) => {
                    eprintln!("\nerror: could not build font: {}", e);
                    return ExitCode::FAILURE;
                }
            }
        }

        if let Some(woff2_path) = &args.woff2 {
            let tables = match build_font_tables(&font_glyphs, args.upm, family) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("\nerror: could not build font: {}", e);
                    return ExitCode::FAILURE;
                }
            };
            match build_woff2(&tables) {
                Ok(bytes) => match std::fs::write(woff2_path, &bytes) {
                    Ok(()) => eprintln!("wrote {} ({} bytes, {} glyphs)", woff2_path.display(), bytes.len(), font_glyphs.len()),
                    Err(e) => {
                        eprintln!("\nerror: could not write '{}': {}", woff2_path.display(), e);
                        return ExitCode::FAILURE;
                    }
                },
                Err(e) => {
                    eprintln!("\nerror: could not build woff2: {}", e);
                    return ExitCode::FAILURE;
                }
            }
        }
    }

    ExitCode::SUCCESS
}
