mod config;
mod font;
mod outline;
mod svg;
mod woff2;

use clap::Parser;
use config::{parse_codepoint, Config};
use font::{build_font, build_font_tables, FontGlyph};
use outline::{build_glyph_outline, GlyphOutline};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use svg::{parse_svg_raw, summarize, OutlineSummary};
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

    let mut resolved = Vec::new();
    let mut missing = 0usize;

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

        if exists {
            match parse_svg_raw(&resolved_path) {
                Ok(raw) => {
                    outline = Some(summarize(&raw));
                    glyph = Some(build_glyph_outline(&raw, args.upm, args.accuracy));
                }
                Err(e) => parse_error = Some(e),
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

    if args.out.is_some() || args.woff2.is_some() {
        let font_glyphs: Vec<FontGlyph> = resolved
            .iter()
            .filter_map(|g| {
                let codepoint = g.codepoint?;
                // glyph field is only None if outline parsing failed above,
                // which would already have set `missing`-style errors
                Some(FontGlyph {
                    codepoint,
                    outline: g.glyph.as_ref()?.clone(),
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
