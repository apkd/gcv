//! Text G-code interpretation without a printer connection.
//!
//! Layers intentionally follow upstream's **Z groups**, not slicer layer
//! comments: every motion changing Z starts a group, including Z hops, startup
//! and parking moves. A nonempty header forms group zero. G90/G91 and M82/M83
//! remain independent, as in gcode-lib. XYZ G92 uses programmed coordinates;
//! it does not simulate machine work offsets. Arc chords are at most 0.2 mm
//! and 5 degrees. Unlike upstream, pure Z moves are included as travel lines.
use crate::geometry::{FeaturePaths, Geometry, PlanarArc, PreviewPaths, Print};
use crate::types::LayerTable;
use crate::types::{Bounds, Diagnostic, ExtrusionType, Segment, ToolpathData};
use std::{
    error::Error,
    f64::consts::{PI, TAU},
    fmt,
};

const EPS: f64 = 1e-9;
const MAX_SEGMENTS: usize = 5_000_000;
const MAX_LAYERS: usize = 1_000_000;
const MAX_LINE_BYTES: usize = 1_048_576;
const MAX_NUMBER: f64 = 1e12;

mod parallel;

fn supported_number(value: f64) -> bool {
    value.abs() <= MAX_NUMBER
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    pub line: usize,
    pub message: String,
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            f.write_str(&self.message)
        } else {
            write!(f, "line {}: {}", self.line, self.message)
        }
    }
}
impl Error for ParseError {}
fn error(line: usize, message: impl Into<String>) -> ParseError {
    ParseError {
        line,
        message: message.into(),
    }
}

#[derive(Clone, Copy)]
struct Word {
    letter: u8,
    value: Option<f64>,
}
#[derive(Default)]
struct Words {
    items: Vec<Word>,
    fields: [Option<f64>; 26],
    present: u32,
}
impl Words {
    fn get(&self, key: u8) -> Option<f64> {
        if self.has(key) {
            self.fields[usize::from(key - b'A')]
        } else {
            None
        }
    }
    fn has(&self, key: u8) -> bool {
        self.present & (1 << (key - b'A')) != 0
    }
    fn set(&mut self, key: u8, value: Option<f64>) {
        self.fields[usize::from(key - b'A')] = value;
        self.present |= 1 << (key - b'A');
    }

    // The common slicer form: one motion followed by plain decimal fields.
    // Exponents, bare selectors, comments, and mixed commands use the full lexer.
    fn plain_motion(&mut self, code: &[u8]) -> Option<u32> {
        let [b'G', n @ b'0'..=b'3', rest @ ..] = code else {
            return None;
        };
        if code.len() > 1024 {
            return None;
        }
        let mut rest = rest.trim_ascii_start();
        self.present = 0;
        if let Some(([x, y], tail)) = fixed_xy(rest) {
            self.set(b'X', Some(x));
            self.set(b'Y', Some(y));
            rest = tail.trim_ascii_start();
        }
        while !rest.is_empty() {
            let (&key, digits) = rest.split_first()?;
            if !matches!(
                key,
                b'X' | b'Y' | b'Z' | b'E' | b'F' | b'I' | b'J' | b'K' | b'R' | b'P'
            ) {
                return None;
            }
            let (value, tail) = decimal(digits)?;
            self.set(key, Some(value));
            rest = tail.trim_ascii_start();
        }
        Some(u32::from(*n - b'0'))
    }
}

// Slicers often emit both axes in this fixed-width form: X123.456 Y123.456.
// Decode both fields together; other widths, signs and separators stay on the
// ordinary decimal path. Keep division by 1000 to preserve decimal rounding.
fn fixed_xy(bytes: &[u8]) -> Option<([f64; 2], &[u8])> {
    let (pair, tail) = bytes.split_first_chunk::<17>()?;
    if pair[8] != b' ' || tail.first().is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let x = u64::from_le_bytes(pair[..8].try_into().unwrap()) ^ u64::from_le_bytes(*b"X000.000");
    let y = u64::from_le_bytes(pair[9..].try_into().unwrap()) ^ u64::from_le_bytes(*b"Y000.000");
    if (x | y) & 0xf0f0_f0ff_f0f0_f0ff != 0 {
        return None;
    }
    let six = 0x0606_0600_0606_0600;
    if ((x + six) | (y + six)) & 0x1010_1000_1010_1000 != 0 {
        return None;
    }
    // Each word now holds the eight byte values [0,a,b,c,0,d,e,f].
    // Sum adjacent digits in 16-bit lanes, then triples in 32-bit lanes.
    Some((
        [x, y].map(|n| {
            let pairs = (n * 10 + (n >> 8)) & 0x00ff_00ff_00ff_00ff;
            let triples = (pairs * 100 + (pairs >> 16)) & 0x0000_ffff_0000_ffff;
            f64::from((triples as u32) * 1000 + (triples >> 32) as u32) / 1000.
        }),
        tail,
    ))
}

fn decimal(mut bytes: &[u8]) -> Option<(f64, &[u8])> {
    const POW10: [f64; 16] = [
        1., 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15,
    ];
    let negative = bytes.first() == Some(&b'-');
    if negative {
        bytes = &bytes[1..];
    }
    let mut i = 0;
    let mut mantissa = 0i64;
    while i < bytes.len() && bytes[i].is_ascii_digit() && i < 15 {
        mantissa = mantissa * 10 + i64::from(bytes[i] - b'0');
        i += 1;
    }
    let mut fraction = 0;
    let mut digits = i;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() && i <= 15 {
            mantissa = mantissa * 10 + i64::from(bytes[i] - b'0');
            i += 1;
        }
        fraction = i - start;
        digits += fraction;
    }
    if digits == 0 || bytes.get(i).is_some_and(u8::is_ascii_digit) {
        return None;
    }
    let value = mantissa as f64 / POW10[fraction];
    (value <= MAX_NUMBER).then(|| (if negative { -value } else { value }, &bytes[i..]))
}

#[derive(Clone, PartialEq)]
struct State {
    pos: [f64; 3],
    e: f64,
    feed: f64,
    absolute: bool,
    absolute_e: bool,
    absolute_arc: bool,
    unit: f64,
    plane: [usize; 3],
    fan: f64,
    temp: f64,
    feature: ExtrusionType,
    motion: Option<u32>,
    skipped_block_indent: Option<usize>,
    filament_area: f64,
    volumetric_e: bool,
}
impl State {
    fn extrusion_scale(&self) -> f64 {
        if self.volumetric_e {
            self.unit.powi(3)
        } else {
            self.unit
        }
    }
}
impl Default for State {
    fn default() -> Self {
        Self {
            pos: [0.0; 3],
            e: 0.0,
            feed: 0.0,
            absolute: true,
            absolute_e: true,
            absolute_arc: false,
            unit: 1.0,
            plane: [0, 1, 2],
            fan: 0.0,
            temp: 0.0,
            feature: ExtrusionType::Unknown,
            motion: None,
            skipped_block_indent: None,
            filament_area: PI * (1.75_f64 / 2.0).powi(2),
            volumetric_e: false,
        }
    }
}

struct Builder<G> {
    geometry: G,
    z: f64,
    index: usize,
    group_has_lines: bool,
    count: usize,
    segment_limit: usize,
    arc_budget_peak: usize,
    bounds: Option<Bounds>,
    diagnostics: Vec<(String, Diagnostic)>,
}
impl<G: Geometry> Builder<G> {
    #[cfg(test)]
    fn new() -> Self
    where
        G: Default,
    {
        Self::with_geometry(G::default())
    }
    fn with_geometry(geometry: G) -> Self {
        Self {
            geometry,
            z: 0.0,
            index: 0,
            group_has_lines: false,
            count: 0,
            segment_limit: MAX_SEGMENTS,
            arc_budget_peak: 0,
            bounds: None,
            diagnostics: vec![],
        }
    }
    fn warn(&mut self, line: usize, key: &str, message: impl Into<String>) {
        if self.diagnostics.len() < MAX_DIAGNOSTICS
            && !self.diagnostics.iter().any(|(k, _)| k == key)
        {
            self.diagnostics.push((
                key.to_owned(),
                Diagnostic {
                    line,
                    message: message.into(),
                },
            ));
        }
    }
    fn change_z(&mut self, z: f64, line: usize) -> Result<(), ParseError> {
        if (z - self.z).abs() > EPS && self.group_has_lines {
            if self.index >= MAX_LAYERS - 1 {
                return Err(error(line, "layer group limit exceeded (1,000,000)"));
            }
            self.geometry.end_layer(self.z);
            self.index += 1;
            self.z = z;
        }
        // The first line may itself move Z. Assign its actual height, fixing
        // upstream's first-line-only incorrect initial Z label.
        if !self.group_has_lines {
            self.z = z;
        }
        Ok(())
    }
    fn push(
        &mut self,
        start: [f64; 3],
        end: [f64; 3],
        e_delta: f64,
        state: &State,
        line: usize,
    ) -> Result<(), ParseError> {
        let distance2 = start
            .iter()
            .zip(end)
            .map(|(a, b)| (b - a).powi(2))
            .sum::<f64>();
        if distance2 <= EPS * EPS {
            return Ok(());
        }
        if self.count >= self.segment_limit {
            return Err(error(line, "segment limit exceeded (5,000,000)"));
        }
        if !start.into_iter().chain(end).all(supported_number) {
            return Err(error(line, "coordinate exceeds supported finite range"));
        }
        let travel = e_delta <= EPS;
        let volume = e_delta
            * if state.volumetric_e {
                1.0
            } else {
                state.filament_area
            };
        let flow = if G::NEEDS_FLOW && !travel && state.feed > 0.0 {
            volume * state.feed / (distance2.sqrt() * 60.0)
        } else {
            0.0
        };
        if !flow.is_finite() {
            return Err(error(line, "volumetric flow overflow"));
        }
        self.geometry.segment(Segment {
            start,
            end,
            extrusion_type: if travel {
                ExtrusionType::Travel
            } else {
                state.feature
            },
            layer_index: self.index,
            is_travel: travel,
            feedrate: state.feed,
            fan_speed: state.fan,
            temperature: state.temp,
            volumetric_flow: flow,
        });
        let bounds = self.bounds.get_or_insert(Bounds {
            min: start,
            max: start,
        });
        bounds.include(start);
        bounds.include(end);
        self.count += 1;
        Ok(())
    }
}

/// Parse UTF-8 G-code. Unknown non-motion M codes are ignored; unsupported
/// geometry-changing commands emit diagnostics. Malformed motion words and
/// impossible arcs return a line-numbered error rather than invented geometry.
pub fn parse_gcode(text: &str) -> Result<ToolpathData, ParseError> {
    parse_detailed(text, None)
}

pub(crate) fn parse_detailed(
    text: &str,
    info: Option<&mut crate::print_info::Collector>,
) -> Result<ToolpathData, ParseError> {
    let p = interpret(text, info, LayerTable::default)?;
    Ok(ToolpathData {
        layers: p.geometry,
        bounds: p.bounds,
        bed_x: p.bed[0],
        bed_y: p.bed[1],
        max_z: p.max_z,
        diagnostics: p.diagnostics,
    })
}

pub(crate) fn parse_features(
    text: &str,
    info: Option<&mut crate::print_info::Collector>,
) -> Result<Print<FeaturePaths>, ParseError> {
    interpret(text, info, FeaturePaths::default)
}

pub(crate) fn parse_preview(
    text: &str,
    info: Option<&mut crate::print_info::Collector>,
    mode: crate::types::ColorMode,
) -> Result<Print<PreviewPaths>, ParseError> {
    interpret(text, info, || PreviewPaths::new(mode))
}

const MAX_DIAGNOSTICS: usize = 128;

fn interpret<G: Geometry>(
    text: &str,
    info: Option<&mut crate::print_info::Collector>,
    section: impl Fn() -> G + Sync,
) -> Result<Print<G>, ParseError> {
    if memchr::memchr(0, text.as_bytes()).is_some() {
        return Err(error(
            0,
            "NUL byte in text G-code (binary input must be decoded first)",
        ));
    }
    let text = text.trim_start_matches('\u{feff}');
    let parse = |mut state| {
        let mut builder = Builder::with_geometry(section());
        parallel::read(text, info, &mut state, &mut builder, &section)?;
        Ok::<_, ParseError>(builder)
    };
    // Only flow depends on the filament diameter. A feature preview can
    // interpret the moves while another worker reads footer settings.
    let (settings, builder) = if !G::NEEDS_FLOW && text.len() >= 1024 * 1024 {
        rayon::join(|| metadata(text), || parse(State::default()))
    } else {
        let settings @ (_, _, _, diameter, _) = metadata(text);
        (
            settings,
            parse(State {
                filament_area: PI * (diameter / 2.).powi(2),
                ..State::default()
            }),
        )
    };
    let (bed_x, bed_y, max_z, _, bed_approximation) = settings;
    let mut builder = builder?;
    let bed_warning = bed_approximation.then(|| Diagnostic {
        line:0,
        message:"A nonrectangular or offset bed_shape is represented by its rectangular width/height anchored at (0, 0).".into()
    });
    if builder.group_has_lines {
        builder.geometry.end_layer(builder.z);
    }
    Ok(Print {
        geometry: builder.geometry,
        segments: builder.count,
        layers: builder.index + usize::from(builder.group_has_lines),
        bed: [bed_x, bed_y],
        max_z,
        bounds: builder.bounds.unwrap_or(Bounds {
            min: [0.0; 3],
            max: [bed_x, bed_y, max_z],
        }),
        diagnostics: bed_warning
            .into_iter()
            .chain(builder.diagnostics.into_iter().map(|(_, d)| d))
            .take(MAX_DIAGNOSTICS)
            .collect(),
    })
}

fn read_lines<G: Geometry>(
    text: &str,
    mut info: Option<&mut crate::print_info::Collector>,
    state: &mut State,
    builder: &mut Builder<G>,
) -> Result<usize, ParseError> {
    let mut words = Words::default();
    let mut lines = 0;
    let mut start = 0;
    let ends = memchr::memchr_iter(b'\n', text.as_bytes())
        .chain((!text.is_empty() && !text.ends_with('\n')).then_some(text.len()));
    for (index, end) in ends.enumerate() {
        let raw = &text[start..end];
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        start = end + 1;
        let line = index + 1;
        lines = line;
        if raw.len() > MAX_LINE_BYTES {
            return Err(error(line, "line exceeds 1 MiB limit"));
        }
        if let Some(info) = &mut info
            && end >= info.body_offset
        {
            info.line(raw);
        }
        if let Some(indent) = state.skipped_block_indent {
            if indentation(raw) > indent {
                continue;
            }
        } else if let Some(motion) = words.plain_motion(raw.as_bytes()) {
            state.motion = Some(motion);
            make_move(state, builder, &words, motion, line)?;
            builder.group_has_lines = true;
            continue;
        }
        let owned;
        let code = if raw.contains(['(', '"']) {
            let (code, comments) = split_comments(raw);
            for comment in comments {
                if let Some(feature) = parse_type_comment(comment) {
                    state.feature = feature;
                }
            }
            owned = code;
            owned.as_str()
        } else if let Some((code, comment)) = raw.split_once(';') {
            if let Some(feature) = parse_type_comment(comment) {
                state.feature = feature;
            }
            code
        } else {
            raw
        };
        let code = code.trim();
        let statement = without_line_number(code);
        if statement.is_empty() || statement == "%" || statement.starts_with('*') {
            builder.group_has_lines = true;
            continue;
        }
        // Only an executable line can end an indented firmware block.
        state.skipped_block_indent = None;
        // These firmware commands accept text or bare flags instead of motion words.
        let first = first_command(statement);
        if matches!(
            first.and_then(|s| s.strip_prefix(['M', 'm'])),
            Some(
                "117"
                    | "118"
                    | "115"
                    | "23"
                    | "28"
                    | "30"
                    | "32"
                    | "486"
                    | "620" // Bambu AMS flags include a bare M (enable remapping).
                    | "862.1"
                    | "862.3"
                    | "862.4"
                    | "862.5"
                    | "862.6"
                    | "1002" // Bambu action names and filament labels are free text.
            )
        ) {
            builder.group_has_lines = true;
            continue;
        }
        if first.is_none()
            && statement
                .as_bytes()
                .first()
                .is_some_and(|c| c.is_ascii_alphabetic())
        {
            let keyword = statement.split([' ', '\t', '{', '"', '(', '*']).next();
            if matches!(keyword, Some("if" | "elif" | "else" | "while")) {
                state.skipped_block_indent = Some(indentation(raw));
                builder.warn(line, "conditional", "RepRapFirmware conditions are not evaluated. Conditional motion and state changes are omitted; subsequent positions may be approximate.");
                builder.group_has_lines = true;
                continue;
            }
            let token = statement.split_whitespace().next().unwrap_or("");
            if token.contains('_') || token.len() > 32 {
                builder.warn(line, "macro", "Named firmware macros are not executed; any motion inside them is absent from this preview.");
                builder.group_has_lines = true;
                continue;
            }
        }
        // Tool-change macros can take named arguments such as PURGE_LENGTH=0.
        let code = match first {
            Some(tool)
                if tool.starts_with(['T', 't']) && named_argument(&statement[tool.len()..]) =>
            {
                tool
            }
            _ => code,
        };
        lex(code, line, &mut words)?;
        let mut has_command = false;
        let mut explicit_motion = false;
        let mut cancelled_motion = false;
        let mut motion_count = 0;
        for command in words
            .items
            .iter()
            .filter(|w| matches!(w.letter, b'G' | b'M'))
        {
            has_command = true;
            let value = command
                .value
                .ok_or_else(|| error(line, "missing command number"))?;
            if command.letter == b'G' {
                let scaled = value * 10.0;
                let command_number = if (scaled - scaled.round()).abs() < EPS {
                    scaled.round() as i64
                } else {
                    -1
                };
                match command_number {
                    n @ (0 | 10 | 20 | 30) if (value * 10.0 - n as f64).abs() < EPS => {
                        motion_count += 1;
                        if motion_count > 1 {
                            return Err(error(
                                line,
                                "multiple motion commands in one block are ambiguous",
                            ));
                        }
                        state.motion = Some((n / 10) as u32);
                        explicit_motion = true;
                    }
                    170 => state.plane = [0, 1, 2],
                    180 => state.plane = [2, 0, 1],
                    190 => state.plane = [1, 2, 0],
                    200 => state.unit = 25.4,
                    210 => state.unit = 1.0,
                    900 => state.absolute = true,
                    910 => state.absolute = false,
                    901 => state.absolute_arc = true,
                    911 => state.absolute_arc = false,
                    920 => {
                        cancelled_motion = true;
                        for (axis, key) in b"XYZ".iter().enumerate() {
                            if let Some(v) = words.get(*key) {
                                state.pos[axis] = v * state.unit;
                            }
                        }
                        if let Some(v) = words.get(b'E') {
                            state.e = v * state.extrusion_scale();
                        }
                        if b"XYZ".iter().any(|k| words.has(*k)) {
                            builder.warn(line, "g92xyz", "G92 XYZ resets displayed programmed coordinates; machine/work-offset transforms are not simulated.");
                        }
                    }
                    280 => {
                        cancelled_motion = true;
                        let all = !b"XYZ".iter().any(|k| words.has(*k));
                        for (axis, key) in b"XYZ".iter().enumerate() {
                            if all || words.has(*key) {
                                state.pos[axis] = 0.0;
                            }
                        }
                        builder.warn(line, "home", "G28 homing assumes the selected axes home to zero; homing travel and machine-specific home offsets are not drawn.");
                    }
                    40 | 940 => cancelled_motion = true,
                    800 => {
                        state.motion = None;
                        cancelled_motion = true;
                    }
                    _ => {
                        cancelled_motion = true;
                        let key = format!("G{value}");
                        builder.warn(line, &key, format!("Unsupported {key}: its geometry/coordinate effects are not simulated."));
                        if matches!(value as u32, 5 | 38 | 73 | 81..=89) {
                            state.motion = None;
                        }
                    }
                }
            } else {
                match value as u32 {
                    82 if value == 82.0 => state.absolute_e = true,
                    83 if value == 83.0 => state.absolute_e = false,
                    106 if value == 106.0 => state.fan = words.get(b'S').unwrap_or(255.0).clamp(0.0,255.0),
                    107 if value == 107.0 => state.fan = 0.0,
                    104 | 109 if value == 104.0 || value == 109.0 => {
                        if let Some(temp) = words.get(b'S').or_else(|| words.get(b'R')) { state.temp = temp; }
                    }
                    200 if value == 200.0 => {
                        if let Some(d) = words.get(b'D') {
                            if d < 0.0 || d > 100.0 { return Err(error(line, "invalid filament diameter in M200")); }
                            state.volumetric_e = d > 0.0;
                            if d > 0.0 { state.filament_area = PI * (d / 2.0).powi(2); }
                        }
                    }
                    206 | 218 | 290 => builder.warn(line, "offset", "Firmware tool/home/babystep offsets are not simulated; the preview uses programmed coordinates."),
                    220 | 221 => builder.warn(line, "override", "Firmware speed/flow overrides are ignored; colors show requested feed rate and extrusion."),
                    163..=165 | 567 => builder.warn(line, "mixing", "Mixing extruder ratios are not simulated."),
                    _ => {}
                }
            }
        }
        if !has_command && words.items.first().is_some_and(|w| w.letter == b'T') {
            builder.warn(line, "tools", "Tool-change motion, return coordinates and tool offsets are not simulated; a shared E accumulator and hotend target are used, matching the upstream viewer.");
            cancelled_motion = true;
        }
        let has_motion_word = b"XYZE".iter().any(|key| words.has(*key));
        let implicit_motion = !has_command && has_motion_word && state.motion.is_some();
        if (explicit_motion || implicit_motion) && !cancelled_motion {
            let motion = state.motion.unwrap_or(1);
            validate_motion_words(&words, line)?;
            if b"ABC".iter().any(|key| words.has(*key)) {
                builder.warn(
                    line,
                    "rotary",
                    "Rotary axes A/B/C are ignored; only XYZ geometry is visualized.",
                );
            }
            make_move(state, builder, &words, motion, line)?;
        } else if !has_command && words.items.iter().all(|w| matches!(w.letter, b'F' | b'N')) {
            if let Some(feed) = words.get(b'F') {
                state.feed = valid_feed(feed * state.unit, line)?;
            }
        } else if !has_command && has_motion_word && !cancelled_motion {
            builder.warn(
                line,
                "orphan",
                "Coordinate words without a preceding G0/G1/G2/G3 modal motion were ignored.",
            );
        }
        builder.group_has_lines = true;
    }
    Ok(lines)
}

fn valid_feed(value: f64, line: usize) -> Result<f64, ParseError> {
    if !(0.0..=MAX_NUMBER).contains(&value) {
        Err(error(line, "feed rate must be a finite nonnegative number"))
    } else {
        Ok(value)
    }
}

fn validate_motion_words(words: &Words, line: usize) -> Result<(), ParseError> {
    for word in &words.items {
        if b"XYZEFRIJK".contains(&word.letter) && word.value.is_none() {
            return Err(error(
                line,
                format!("missing value for motion word {}", char::from(word.letter)),
            ));
        }
    }
    Ok(())
}

fn make_move<G: Geometry>(
    state: &mut State,
    out: &mut Builder<G>,
    words: &Words,
    motion: u32,
    line: usize,
) -> Result<(), ParseError> {
    let start = state.pos;
    let mut end = start;
    for (i, key) in b"XYZ".iter().enumerate() {
        if let Some(value) = words.get(*key) {
            end[i] = value * state.unit + if state.absolute { 0.0 } else { start[i] };
        }
    }
    if !end.into_iter().all(supported_number) {
        return Err(error(line, "coordinate exceeds supported finite range"));
    }
    let next_e = words
        .get(b'E')
        .map(|value| value * state.extrusion_scale() + if state.absolute_e { 0.0 } else { state.e })
        .unwrap_or(state.e);
    if !supported_number(next_e) {
        return Err(error(line, "extrusion exceeds supported finite range"));
    }
    let e_delta = next_e - state.e;
    if let Some(feed) = words.get(b'F') {
        state.feed = valid_feed(feed * state.unit, line)?;
    }
    if motion <= 1 {
        if words.has(b'Z') {
            out.change_z(end[2], line)?;
        }
        out.push(start, end, e_delta, state, line)?;
    } else {
        linearize_arc(state, out, words, start, end, e_delta, motion == 2, line)?;
    }
    state.pos = end;
    state.e = next_e;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn linearize_arc<G: Geometry>(
    state: &State,
    out: &mut Builder<G>,
    words: &Words,
    start: [f64; 3],
    end: [f64; 3],
    e_delta: f64,
    clockwise: bool,
    line: usize,
) -> Result<(), ParseError> {
    let [u, v, w] = state.plane;
    let offsets = b"IJK";
    let has_center = words.has(offsets[u]) || words.has(offsets[v]);
    let center: [f64; 2];
    if let Some(radius) = words.get(b'R').filter(|_| !has_center) {
        let radius = radius * state.unit;
        let dx = end[u] - start[u];
        let dy = end[v] - start[v];
        let half = dx.hypot(dy) * 0.5;
        if half <= EPS {
            return Err(error(
                line,
                "R-format full circles are ambiguous; use I/J/K center offsets",
            ));
        }
        if radius.abs() + 1e-7 < half {
            return Err(error(line, "arc radius is smaller than half its chord"));
        }
        let height = (radius.powi(2) - half.powi(2)).max(0.0).sqrt();
        let side = if clockwise != (radius < 0.0) {
            -1.0
        } else {
            1.0
        };
        center = [
            (start[u] + end[u]) * 0.5 - side * height * dy / (2.0 * half),
            (start[v] + end[v]) * 0.5 + side * height * dx / (2.0 * half),
        ];
    } else if has_center {
        center = [u, v].map(|axis| match words.get(offsets[axis]) {
            Some(value) => value * state.unit + if state.absolute_arc { 0.0 } else { start[axis] },
            None => {
                if state.absolute_arc {
                    0.0
                } else {
                    start[axis]
                }
            }
        });
    } else {
        return Err(error(
            line,
            "arc requires R or I/J/K center offsets for the active plane",
        ));
    }
    let radius = (start[u] - center[0]).hypot(start[v] - center[1]);
    let end_radius = (end[u] - center[0]).hypot(end[v] - center[1]);
    if radius <= EPS {
        return Err(error(line, "arc has a zero radius"));
    }
    if (radius - end_radius).abs() > 0.05_f64.max(radius * 0.01) {
        return Err(error(
            line,
            format!("arc endpoints have inconsistent radii ({radius:.6} and {end_radius:.6} mm)"),
        ));
    }
    let a0 = (start[v] - center[1]).atan2(start[u] - center[0]);
    let a1 = (end[v] - center[1]).atan2(end[u] - center[0]);
    let mut sweep = if clockwise {
        -PlanarArc::phase(a0 - a1)
    } else {
        PlanarArc::phase(a1 - a0)
    };
    if sweep.abs() <= EPS {
        sweep = if clockwise { -TAU } else { TAU };
    }
    if let Some(turns) = words.get(b'P') {
        if turns < 1.0 || turns.fract() != 0.0 || turns > 10000.0 {
            return Err(error(
                line,
                "arc P turns must be an integer from 1 to 10,000",
            ));
        }
        out.warn(line, "arc_p", "Arc P uses a total-turn count (P1 = ordinary arc, P2 = one extra revolution); firmware dialects that count additional circles differ.");
        sweep += if clockwise { -TAU } else { TAU } * (turns - 1.0);
    }
    let planar_length = sweep.abs() * radius;
    let length = if end[w] == start[w] {
        planar_length
    } else {
        planar_length.hypot(end[w] - start[w])
    };
    let steps_f = (length / 0.2)
        .max(sweep.abs() / 5_f64.to_radians())
        .ceil()
        .max(1.0);
    if !steps_f.is_finite() || steps_f > (MAX_SEGMENTS - out.count) as f64 {
        return Err(error(
            line,
            "arc exceeds remaining 5,000,000 segment budget",
        ));
    }
    let steps = steps_f as usize;
    // Arc admission reserves all canonical chords, including any that later
    // turn out to have zero length. Preserve that check when joining sections.
    out.arc_budget_peak = out.arc_budget_peak.max(out.count + steps);
    let mut rotation = None;
    if steps > 1 && w == 2 {
        let arc = PlanarArc {
            center,
            radius,
            end_error: (end_radius - radius).abs(),
            angle: a0,
            sweep,
            steps,
        };
        // Only bulk count chords when all of them move by a representable
        // distance. Degenerate or extreme-coordinate arcs use the exact loop.
        let extent = center[0].abs().max(center[1].abs()) + radius;
        let min_chord = radius.min(end_radius) * (sweep.abs() / steps_f) * 0.9;
        if end[2] == start[2]
            && extent <= MAX_NUMBER
            && min_chord > EPS + extent * f64::EPSILON * 8.0
            && out
                .geometry
                .store_arc(arc, start, end, state.feature, e_delta / steps_f <= EPS)
        {
            if out.count >= out.segment_limit {
                return Err(error(line, "segment limit exceeded (5,000,000)"));
            }
            // A retained arc takes one worker budget slot, regardless of its
            // logical chord count. The file-wide segment limit still applies.
            out.segment_limit = (out.segment_limit + steps - 1).min(MAX_SEGMENTS);
            let bounds = out.bounds.get_or_insert(Bounds {
                min: start,
                max: start,
            });
            bounds.include(start);
            bounds.include(end);
            arc.include_curve(bounds, start[2]);
            out.count += steps;
            out.group_has_lines = true;
            return Ok(());
        }
        // Rotate short runs of radii in the XY plane. Direct angle evaluation
        // every 32 chords bounds drift. Keep the original calculation when
        // coordinate rounding could change whether a chord counts as a move.
        if steps >= 4 && extent * (128. * f64::EPSILON) < EPS && min_chord > 4. * EPS {
            rotation = Some((sweep / steps_f).sin_cos());
        }
    }
    let mut previous = start;
    let mut radial = [start[u] - center[0], start[v] - center[1]];
    for index in 1..=steps {
        let t = index as f64 / steps as f64;
        let point = if index == steps {
            end
        } else {
            radial = match rotation {
                Some((s, c)) if index % 32 != 0 => {
                    [radial[0] * c - radial[1] * s, radial[0] * s + radial[1] * c]
                }
                _ => {
                    let (s, c) = (a0 + sweep * t).sin_cos();
                    [radius * c, radius * s]
                }
            };
            let mut point = start;
            point[u] = center[0] + radial[0];
            point[v] = center[1] + radial[1];
            point[w] = start[w] + (end[w] - start[w]) * t;
            point
        };
        // Match the original's linearize-before-layer-grouping order. Helical
        // arcs therefore create a Z group for every tessellated Z change.
        if (point[2] - previous[2]).abs() > EPS {
            out.change_z(point[2], line)?;
        }
        out.push(previous, point, e_delta / steps as f64, state, line)?;
        out.group_has_lines = true;
        previous = point;
    }
    Ok(())
}

/// Recognizes PrusaSlicer names and common Cura/Orca equivalents.
pub fn parse_type_comment(comment: &str) -> Option<ExtrusionType> {
    let comment = comment.trim().trim_start_matches(';').trim();
    let (key, value) = comment.split_once(':')?;
    let key = key.trim();
    if !key.eq_ignore_ascii_case("TYPE") && !key.eq_ignore_ascii_case("FEATURE") {
        return None;
    }
    use ExtrusionType::*;
    const NAMES: &[(&[&str], ExtrusionType)] = &[
        (
            &[
                "external perimeter",
                "overhang perimeter",
                "wall-outer",
                "outer wall",
                "overhang wall",
            ],
            ExternalPerimeter,
        ),
        (&["perimeter", "wall-inner", "inner wall"], Perimeter),
        (&["infill", "fill", "sparse infill"], Infill),
        (
            &["solid infill", "internal solid infill", "skin"],
            SolidInfill,
        ),
        (&["top solid infill", "top surface"], TopSolidInfill),
        (
            &["bridge infill", "bridge", "internal bridge"],
            BridgeInfill,
        ),
        (
            &[
                "support material",
                "support material interface",
                "support",
                "support-interface",
                "support interface",
            ],
            SupportMaterial,
        ),
        (&["skirt/brim", "skirt", "brim"], SkirtBrim),
        (&["custom"], Custom),
        (&["gap fill"], GapFill),
        (&["ironing"], Ironing),
        (&["wipe tower", "prime-tower", "prime tower"], Wipe),
        (&["travel"], Travel),
    ];
    let value = value.trim();
    Some(
        NAMES
            .iter()
            .find_map(|(names, feature)| {
                names
                    .iter()
                    .any(|s| value.eq_ignore_ascii_case(s))
                    .then_some(*feature)
            })
            .unwrap_or(Unknown),
    )
}

fn split_comments(line: &str) -> (String, Vec<&str>) {
    let mut code = String::with_capacity(line.len());
    let mut comments = vec![];
    let mut depth = 0usize;
    let mut start = 0;
    let mut quoted = false;
    for (index, ch) in line.char_indices() {
        if depth == 0 {
            if ch == '"' {
                quoted = !quoted;
            }
            if ch == ';' && !quoted {
                comments.push(&line[index..]);
                break;
            }
            if ch == '(' && !quoted {
                depth = 1;
                start = index;
                code.push(' ');
            } else {
                code.push(ch);
            }
        } else if ch == '(' {
            depth += 1;
        } else if ch == ')' {
            depth -= 1;
            if depth == 0 {
                comments.push(&line[start + 1..index]);
            }
        }
    }
    if depth != 0 {
        comments.push(&line[start + 1..]);
    }
    (code, comments)
}

fn indentation(raw: &str) -> usize {
    let mut column = 0;
    for byte in raw.bytes() {
        match byte {
            b' ' => column += 1,
            b'\t' => column = (column + 4) & !3, // RepRapFirmware uses four-column tab stops.
            _ => break,
        }
    }
    column
}

fn without_line_number(code: &str) -> &str {
    if let [b'N' | b'n', b'0'..=b'9', ..] = code.as_bytes() {
        code[1..]
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start()
    } else {
        code
    }
}

fn first_command(code: &str) -> Option<&str> {
    if let [b'G' | b'g' | b'M' | b'm' | b'T' | b't', b'0'..=b'9', ..] = code.as_bytes() {
        let end = code[1..].find(|c: char| !c.is_ascii_digit() && c != '.');
        Some(&code[..end.map_or(code.len(), |n| n + 1)])
    } else {
        None
    }
}

fn named_argument(args: &str) -> bool {
    args.strip_prefix([' ', '\t'])
        .and_then(|s| s.split_ascii_whitespace().next())
        .and_then(|s| s.split_once('='))
        .is_some_and(|(key, _)| {
            key.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
                && key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
}

fn lex(code: &str, line: usize, words: &mut Words) -> Result<(), ParseError> {
    let bytes = code.as_bytes();
    words.items.clear();
    words.present = 0;
    let mut i = 0;
    let mut in_g29 = false;
    while i < bytes.len() {
        if bytes[i] == b'*' {
            break;
        } // checksum suffix, not a word
        if bytes[i] == b'"' {
            i += 1;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            i = (i + 1).min(bytes.len());
            continue;
        }
        if !bytes[i].is_ascii_alphabetic() {
            i += 1;
            continue;
        }
        let letter = bytes[i].to_ascii_uppercase();
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        let mut digits = 0;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            digits += 1;
            i += 1;
        }
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                digits += 1;
                i += 1;
            }
        }
        if digits > 0 && i < bytes.len() && bytes[i] == b'e' {
            let mut end = i + 1;
            if end < bytes.len() && matches!(bytes[end], b'+' | b'-') {
                end += 1;
            }
            let exp_start = end;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end > exp_start && bytes.get(end) != Some(&b'.') {
                i = end;
            }
        }
        if digits == 0 {
            // Prusa uses a bare G flag for bed heat soak, often written G29G.
            if in_g29 && letter == b'G' && i == start {
                continue;
            }
            if matches!(letter, b'G' | b'M') {
                return Err(error(
                    line,
                    format!("missing number after {}", char::from(letter)),
                ));
            }
            let tail = code.get(start..).unwrap_or("").to_ascii_lowercase();
            if b"XYZEFRIJK".contains(&letter)
                && (tail.starts_with("nan")
                    || tail.starts_with("inf")
                    || tail.starts_with("+inf")
                    || tail.starts_with("-inf"))
            {
                return Err(error(line, "non-finite numeric word"));
            }
            // Bare axis selectors are valid for G28/M84 etc.; required move
            // values are checked after the command is known.
            words.set(letter, None);
            words.items.push(Word {
                letter,
                value: None,
            });
        } else {
            let value: f64 = code[start..i]
                .parse()
                .map_err(|_| error(line, "invalid numeric word"))?;
            if !supported_number(value) {
                return Err(error(line, "numeric word exceeds supported finite range"));
            }
            if bytes.get(i) == Some(&b'.') {
                return Err(error(
                    line,
                    format!("malformed numeric word {}", &code[start..]),
                ));
            }
            if matches!(letter, b'G' | b'M') {
                in_g29 = letter == b'G' && value == 29.0;
            }
            words.set(letter, Some(value));
            words.items.push(Word {
                letter,
                value: Some(value),
            });
        }
        if words.items.len() > 1024 {
            return Err(error(line, "more than 1,024 words in a line"));
        }
    }
    let is_move = words.items.iter().any(|w| {
        w.letter == b'G'
            && w.value
                .is_some_and(|v| (0.0..=3.0).contains(&v) && v.fract() == 0.0)
    });
    if is_move {
        validate_motion_words(words, line)?;
    }
    Ok(())
}

fn metadata(text: &str) -> (f64, f64, f64, f64, bool) {
    let mut preset = (250.0, 210.0, 220.0);
    let mut bed = None;
    let mut height = None;
    let mut diameter = 1.75;
    let mut bed_approximation = false;
    // Metadata lines either contain an assignment or an M862.3 command.
    // Skip runs of numeric motion records with the vectorized byte search.
    let mut remaining = text;
    while let Some(hit) = memchr::memchr3(b'=', b'M', b'm', remaining.as_bytes()) {
        let start = memchr::memrchr(b'\n', &remaining.as_bytes()[..hit]).map_or(0, |i| i + 1);
        let end = memchr::memchr(b'\n', &remaining.as_bytes()[hit..])
            .map_or(remaining.len(), |i| hit + i);
        let raw = &remaining[start..end];
        remaining = &remaining[(end + 1).min(remaining.len())..];
        if raw.len() > MAX_LINE_BYTES {
            continue;
        }
        let line = raw.trim();
        if line
            .get(..6)
            .is_some_and(|s| s.eq_ignore_ascii_case("M862.3"))
            && let Some(p) = line.find(['P', 'p'])
        {
            let name = line[p + 1..]
                .trim()
                .trim_matches('"')
                .split(['"', ';', ' '])
                .next()
                .unwrap_or("");
            if let Some(dim) = printer_preset(&name.to_ascii_uppercase()) {
                preset = dim;
            }
        }
        let comment = line.trim_start_matches(';').trim();
        if let Some((key, value)) = comment.split_once('=') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            match key.as_str() {
                "bed_shape" | "print_bed_shape" | "printable_area" => {
                    let points: Vec<[f64; 2]> = value
                        .split(',')
                        .filter_map(|p| {
                            let (x, y) = p.trim().split_once('x')?;
                            Some([x.trim().parse().ok()?, y.trim().parse().ok()?])
                        })
                        .collect();
                    if points.len() >= 3
                        && points
                            .iter()
                            .flatten()
                            .all(|v| v.is_finite() && v.abs() <= 1e6)
                    {
                        let minx = points.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
                        let maxx = points
                            .iter()
                            .map(|p| p[0])
                            .fold(f64::NEG_INFINITY, f64::max);
                        let miny = points.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
                        let maxy = points
                            .iter()
                            .map(|p| p[1])
                            .fold(f64::NEG_INFINITY, f64::max);
                        if maxx > minx && maxy > miny {
                            bed = Some((maxx - minx, maxy - miny));
                            bed_approximation = minx.abs() > EPS
                                || miny.abs() > EPS
                                || points.iter().any(|p| {
                                    !((p[0] == minx || p[0] == maxx)
                                        && (p[1] == miny || p[1] == maxy))
                                })
                                || [[minx, miny], [minx, maxy], [maxx, miny], [maxx, maxy]]
                                    .iter()
                                    .any(|corner| !points.contains(corner));
                        }
                    }
                }
                "max_print_height" | "machine_height" | "printable_height" => {
                    if let Ok(v) = value.parse::<f64>()
                        && v.is_finite()
                        && v > 0.0
                        && v <= 1e6
                    {
                        height = Some(v);
                    }
                }
                "filament_diameter" => {
                    if let Ok(v) = value
                        .split([',', ';'])
                        .next()
                        .unwrap_or("")
                        .trim()
                        .parse::<f64>()
                        && v.is_finite()
                        && v > 0.0
                        && v <= 100.0
                    {
                        diameter = v;
                    }
                }
                "printer_model" => {
                    if let Some(dim) = printer_preset(&value.to_ascii_uppercase()) {
                        preset = dim;
                    }
                }
                _ => {}
            }
        }
    }
    let (bx, by) = bed.unwrap_or((preset.0, preset.1));
    (
        bx,
        by,
        height.unwrap_or(preset.2),
        diameter,
        bed_approximation,
    )
}

fn printer_preset(name: &str) -> Option<(f64, f64, f64)> {
    match name {
        "COREONE" | "COREONEHF" | "COREONE+" | "CORE ONE" => Some((250.0, 220.0, 250.0)),
        "COREONEL" | "CORE ONE L" => Some((300.0, 300.0, 330.0)),
        "MK4" | "MK4S" | "MK3.9" | "MK3.9S" => Some((250.0, 210.0, 220.0)),
        "MK3" | "MK3S" | "MK3S+" => Some((250.0, 210.0, 210.0)),
        "MINI" | "MINI+" => Some((180.0, 180.0, 180.0)),
        "XL" | "XL2" | "XL5" => Some((360.0, 360.0, 360.0)),
        _ => None,
    }
}
