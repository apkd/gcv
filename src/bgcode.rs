//! Prusa binary G-code decoding, implemented in Rust.
//!
//! Framing follows libbgcode's version 1 specification. Version 2 is accepted
//! for compatibility with gcode-lib's identical framing. No external converter
//! is invoked. Every block (including unused thumbnails) has its checksum and
//! compressed size validated before its contents are used.
//!
//! References: <https://github.com/prusa3d/libbgcode/blob/main/doc/specifications.md>
//! and the MeatPack format described by that repository's `meatpack.cpp`.

use std::{borrow::Cow, error::Error, fmt};

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

#[derive(Clone, Copy)]
struct Block<'a> {
    offset: usize,
    block_type: u16,
    compression: u16,
    encoding: u16,
    uncompressed_size: usize,
    payload: &'a [u8],
}

impl<'a> Reader<'a> {
    fn block(
        &mut self,
        checksum_type: u16,
        limits: DecodeLimits,
        blocks: &mut usize,
        decompressed_total: &mut usize,
    ) -> Result<Block<'a>, BgcodeError> {
        *blocks += 1;
        let offset = self.pos;
        if *blocks > limits.max_blocks {
            return Err(BgcodeError::new(offset, "too many binary blocks"));
        }
        let block_type = self.u16()?;
        if block_type > 5 {
            return Err(BgcodeError::new(offset, "unsupported block type"));
        }
        let compression = self.u16()?;
        if compression > 3 {
            return Err(BgcodeError::new(
                offset + 2,
                "unsupported compression method",
            ));
        }
        let uncompressed_size = self.u32()? as usize;
        let payload_size = if compression == 0 {
            uncompressed_size
        } else {
            self.u32()? as usize
        };
        if uncompressed_size > limits.max_block_bytes || payload_size > limits.max_block_bytes {
            return Err(BgcodeError::new(
                offset,
                "block exceeds configured size limit",
            ));
        }
        *decompressed_total = decompressed_total
            .checked_add(uncompressed_size)
            .filter(|n| *n <= limits.max_total_bytes)
            .ok_or_else(|| BgcodeError::new(offset, "total decompression exceeds size limit"))?;

        let encoding = self.u16()?;
        if block_type == 5 {
            let width = self.u16()?;
            let height = self.u16()?;
            if encoding > 2 || width == 0 || height == 0 {
                return Err(BgcodeError::new(
                    offset,
                    "invalid thumbnail format or dimensions",
                ));
            }
        }
        let payload = self.take(payload_size)?;
        let checksum_end = self.pos;
        if checksum_type == 1 {
            let stored = self.u32()?;
            let calculated = crc32fast::hash(&self.bytes[offset..checksum_end]);
            if stored != calculated {
                return Err(BgcodeError::new(
                    offset,
                    format!("CRC32 mismatch (stored {stored:08x}, calculated {calculated:08x})"),
                ));
            }
        }

        Ok(Block {
            offset,
            block_type,
            compression,
            encoding,
            uncompressed_size,
            payload,
        })
    }
}

impl<'a> Block<'a> {
    fn is_meatpack(&self) -> bool {
        self.block_type == 1 && matches!(self.encoding, 1 | 2)
    }
    fn unpack(self) -> Result<Cow<'a, [u8]>, BgcodeError> {
        let Self {
            compression,
            payload,
            uncompressed_size,
            offset,
            ..
        } = self;
        Ok(Cow::Owned(match compression {
            0 => return Ok(Cow::Borrowed(payload)),
            1 => inflate_exact(payload, uncompressed_size, offset)?,
            2 => heatshrink(payload, uncompressed_size, 11, offset)?,
            3 => heatshrink(payload, uncompressed_size, 12, offset)?,
            _ => unreachable!(),
        }))
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
    decode(bytes, limits, None)
}

pub(crate) fn decode(
    bytes: &[u8],
    limits: DecodeLimits,
    mut info: Option<&mut crate::print_info::Collector>,
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
    let mut body_start = None;
    unpack_blocks(&mut reader, checksum_type, limits, |block, raw| {
        let Block {
            offset,
            block_type,
            encoding,
            ..
        } = block;
        match block_type {
            1 => {
                // Most producers put all metadata first. Append the body to
                // that buffer directly, so normal files need no final copy.
                if body_start.is_none() {
                    body_start = Some(metadata.len());
                    output = std::mem::take(&mut metadata);
                }
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
                            if let Some(info) = &mut info
                                && let Some((k, v)) = line.split_once('=')
                            {
                                info.binary_field(block_type, k.trim(), v.trim());
                            }
                        }
                    }
                    1 => {
                        let value: serde_json::Value = serde_json::from_str(text).map_err(|e| {
                            BgcodeError::new(offset, format!("invalid JSON metadata: {e}"))
                        })?;
                        append_json_metadata(
                            &mut metadata,
                            &value,
                            remaining,
                            offset,
                            &mut |key, json, text| {
                                if let Some(info) = &mut info {
                                    info.binary_json_field(block_type, key, json, text);
                                }
                            },
                        )?;
                    }
                    _ => return Err(BgcodeError::new(offset, "unsupported metadata encoding")),
                }
                if let Some(info) = &mut info {
                    info.body_offset = body_start.unwrap_or(0) + metadata.len();
                }
            }
        }
        limits
            .max_total_bytes
            .checked_sub(output.len().saturating_add(metadata.len()))
            .ok_or_else(|| BgcodeError::new(offset, "decoded text exceeds size limit"))
    })?;

    let start = body_start.ok_or_else(|| BgcodeError::new(reader.pos, "no G-code blocks found"))?;
    // Metadata is placed first even if a permissive producer wrote it later.
    // This also prevents it from splitting a command spanning two data blocks.
    if !metadata.is_empty() {
        output.insert_str(start, &metadata);
    }
    Ok(output)
}

fn unpack_blocks(
    reader: &mut Reader<'_>,
    checksum_type: u16,
    limits: DecodeLimits,
    mut consume: impl FnMut(Block<'_>, &[u8]) -> Result<usize, BgcodeError>,
) -> Result<(), BgcodeError> {
    let size = reader.bytes.len();
    let mut count = 0;
    let mut decompressed = 0;
    let mut blocks = std::iter::from_fn(|| {
        (reader.pos < size)
            .then(|| reader.block(checksum_type, limits, &mut count, &mut decompressed))
    })
    .peekable();
    const WINDOW: usize = 2 * 1024 * 1024;
    let mut batch = Vec::new();
    let mut remaining_text = limits.max_total_bytes;
    while let Some(block) = blocks.next() {
        let block = block?;
        if (block.compression == 0 && !block.is_meatpack())
            || size < 512 * 1024
            || block.uncompressed_size >= WINDOW
            || rayon::current_num_threads() == 1
        {
            remaining_text = consume(block, &block.unpack()?)?;
            continue;
        }
        let mut available = WINDOW - block.uncompressed_size;
        batch.clear();
        batch.push(block);
        while batch.len() < 32
            && let Some(Ok(next)) =
                blocks.next_if(|r| r.as_ref().is_ok_and(|b| b.uncompressed_size <= available))
        {
            available -= next.uncompressed_size;
            batch.push(next);
        }
        if batch.len() == 1 {
            remaining_text = consume(block, &block.unpack()?)?;
        } else {
            use rayon::prelude::*;
            // Decode independent blocks together. Interpret them in order so
            // byte limits, intermediate metadata and the first error stay exact.
            // A MeatPack input byte emits at most two text bytes. When every
            // block is G-code and all text fits, the limit cannot interrupt a
            // token. Otherwise decode tokens in order with the exact budget.
            let expand_text = (WINDOW - available) * 2 <= remaining_text
                && batch.iter().all(|b| b.block_type == 1);
            let decoded: Vec<_> = batch
                .par_iter()
                .map(|&block| {
                    let raw = block.unpack()?;
                    if expand_text && block.is_meatpack() {
                        let text = meatpack(&raw, 2 * raw.len(), block.offset)?;
                        Ok((
                            Block {
                                encoding: 0,
                                ..block
                            },
                            Cow::Owned(text),
                        ))
                    } else {
                        Ok((block, raw))
                    }
                })
                .collect();
            for result in decoded {
                let (block, raw) = result?;
                remaining_text = consume(block, &raw)?;
            }
        }
    }
    Ok(())
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
    field: &mut impl FnMut(&str, &serde_json::Value, &str),
) -> Result<(), BgcodeError> {
    if let Some(object) = value.as_object() {
        for (key, value) in object {
            if value.is_object() {
                append_json_metadata(out, value, limit, offset, field)?;
            } else {
                let text = match value {
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
                field(key, value, &text);
                append_comment(out, &format!("{key} = {text}"), limit, offset)?;
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

fn heatshrink(
    mut payload: &[u8],
    expected: usize,
    window_bits: usize,
    offset: usize,
) -> Result<Vec<u8>, BgcodeError> {
    let mut out = vec![0; expected];
    let mut written = 0;
    let mut skip = 0;
    while written < expected {
        // Even an unaligned window-12 match fits in three bytes (7 + 17 bits).
        let word = if let Some(bytes) = payload.first_chunk::<4>() {
            u32::from_be_bytes(*bytes)
        } else {
            let mut tail = [0; 4];
            tail[..payload.len()].copy_from_slice(payload);
            u32::from_be_bytes(tail)
        } << skip;
        let literal = word >> 31 != 0;
        let used = skip + if literal { 9 } else { window_bits + 5 };
        if payload.len() < 3 && used > payload.len() * 8 {
            return Err(BgcodeError::new(offset, "truncated Heatshrink stream"));
        }
        payload = &payload[used / 8..];
        skip = used % 8;
        if literal {
            out[written] = (word >> 23) as u8;
            written += 1;
        } else {
            let distance = ((word << 1) >> (32 - window_bits)) as usize + 1;
            let length = ((word >> (27 - window_bits)) & 15) as usize + 1;
            if length > expected - written {
                return Err(BgcodeError::new(
                    offset,
                    "Heatshrink output exceeds declared size",
                ));
            }
            let end = written + length;
            // References before the first byte read from a zero-filled dictionary.
            written = written.max(distance.min(end));
            if written < end {
                let source = written - distance;
                if end - written <= distance {
                    out.copy_within(source..source + (end - written), written);
                } else {
                    for i in written..end {
                        out[i] = out[i - distance];
                    }
                }
            }
            written = end;
        }
    }
    if !(payload.is_empty() || matches!(payload, [last] if skip != 0 && last << skip == 0)) {
        return Err(BgcodeError::new(
            offset,
            "Heatshrink stream has non-padding trailing data",
        ));
    }
    Ok(out)
}

fn meatpack(payload: &[u8], limit: usize, offset: usize) -> Result<Vec<u8>, BgcodeError> {
    const fn pairs(alphabet: &[u8; 16]) -> [[u8; 2]; 256] {
        let mut table = [[0; 2]; 256];
        let mut i = 0;
        while i < 256 {
            table[i] = [alphabet[i & 15], alphabet[i >> 4]];
            i += 1;
        }
        table
    }
    static SPACES: [[u8; 2]; 256] = pairs(b"0123456789. \nGX\0");
    static EXTRUSION: [[u8; 2]; 256] = pairs(b"0123456789.E\nGX\0");
    let mut alphabet = &SPACES;
    let mut out = Vec::with_capacity(payload.len().min(limit));
    let mut pos = 0;
    let mut packing = false;
    let mut pending_raw = 0;
    let mut held = None;
    while pos < payload.len() {
        let byte = payload[pos];
        if !packing && byte != 0xff {
            let rest = &payload[pos..];
            let n = memchr::memchr(0xff, rest).unwrap_or(rest.len());
            if n > limit - out.len() {
                return Err(BgcodeError::new(
                    offset,
                    "MeatPack output exceeds size limit",
                ));
            }
            out.extend_from_slice(&rest[..n]);
            pos += n;
            continue;
        }
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
                    alphabet = &SPACES;
                }
                248 => {} // Query configuration emits no G-code.
                247 => alphabet = &EXTRUSION,
                246 => alphabet = &SPACES,
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
            // Zero marks an escaped nibble. Most bytes expand to two digits.
            let pair = alphabet[usize::from(byte)];
            match pair {
                [0, second] => {
                    pending_raw = if second == 0 { 2 } else { 1 };
                    if second != 0 {
                        held = Some(second);
                    }
                }
                // The upper nibble is padding when the low nibble ends a line.
                [b'\n', _] => out.push(b'\n'),
                [first, 0] => {
                    out.push(first);
                    pending_raw = 1;
                }
                _ => out.extend_from_slice(&pair),
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
