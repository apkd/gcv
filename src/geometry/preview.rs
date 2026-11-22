//! A detailed PNG needs one color value per chord. Keep endpoints and layer
//! boundaries in their parser sections so joining workers moves no geometry.
use super::{Geometry, Vertices};
use crate::{
    colors::segment_value,
    types::{ColorMode, ExtrusionType, Segment},
};
use std::ops::Range;

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(crate) struct PreviewLine {
    pub start: [f64; 3],
    pub end: [f64; 3],
    pub feature: ExtrusionType,
    pub travel: bool,
    pub value: f64,
}
impl PreviewLine {
    pub fn from_segment(s: &Segment, mode: ColorMode) -> Self {
        Self {
            start: s.start,
            end: s.end,
            feature: s.extrusion_type,
            travel: s.is_travel,
            value: segment_value(s, mode),
        }
    }
}
struct Chord {
    vertex: u32,
    feature: ExtrusionType,
    travel: bool,
    value: f64,
}
#[derive(Default)]
struct Batch {
    vertices: Vertices,
    chords: Vec<Chord>,
    ends: Vec<usize>,
}
#[derive(Clone, Copy)]
pub(crate) struct Chunk<'a>(&'a Vertices, &'a [Chord]);
impl<'a> Chunk<'a> {
    pub fn values(self) -> impl Iterator<Item = (ExtrusionType, bool, f64)> + 'a {
        self.1.iter().map(|c| (c.feature, c.travel, c.value))
    }
    pub fn lines(self) -> impl ExactSizeIterator<Item = PreviewLine> + 'a {
        self.1.iter().map(move |c| {
            let [start, end] = self.0.endpoints(c.vertex);
            PreviewLine {
                start,
                end,
                feature: c.feature,
                travel: c.travel,
                value: c.value,
            }
        })
    }
}
pub(crate) struct PreviewPaths {
    mode: ColorMode,
    batches: Vec<Batch>,
}
impl PreviewPaths {
    pub fn new(mode: ColorMode) -> Self {
        Self {
            mode,
            batches: vec![Batch::default()],
        }
    }

    pub fn chunks(&self, layers: Range<usize>) -> impl Iterator<Item = Chunk<'_>> {
        let mut base = 0;
        self.batches.iter().flat_map(move |b| {
            // The last unfinished layer can continue into the next batch.
            let start = |layer: usize| match layer.saturating_sub(base) {
                0 => 0,
                i => b.ends.get(i - 1).copied().unwrap_or(b.chords.len()),
            };
            let selected = &b.chords[start(layers.start)..start(layers.end)];
            base += b.ends.len();
            selected.chunks(4096).map(|chunk| Chunk(&b.vertices, chunk))
        })
    }
}
impl Geometry for PreviewPaths {
    const NEEDS_FLOW: bool = true;
    fn segment(&mut self, s: Segment) {
        let b = self.batches.last_mut().unwrap();
        let vertex = b.vertices.push(s.start, s.end);
        b.chords.push(Chord {
            vertex,
            feature: s.extrusion_type,
            travel: s.is_travel,
            value: segment_value(&s, self.mode),
        });
    }
    fn end_layer(&mut self, _: f64) {
        let b = self.batches.last_mut().unwrap();
        b.ends.push(b.chords.len());
    }
    fn append_section(&mut self, part: Self) {
        self.batches.extend(part.batches);
    }
}
