//! Use extrusion resets, layer comments or motion commands to parse sections in
//! parallel. Only accept a section if its initial state matches the preceding
//! section's verified final state. Otherwise replay it from the correct state.
use super::*;
use crate::print_info::Collector;
use rayon::prelude::*;

const CHUNK: usize = 256 * 1024;
const OVERLAP: usize = 8192;
const DETAIL_SECTION_LIMIT: usize = 64 * 1024;
const DETAIL_WAVE: usize = 8;

#[derive(Clone)]
struct Cursor {
    state: State,
    z: f64,
    has_lines: bool,
}

impl Cursor {
    fn at<G>(state: &State, builder: &Builder<G>) -> Self {
        Self {
            state: state.clone(),
            z: builder.z,
            has_lines: builder.group_has_lines,
        }
    }

    fn matches<G: Geometry>(&self, other: &Self) -> bool {
        // Feed, fan, temperature and filament area affect detailed segment
        // attributes only. All other modal state must agree, including signed
        // coordinate zeros, which can affect arc angles.
        let expected = State {
            feed: other.state.feed,
            temp: other.state.temp,
            fan: other.state.fan,
            filament_area: other.state.filament_area,
            ..self.state.clone()
        };
        let attributes = |s: &State| [s.feed, s.temp, s.fan, s.filament_area].map(f64::to_bits);
        (!G::NEEDS_FLOW || attributes(&self.state) == attributes(&other.state))
            && expected == other.state
            && self.state.e.to_bits() == other.state.e.to_bits()
            && expected.pos.map(f64::to_bits) == other.state.pos.map(f64::to_bits)
            && self.z.to_bits() == other.z.to_bits()
            && self.has_lines == other.has_lines
    }

    fn read<G: Geometry>(&mut self, text: &str, limit: usize) -> Option<(Builder<G>, usize)> {
        let mut builder = Builder::new();
        builder.z = self.z;
        builder.group_has_lines = self.has_lines;
        builder.segment_limit = limit;
        let lines = read_lines(text, None, &mut self.state, &mut builder).ok()?;
        self.z = builder.z;
        self.has_lines = builder.group_has_lines;
        Some((builder, lines))
    }
}

pub(super) fn read<G: Geometry>(
    text: &str,
    info: Option<&mut Collector>,
    state: &mut State,
    out: &mut Builder<G>,
) -> Result<usize, ParseError> {
    if text.len() < CHUNK || rayon::current_num_threads() == 1 {
        return read_lines(text, info, state, out);
    }
    let mut parse =
        || parallel(text, state, out).map_or_else(|| read_lines(text, None, state, out), Ok);
    if let Some(info) = info {
        rayon::join(parse, || info.scan(text)).0
    } else {
        parse()
    }
}

fn checkpoints(bytes: &[u8], marker: &[u8], max_stride: usize) -> Vec<(usize, usize)> {
    let stride = (bytes.len() / (2 * rayon::current_num_threads())).clamp(32 * 1024, max_stride);
    let marker = memchr::memmem::Finder::new(marker);
    let mut joins = Vec::new();
    let mut next = 0;
    while next + stride < bytes.len() {
        let Some(pos) = marker.find(&bytes[next..]) else {
            break;
        };
        // A Z word can follow X and Y on the same line. Warm up from the
        // complete command so it establishes the new layer height.
        let at = next + pos;
        let start = if bytes[at] == b'\n' {
            at + 1
        } else {
            memchr::memrchr(b'\n', &bytes[..at]).map_or(0, |n| n + 1)
        };
        let after = (start + OVERLAP).min(bytes.len());
        let Some(newline) = memchr::memchr(b'\n', &bytes[after..]) else {
            break;
        };
        let cut = after + newline + 1;
        joins.push((start, cut));
        next = cut + stride;
    }
    joins
}

fn parallel<G: Geometry>(text: &str, state: &State, out: &mut Builder<G>) -> Option<usize> {
    let bytes = text.as_bytes();
    // Give large, detailed prints more workers without growing the budget
    // for unverified geometry. Small inputs keep the existing section size.
    let width = rayon::current_num_threads()
        .min(bytes.len() / CHUNK)
        .clamp(DETAIL_WAVE, 32);
    let max_stride = if G::NEEDS_FLOW {
        CHUNK * DETAIL_WAVE / width
    } else {
        CHUNK
    };
    let (resets, joins) = [
        (true, b"\nG92 E".as_slice()),
        (true, b"\nG92E"),
        (false, b"\n;LAYER"),
        (false, b" Z"),
        (false, b"\nG1"),
    ]
    .into_iter()
    .map(|(resets, marker)| (resets, checkpoints(bytes, marker, max_stride)))
    .find(|(_, joins)| joins.len() >= 3)?;
    // Split the original segment budget across workers. If a section needs
    // more, the serial parser enforces the file-wide limit and exact error line.
    let budget = |n: usize| (MAX_SEGMENTS as u64 * n as u64 / bytes.len() as u64) as usize;
    let mut seed = Cursor::at(state, out);
    let (mut joined, mut offset) = seed.read::<G>(&text[..joins[0].1], budget(joins[0].1))?;
    if !resets && !seed.state.absolute_e {
        return None;
    }
    // Detailed sections hold every chord. Join a few at a time to bound
    // temporary memory and start each wave with verified machine settings.
    let wave = if G::NEEDS_FLOW { width } else { joins.len() };
    let section_end = |i: usize| joins.get(i + 1).map_or(text.len(), |j| j.1);
    for base in (0..joins.len()).step_by(wave) {
        let cuts = &joins[base..(base + wave).min(joins.len())];
        let available = MAX_SEGMENTS - joined.count;
        let detail_limit = (available.min(DETAIL_WAVE * DETAIL_SECTION_LIMIT) / cuts.len())
            .min(DETAIL_SECTION_LIMIT);
        // Each worker drops its warm-up strokes before building the section.
        let overlap_limit = if G::NEEDS_FLOW {
            detail_limit.min(OVERLAP * 4)
        } else {
            OVERLAP * 4
        };
        let parts: Vec<_> = cuts
            .par_iter()
            .enumerate()
            .map(|(i, &(start, cut))| {
                let mut cursor = seed.clone();
                if i != 0 {
                    if let Some(feature) = text[..start]
                        .rsplit_terminator('\n')
                        .take(2048)
                        .filter(|s| s.starts_with(';'))
                        .find_map(parse_type_comment)
                    {
                        cursor.state.feature = feature;
                    }
                    cursor.read::<FeaturePaths>(&text[start..cut], overlap_limit)?;
                }
                let entry = cursor.clone();
                let end = section_end(base + i);
                let limit = if G::NEEDS_FLOW {
                    detail_limit
                } else {
                    budget(end - cut)
                };
                let part = cursor.read(&text[cut..end], limit)?;
                Some((entry, cursor, part))
            })
            .collect();
        for (i, candidate) in parts.into_iter().enumerate() {
            let remaining = MAX_SEGMENTS - joined.count;
            let (part, lines) = if let Some((entry, end, part)) = candidate
                && seed.matches::<G>(&entry)
            {
                seed = end;
                part
            } else {
                let limit = if G::NEEDS_FLOW {
                    remaining.min(DETAIL_SECTION_LIMIT)
                } else {
                    remaining
                };
                seed.read(&text[cuts[i].1..section_end(base + i)], limit)?
            };
            if part.arc_budget_peak > remaining || part.count > remaining {
                return None;
            }
            joined.index += part.index;
            if joined.index >= MAX_LAYERS {
                return None;
            }
            joined.count += part.count;
            if let Some(b) = part.bounds {
                let bounds = joined.bounds.get_or_insert(b);
                bounds.include(b.min);
                bounds.include(b.max);
            }
            for (key, d) in part.diagnostics {
                joined.warn(offset + d.line, &key, d.message);
            }
            offset += lines;
            joined.geometry.append_section(part.geometry);
        }
    }
    joined.z = seed.z;
    joined.group_has_lines = seed.has_lines;
    *out = joined;
    Some(offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write;

    fn sample(relative: bool) -> String {
        let mut text = String::from(if relative { "M83\n" } else { "M82\n" });
        let moves = "G1 X1 Y2 E1\nG2 I1 E2\nG1 X2 Y3 E3\n".repeat(800);
        for layer in 0..48 {
            if relative {
                text.push_str("G92 E0\n");
            } else {
                writeln!(text, ";LAYER:{layer}").unwrap();
            }
            writeln!(text, "G1 Z{layer}\n;TYPE:Perimeter").unwrap();
            if layer == 17 {
                // A modal change persists across sections. The speculative
                // start no longer agrees; replay must preserve relative XYZ.
                text.push_str("G91\nG38\n");
            }
            if layer == 36 {
                text.push_str("G90\nM206 X1\n");
            }
            text.push_str(&moves);
        }
        text
    }

    #[test]
    fn joined_sections_match_serial_geometry_and_diagnostics() {
        let absolute = sample(false);
        let unmarked = absolute.replace(";LAYER:", "; group ");
        for text in [
            sample(true),
            sample(true).replace("G92 E", "G92E"),
            absolute,
            unmarked.replace("G1 Z", "G1Z"),
            unmarked,
        ] {
            let mut reference = Builder::<FeaturePaths>::new();
            read_lines(&text, None, &mut State::default(), &mut reference).unwrap();
            let mut result = Builder::<FeaturePaths>::new();
            assert!(parallel(&text, &State::default(), &mut result).is_some());
            let strokes = |g: FeaturePaths| {
                g.batches.into_iter().flat_map(|batch| {
                    (0..batch.strokes.len()).map(move |i| {
                        let s = &batch.strokes[i];
                        let [start, end] = *batch.endpoints(s.vertex);
                        (
                            start,
                            end,
                            s.feature,
                            s.arc.map(|i| batch.arcs[i.get() as usize - 1]),
                        )
                    })
                })
            };
            assert_eq!(reference.geometry.counts, result.geometry.counts);
            assert!(strokes(reference.geometry).eq(strokes(result.geometry)));
            assert_eq!(reference.bounds, result.bounds);
            assert_eq!(reference.index, result.index);
            assert_eq!(reference.count, result.count);
            assert_eq!(reference.diagnostics, result.diagnostics);
        }
    }

    #[test]
    fn detailed_sections_preserve_chord_attributes_and_layer_boundaries() {
        let mut text = String::from("M83\nM104 S215\nG1 F1800\n");
        for i in 0..64 {
            writeln!(text, "G92 E0\nG1 Z{}\n;TYPE:Perimeter", i / 3).unwrap();
            match i {
                5 => text.push_str("M106 S127\nM104 S-0\nG38\n"),
                17 => text.push_str("M200 D2.85\nG1 F1200\n"),
                40 => text.push_str("M107\nM200 D0\n"),
                _ => {}
            }
            text.push_str(&"G1 X3 Y2 E1\nG1 X4 Y3 E1\n".repeat(512));
            text.push_str("G3 X3 Y4 I-1 Z0.5 E1\n");
        }
        let mut serial = Builder::<LayerTable>::new();
        let lines = read_lines(&text, None, &mut State::default(), &mut serial).unwrap();
        let mut joined = Builder::<LayerTable>::new();
        assert_eq!(parallel(&text, &State::default(), &mut joined), Some(lines));
        for builder in [&mut serial, &mut joined] {
            builder.geometry.finish(builder.z);
        }
        assert_eq!(serial.geometry, joined.geometry);
        assert!(
            serial
                .geometry
                .segments()
                .iter()
                .zip(joined.geometry.segments())
                .all(|(a, b)| a.temperature.to_bits() == b.temperature.to_bits())
        );
        assert_eq!(serial.bounds, joined.bounds);
        assert_eq!(serial.diagnostics, joined.diagnostics);
    }

    #[test]
    fn late_errors_keep_serial_line_numbers_and_limits() {
        // The final arc has many planned chords but none long enough to draw.
        // Serial parsing still checks their budget before starting that arc.
        let mut text = String::from("M83\nG2 I100 P1450 E10\n");
        let padding = format!("G92 E0\n;{}\n", " ".repeat(CHUNK / 2));
        text.push_str(&padding.repeat(12));
        text.push_str("G2 I.000000002 P10000 E10\n");
        let expected = read_lines(
            &text,
            None,
            &mut State::default(),
            &mut Builder::<FeaturePaths>::new(),
        )
        .unwrap_err();
        assert_eq!(parse_features(&text, None).err().unwrap(), expected);
    }
}
