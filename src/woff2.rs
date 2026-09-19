//! Wrapping a set of font tables into a WOFF2 file.
//!
//! WOFF2 is not a font format of its own - it's a small header plus a table
//! directory plus every table's raw bytes concatenated and brotli-compressed
//! as a single stream. The full spec (<https://www.w3.org/TR/WOFF2/>) also
//! defines a "transform" for `glyf`/`loca` that repacks them for extra
//! compression, but that transform is optional: a table can always be
//! stored untransformed ("null transform"), which is what this encoder
//! does. The result is a fully valid, spec-compliant WOFF2 - just not quite
//! as tightly packed as what fonttools/google-fonts tooling produces.

use brotli::enc::{BrotliCompress, BrotliEncoderParams};
use std::io::Cursor;
use write_fonts::types::Tag;

const WOFF2_SIGNATURE: u32 = 0x774F_4632; // 'wOF2'
const TRUETYPE_FLAVOR: u32 = 0x0001_0000; // sfnt version for TrueType (glyf) outlines

/// The 63 tags WOFF2 can reference by a single 6-bit index instead of
/// spelling out all 4 bytes. Only the tags we ever emit are listed with
/// their real index (https://www.w3.org/TR/WOFF2/#table_dir_knownTags);
/// anything else falls back to the "arbitrary tag" encoding (index 63 plus
/// the literal 4 bytes), which every table in this codebase avoids anyway.
fn known_tag_index(tag: Tag) -> Option<u8> {
    let bytes = tag.into_bytes();
    let index = match &bytes {
        b"cmap" => 0,
        b"head" => 1,
        b"hhea" => 2,
        b"hmtx" => 3,
        b"maxp" => 4,
        b"name" => 5,
        b"OS/2" => 6,
        b"post" => 7,
        b"cvt " => 8,
        b"fpgm" => 9,
        b"glyf" => 10,
        b"loca" => 11,
        b"prep" => 12,
        _ => return None,
    };
    Some(index)
}

/// WOFF2's UIntBase128: a big-endian base-128 varint (7 data bits per byte,
/// high bit set on every byte except the last). Used for every length field
/// in the table directory.
fn write_uint_base128(out: &mut Vec<u8>, mut value: u32) {
    let mut digits = [0u8; 5];
    let mut n = 0;
    loop {
        digits[n] = (value & 0x7f) as u8;
        n += 1;
        value >>= 7;
        if value == 0 {
            break;
        }
    }
    // digits were filled least-significant-first; emit most-significant-first,
    // with the continuation bit (0x80) set on every byte but the last.
    for i in (0..n).rev() {
        let byte = digits[i];
        if i == 0 {
            out.push(byte);
        } else {
            out.push(byte | 0x80);
        }
    }
}

fn pad4(len: usize) -> usize {
    (len + 3) & !3
}

/// Wraps a set of (tag, raw table bytes) pairs - e.g. from
/// `font::build_font_tables` - into a complete WOFF2 file.
pub fn build_woff2(tables: &[(Tag, Vec<u8>)]) -> Result<Vec<u8>, String> {
    let num_tables = tables.len() as u16;

    // --- table directory + concatenated raw table data, in the same order ---
    let mut directory = Vec::new();
    let mut concatenated = Vec::new();

    for (tag, bytes) in tables {
        // Every table here is stored untransformed. Per spec, transform
        // version 0 already means "null transform" for every tag except
        // glyf/loca, where the null transform is signaled by version 3
        // instead (version 0 there would mean "apply the glyf/loca
        // repacking transform", which we don't implement).
        let bytes_str = tag.into_bytes();
        let is_glyf_or_loca = &bytes_str == b"glyf" || &bytes_str == b"loca";
        let transform_version: u8 = if is_glyf_or_loca { 3 } else { 0 };

        match known_tag_index(*tag) {
            Some(index) => {
                directory.push((transform_version << 6) | index);
            }
            None => {
                directory.push((transform_version << 6) | 0x3F);
                directory.extend_from_slice(&bytes_str);
            }
        }
        write_uint_base128(&mut directory, bytes.len() as u32);
        // No transformLength field: with the null transform selected above
        // (for every table, glyf/loca included) the spec says none follows.

        concatenated.extend_from_slice(bytes);
    }

    // --- brotli-compress the concatenated table data as one stream ---
    let mut params = BrotliEncoderParams::default();
    params.quality = 11;
    params.lgwin = 24;
    params.size_hint = concatenated.len();

    let mut compressed = Vec::new();
    BrotliCompress(&mut Cursor::new(&concatenated), &mut compressed, &params)
        .map_err(|e| format!("brotli compression failed: {e}"))?;

    // --- totalSfntSize: size of the OpenType file these tables would
    // reconstruct into (12-byte offset table + 16 bytes/table record +
    // each table padded to a 4-byte boundary) ---
    let sfnt_header = 12 + 16 * tables.len();
    let sfnt_tables: usize = tables.iter().map(|(_, bytes)| pad4(bytes.len())).sum();
    let total_sfnt_size = (sfnt_header + sfnt_tables) as u32;

    // --- assemble the file: 48-byte header, table directory, compressed data ---
    let mut out = Vec::new();
    out.extend_from_slice(&WOFF2_SIGNATURE.to_be_bytes());
    out.extend_from_slice(&TRUETYPE_FLAVOR.to_be_bytes());
    let length_pos = out.len();
    out.extend_from_slice(&0u32.to_be_bytes()); // length - patched below
    out.extend_from_slice(&num_tables.to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // reserved
    out.extend_from_slice(&total_sfnt_size.to_be_bytes());
    out.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    out.extend_from_slice(&0u16.to_be_bytes()); // majorVersion
    out.extend_from_slice(&0u16.to_be_bytes()); // minorVersion
    out.extend_from_slice(&0u32.to_be_bytes()); // metaOffset (no extended metadata)
    out.extend_from_slice(&0u32.to_be_bytes()); // metaLength
    out.extend_from_slice(&0u32.to_be_bytes()); // metaOrigLength
    out.extend_from_slice(&0u32.to_be_bytes()); // privOffset (no private data)
    out.extend_from_slice(&0u32.to_be_bytes()); // privLength

    out.extend_from_slice(&directory);
    out.extend_from_slice(&compressed);
    while out.len() % 4 != 0 {
        out.push(0);
    }

    let total_length = (out.len() as u32).to_be_bytes();
    out[length_pos..length_pos + 4].copy_from_slice(&total_length);

    Ok(out)
}
