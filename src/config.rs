//! Loading and resolving the font.json-style config: font metadata (`props`),
//! the glyph list, and where each glyph's SVG source file lives.

use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Deserialize, Debug)]
pub struct Config {
    #[serde(default)]
    pub props: Map<String, Value>,

    /// Folder that glyph "src" paths are relative to, same as the Python script's config['input']
    pub input: Option<String>,

    #[serde(default)]
    pub output: Vec<String>,

    pub glyphs: BTreeMap<String, GlyphEntry>,
}

#[derive(Deserialize, Debug)]
pub struct GlyphEntry {
    pub src: String,

    /// any other per-glyph attributes (width, emoji, altuni, ...) - kept but not
    /// interpreted yet
    #[serde(flatten)]
    #[allow(dead_code)]
    pub extra: Map<String, Value>,
}

/// Mirrors Python's `int(k, 0)`: accepts "0xE000", "0XE000", "U+E000", or plain decimal.
pub fn parse_codepoint(key: &str) -> Option<u32> {
    let k = key.trim();
    if let Some(hex) = k.strip_prefix("U+").or_else(|| k.strip_prefix("u+")) {
        return u32::from_str_radix(hex, 16).ok();
    }
    if let Some(hex) = k.strip_prefix("0x").or_else(|| k.strip_prefix("0X")) {
        return u32::from_str_radix(hex, 16).ok();
    }
    k.parse::<u32>().ok()
}
