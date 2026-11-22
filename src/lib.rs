//! G-code decoding, extraction and rendering without a display server.
//!
//! ```no_run
//! use gcv::{load_toolpath, renderer::{render, RenderOptions}, save_png};
//! let toolpath = load_toolpath("part.gcode")?;
//! let frame = render(&toolpath, &RenderOptions::default())?;
//! save_png(&frame, "part.png")?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod bgcode;
pub mod camera;
pub mod cli;
pub mod colors;
mod geometry;
#[cfg(feature = "gui")]
pub mod gui;
pub mod parser;
mod print_info;
pub mod renderer;
pub mod types;

use std::{error::Error, fmt, fs::File, io::Read, path::Path};
use types::ToolpathData;

/// Input limit, applied even to growing files and non-seekable inputs.
pub const MAX_INPUT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug)]
pub enum LoadError {
    Io(std::io::Error),
    TooLarge,
    Utf8(std::str::Utf8Error),
    Binary(bgcode::BgcodeError),
    Parse(parser::ParseError),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read input: {e}"),
            Self::TooLarge => write!(f, "input exceeds the 512 MiB limit"),
            Self::Utf8(e) => write!(f, "text G-code is not valid UTF-8: {e}"),
            Self::Binary(e) => write!(f, "invalid binary G-code: {e}"),
            Self::Parse(e) => write!(f, "invalid G-code: {e}"),
        }
    }
}

impl Error for LoadError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Utf8(e) => Some(e),
            Self::Binary(e) => Some(e),
            Self::Parse(e) => Some(e),
            Self::TooLarge => None,
        }
    }
}

/// Detects binary G-code by its magic, independent of filename extension.
pub fn load_toolpath(path: impl AsRef<Path>) -> Result<ToolpathData, LoadError> {
    load_with(path, None, parser::parse_detailed)
}

pub(crate) fn load_with<T>(
    path: impl AsRef<Path>,
    info: Option<&mut print_info::Collector>,
    parse: impl FnOnce(&str, Option<&mut print_info::Collector>) -> Result<T, parser::ParseError>,
) -> Result<T, LoadError> {
    let file = File::open(path).map_err(LoadError::Io)?;
    let metadata = file.metadata().map_err(LoadError::Io)?;
    if metadata.len() > MAX_INPUT_BYTES {
        return Err(LoadError::TooLarge);
    }
    let bytes = read_input(&file, &metadata).map_err(LoadError::Io)?;
    decode_with(&bytes, info, parse)
}

fn read_input(file: &File, metadata: &std::fs::Metadata) -> std::io::Result<Vec<u8>> {
    let size = metadata.len() as usize;
    #[cfg(unix)]
    if metadata.is_file() && size >= 2 * 1024 * 1024 && rayon::current_num_threads() > 1 {
        use rayon::prelude::*;
        use std::os::unix::fs::FileExt;
        const CHUNK: usize = 1024 * 1024;
        let mut bytes = vec![0; size];
        // Copy and fault in independent pages in parallel. Positional reads
        // leave the file cursor at zero for the streaming fallback below.
        let read = bytes
            .par_chunks_mut(CHUNK)
            .enumerate()
            .try_for_each(|(i, dst)| file.read_exact_at(dst, (i * CHUNK) as u64));
        if read.is_ok() && matches!(file.read_at(&mut [0], size as u64), Ok(0)) {
            return Ok(bytes);
        }
    }
    let mut bytes = Vec::with_capacity(size);
    file.take(MAX_INPUT_BYTES + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// Decode an in-memory text or binary G-code file.
pub fn parse_bytes(bytes: &[u8]) -> Result<ToolpathData, LoadError> {
    decode_with(bytes, None, parser::parse_detailed)
}
fn decode_with<T>(
    bytes: &[u8],
    mut info: Option<&mut print_info::Collector>,
    parse: impl FnOnce(&str, Option<&mut print_info::Collector>) -> Result<T, parser::ParseError>,
) -> Result<T, LoadError> {
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(LoadError::TooLarge);
    }
    if bytes.starts_with(b"GCDE") {
        let text = bgcode::decode(bytes, Default::default(), info.as_deref_mut())
            .map_err(LoadError::Binary)?;
        parse(&text, info).map_err(LoadError::Parse)
    } else {
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
        let text = std::str::from_utf8(bytes).map_err(LoadError::Utf8)?;
        parse(text, info).map_err(LoadError::Parse)
    }
}

/// Write a real PNG regardless of the destination's filename extension.
pub fn save_png(frame: &renderer::Frame, path: impl AsRef<Path>) -> image::ImageResult<()> {
    image::save_buffer_with_format(
        path,
        &frame.rgba,
        frame.width,
        frame.height,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{io::Seek, os::unix::fs::FileExt};

    #[test]
    fn reads_to_the_actual_end_when_a_file_changes_size() {
        let path = std::env::temp_dir().join(format!("gcv-read-{}", std::process::id()));
        let mut file = File::create_new(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        file.set_len(3 * 1024 * 1024).unwrap();
        let snapshot = file.metadata().unwrap();
        for size in [snapshot.len() + 64, 16] {
            file.set_len(size).unwrap();
            file.write_all_at(b"tail", size - 4).unwrap();
            file.rewind().unwrap();
            let bytes = read_input(&file, &snapshot).unwrap();
            assert_eq!(bytes.len() as u64, size);
            assert!(bytes.ends_with(b"tail"));
        }
    }
}
