//! Prusa binary G-code decoding, implemented in Rust.
//!
//! Framing follows libbgcode's version 1 specification. Version 2 is accepted
//! for compatibility with gcode-lib's identical framing. No external converter
//! is invoked. Every block (including unused thumbnails) has its checksum and
//! compressed size validated before its contents are used.
//!
//! References: <https://github.com/prusa3d/libbgcode/blob/main/doc/specifications.md>
//! and the MeatPack format described by that repository's `meatpack.cpp`.

use std::{error::Error, fmt};

use flate2::{Decompress, FlushDecompress, Status};

/// Resource limits apply to decompressed blocks, total decompression work,
/// and decoded text. They can be lowered by embedding applications.
#[derive(Clone, Copy, Debug)]
pub struct DecodeLimits {
    pub max_block_bytes: usize,
    pub max_total_bytes: usize,
    pub max_blocks: usize,
}

impl Default for DecodeLimits {
    fn default() -> Self {
        Self {
            max_block_bytes: 256 * 1024 * 1024,
            max_total_bytes: 512 * 1024 * 1024,
            max_blocks: 1_000_000,
        }
    }
}

/// A malformed or unsupported binary file, with its byte offset.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BgcodeError {
    pub offset: usize,
    pub message: String,
}

impl BgcodeError {
    fn new(offset: usize, message: impl Into<String>) -> Self {
        Self {
            offset,
            message: message.into(),
        }
    }
}

impl fmt::Display for BgcodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "binary G-code at byte {}: {}", self.offset, self.message)
    }
}

impl Error for BgcodeError {}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], BgcodeError> {
        let end = self
            .pos
            .checked_add(count)
            .ok_or_else(|| BgcodeError::new(self.pos, "block size overflow"))?;
        let data = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| BgcodeError::new(self.pos, "truncated block or file header"))?;
        self.pos = end;
        Ok(data)
    }

    fn u16(&mut self) -> Result<u16, BgcodeError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, BgcodeError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Decode G-code blocks and preserve metadata as semicolon comments.
///
/// Block boundaries do not insert line breaks into G-code. MeatPack produces
/// compact words such as `G1X10Y20`; the text parser accepts these directly.
/// Thumbnails are validated but omitted from the result.
pub fn decode_bgcode(bytes: &[u8]) -> Result<String, BgcodeError> {
    decode_bgcode_with_limits(bytes, DecodeLimits::default())
}

pub fn decode_bgcode_with_limits(
    bytes: &[u8],
    limits: DecodeLimits,
) -> Result<String, BgcodeError> {
    let mut reader = Reader { bytes, pos: 0 };
    if reader.take(4)? != b"GCDE" {
        return Err(BgcodeError::new(0, "expected GCDE magic number"));
    }
    let version = reader.u32()?;
    if !matches!(version, 1 | 2) {
        return Err(BgcodeError::new(
            4,
            format!("unsupported version {version}; supported versions are 1 and 2"),
        ));
    }
    let checksum_type = reader.u16()?;
    if checksum_type > 1 {
        return Err(BgcodeError::new(8, "unsupported checksum algorithm"));
    }

    let mut output = String::new();
    let mut metadata = String::new();
    let mut decompressed_total = 0usize;
    let mut blocks = 0usize;
    let mut found_gcode = false;

    while reader.pos < bytes.len() {
        blocks += 1;
        let offset = reader.pos;
        if blocks > limits.max_blocks {
            return Err(BgcodeError::new(offset, "too many binary blocks"));
        }
        let block_type = reader.u16()?;
        if block_type > 5 {
            return Err(BgcodeError::new(offset, "unsupported block type"));
        }
        let compression = reader.u16()?;
        if compression > 3 {
            return Err(BgcodeError::new(
                offset + 2,
                "unsupported compression method",
            ));
        }
        let uncompressed_size = reader.u32()? as usize;
        let payload_size = if compression == 0 {
            uncompressed_size
        } else {
            reader.u32()? as usize
        };
        if uncompressed_size > limits.max_block_bytes || payload_size > limits.max_block_bytes {
            return Err(BgcodeError::new(
                offset,
                "block exceeds configured size limit",
            ));
        }
        decompressed_total = decompressed_total
            .checked_add(uncompressed_size)
            .filter(|n| *n <= limits.max_total_bytes)
            .ok_or_else(|| BgcodeError::new(offset, "total decompression exceeds size limit"))?;

        let encoding = reader.u16()?;
        if block_type == 5 {
            let width = reader.u16()?;
            let height = reader.u16()?;
            if encoding > 2 || width == 0 || height == 0 {
                return Err(BgcodeError::new(
                    offset,
                    "invalid thumbnail format or dimensions",
                ));
            }
        }
        let payload = reader.take(payload_size)?;
        let checksum_end = reader.pos;
        if checksum_type == 1 {
            let stored = reader.u32()?;
            let calculated = crc32fast::hash(&bytes[offset..checksum_end]);
            if stored != calculated {
                return Err(BgcodeError::new(
                    offset,
                    format!("CRC32 mismatch (stored {stored:08x}, calculated {calculated:08x})"),
                ));
            }
        }

        // Avoid a second allocation for uncompressed blocks.
        let decompressed;
        let raw = if compression == 0 {
            payload
        } else {
            decompressed = match compression {
                1 => inflate_exact(payload, uncompressed_size, offset)?,
                2 => heatshrink(payload, uncompressed_size, 11, offset)?,
                3 => heatshrink(payload, uncompressed_size, 12, offset)?,
                _ => unreachable!(),
            };
            decompressed.as_slice()
        };

        match block_type {
            1 => {
                found_gcode = true;
                let decoded;
                let text_bytes = match encoding {
                    0 => raw,
                    1 | 2 => {
                        let remaining = limits
                            .max_total_bytes
                            .saturating_sub(output.len().saturating_add(metadata.len()));
                        decoded = meatpack(raw, remaining, offset)?;
                        decoded.as_slice()
                    }
                    _ => return Err(BgcodeError::new(offset, "unsupported G-code encoding")),
                };
                let text = std::str::from_utf8(text_bytes)
                    .map_err(|_| BgcodeError::new(offset, "G-code block is not valid UTF-8"))?;
                append_bounded(
                    &mut output,
                    text,
                    limits.max_total_bytes.saturating_sub(metadata.len()),
                    offset,
                )?;
            }
            5 => {} // Thumbnails are not part of the toolpath.
            _ => {
                let text = std::str::from_utf8(raw)
                    .map_err(|_| BgcodeError::new(offset, "metadata block is not valid UTF-8"))?;
                let remaining = limits.max_total_bytes.saturating_sub(output.len());
                match encoding {
                    0 => {
                        for line in text.lines() {
                            append_comment(&mut metadata, line, remaining, offset)?;
                        }
                    }
                    1 => {
                        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| {
                            BgcodeError::new(offset, format!("invalid JSON metadata: {e}"))
                        })?;
                        append_json_metadata(&mut metadata, &value, remaining, offset)?;
                    }
                    _ => return Err(BgcodeError::new(offset, "unsupported metadata encoding")),
                }
            }
        }
        if output.len().saturating_add(metadata.len()) > limits.max_total_bytes {
            return Err(BgcodeError::new(offset, "decoded text exceeds size limit"));
        }
    }

    if !found_gcode {
        return Err(BgcodeError::new(reader.pos, "no G-code blocks found"));
    }
    // Metadata is placed first even if a permissive producer wrote it later.
    // This also prevents it from splitting a command spanning two data blocks.
    if metadata.is_empty() {
        Ok(output)
    } else {
        metadata.push_str(&output);
        Ok(metadata)
    }
}

fn append_bounded(
    out: &mut String,
    text: &str,
    limit: usize,
    offset: usize,
) -> Result<(), BgcodeError> {
    if out.len().saturating_add(text.len()) > limit {
        return Err(BgcodeError::new(offset, "decoded text exceeds size limit"));
    }
    out.push_str(text);
    Ok(())
}

fn append_comment(
    out: &mut String,
    text: &str,
    limit: usize,
    offset: usize,
) -> Result<(), BgcodeError> {
    // Split again: JSON strings may embed newlines. Every line must remain a
    // comment, even when metadata happens to contain something like G92.
    for line in text.lines() {
        append_bounded(out, "; ", limit, offset)?;
        append_bounded(out, line, limit, offset)?;
        append_bounded(out, "\n", limit, offset)?;
    }
    Ok(())
}

fn append_json_metadata(
    out: &mut String,
    value: &serde_json::Value,
    limit: usize,
    offset: usize,
) -> Result<(), BgcodeError> {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            if value.is_object() {
                append_json_metadata(out, value, limit, offset)?;
            } else {
                let value = match value {
                    serde_json::Value::String(s) => s.clone(),
                    // Preserve familiar INI-style arrays: [250,210] -> 250,210.
                    serde_json::Value::Array(values) => values
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| v.to_string())
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                    value => value.to_string(),
                };
                append_comment(out, &format!("{key} = {value}"), limit, offset)?;
            }
        }
    } else {
        append_comment(out, &value.to_string(), limit, offset)?;
    }
    Ok(())
}

fn inflate_exact(payload: &[u8], expected: usize, offset: usize) -> Result<Vec<u8>, BgcodeError> {
    // libbgcode skips compression entirely for empty blocks, retaining the
    // requested compression enum with both sizes zero.
    if payload.is_empty() && expected == 0 {
        return Ok(Vec::new());
    }
    let capacity = expected
        .checked_add(1)
        .ok_or_else(|| BgcodeError::new(offset, "DEFLATE size overflow"))?;
    let mut out = vec![0u8; capacity];
    let mut decoder = Decompress::new(true); // libbgcode uses the zlib wrapper.
    let status = decoder
        .decompress(payload, &mut out, FlushDecompress::Finish)
        .map_err(|e| BgcodeError::new(offset, format!("invalid DEFLATE stream: {e}")))?;
    if status != Status::StreamEnd
        || decoder.total_out() != expected as u64
        || decoder.total_in() != payload.len() as u64
    {
        return Err(BgcodeError::new(
            offset,
            "DEFLATE stream is truncated, has trailing data, or does not match its declared size",
        ));
    }
    out.truncate(expected);
    Ok(out)
}

struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
    offset: usize,
}

impl Bits<'_> {
    fn read(&mut self, count: usize) -> Result<usize, BgcodeError> {
        if self.pos.saturating_add(count) > self.bytes.len().saturating_mul(8) {
            return Err(BgcodeError::new(self.offset, "truncated Heatshrink stream"));
        }
        let mut result = 0;
        for _ in 0..count {
            result =
                (result << 1) | usize::from((self.bytes[self.pos / 8] >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Ok(result)
    }
}

fn heatshrink(
    payload: &[u8],
    expected: usize,
    window_bits: usize,
    offset: usize,
) -> Result<Vec<u8>, BgcodeError> {
    let mut bits = Bits {
        bytes: payload,
        pos: 0,
        offset,
    };
    let mut out = Vec::with_capacity(expected);
    let mut window = vec![0u8; 1 << window_bits];
    let mask = window.len() - 1;
    let mut head = 0usize;
    while out.len() < expected {
        if bits.read(1)? == 1 {
            let byte = bits.read(8)? as u8;
            out.push(byte);
            window[head & mask] = byte;
            head += 1;
        } else {
            let distance = bits.read(window_bits)? + 1;
            let length = bits.read(4)? + 1;
            if length > expected - out.len() {
                return Err(BgcodeError::new(
                    offset,
                    "Heatshrink output exceeds declared size",
                ));
            }
            for _ in 0..length {
                // The initial window contains zeroes, as in reference heatshrink.
                let byte = window[head.wrapping_sub(distance) & mask];
                out.push(byte);
                window[head & mask] = byte;
                head += 1;
            }
        }
    }
    let remaining = payload.len() * 8 - bits.pos;
    if remaining >= 8 || bits.read(remaining)? != 0 {
        return Err(BgcodeError::new(
            offset,
            "Heatshrink stream has non-padding trailing data",
        ));
    }
    Ok(out)
}

fn meatpack(payload: &[u8], limit: usize, offset: usize) -> Result<Vec<u8>, BgcodeError> {
    let mut out = Vec::with_capacity(payload.len().min(limit));
    let mut pos = 0;
    let mut packing = false;
    let mut no_spaces = false;
    let mut pending_raw = 0;
    let mut held = None;
    while pos < payload.len() {
        let byte = payload[pos];
        pos += 1;
        if byte == 0xff && payload.get(pos) == Some(&0xff) {
            pos += 1;
            let command = *payload
                .get(pos)
                .ok_or_else(|| BgcodeError::new(offset, "truncated MeatPack command"))?;
            pos += 1;
            if pending_raw != 0 {
                return Err(BgcodeError::new(
                    offset,
                    "MeatPack command interrupts escaped character",
                ));
            }
            match command {
                251 => packing = true,
                250 => packing = false,
                249 => {
                    packing = false;
                    no_spaces = false;
                }
                248 => {} // Query configuration emits no G-code.
                247 => no_spaces = true,
                246 => no_spaces = false,
                _ => return Err(BgcodeError::new(offset, "unsupported MeatPack command")),
            }
            continue;
        }
        if pending_raw > 0 {
            out.push(byte);
            pending_raw -= 1;
            if pending_raw == 0
                && let Some(char) = held.take()
            {
                out.push(char);
            }
        } else if !packing {
            out.push(byte);
        } else {
            let decode_nibble = |n: u8| {
                if n == 11 && no_spaces {
                    b'E'
                } else {
                    b"0123456789. \nGX"[usize::from(n)]
                }
            };
            let low = byte & 15;
            let high = byte >> 4;
            if low == 15 {
                pending_raw = if high == 15 { 2 } else { 1 };
                if high != 15 {
                    held = Some(decode_nibble(high));
                }
            } else {
                let first = decode_nibble(low);
                out.push(first);
                // The upper nibble is padding when the low nibble ends a line.
                if first != b'\n' {
                    if high == 15 {
                        pending_raw = 1;
                    } else {
                        out.push(decode_nibble(high));
                    }
                }
            }
        }
        if out.len() > limit {
            return Err(BgcodeError::new(
                offset,
                "MeatPack output exceeds size limit",
            ));
        }
    }
    if pending_raw != 0 || held.is_some() {
        return Err(BgcodeError::new(
            offset,
            "truncated MeatPack escaped character",
        ));
    }
    Ok(out)
}
