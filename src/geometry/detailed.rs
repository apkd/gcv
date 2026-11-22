use super::{Geometry, Vertices};
use crate::types::{ExtrusionType, Segment, ToolpathData};
use std::ops::Range;

pub(crate) type DetailedPrint = ToolpathData<DetailedLayers>;

// Keep flow per chord: even adjacent chords can have different exact values.
struct Chord {
    vertex: u32,
    settings: u32,
    flow: f64,
}

#[derive(PartialEq)]
struct Settings {
    feature: ExtrusionType,
    travel: bool,
    layer: usize,
    // Compare bits so a change from -0 to +0 is preserved.
    values: [u64; 3],
}

#[derive(Default)]
struct Batch {
    first: usize,
    layer_offset: usize,
    vertices: Vertices,
    chords: Vec<Chord>,
    settings: Vec<Settings>,
}

impl Batch {
    fn segment(&self, c: &Chord) -> Segment {
        let [start, end] = self.vertices.endpoints(c.vertex);
        let s = &self.settings[c.settings as usize];
        let [feedrate, fan_speed, temperature] = s.values.map(f64::from_bits);
        Segment {
            start,
            end,
            extrusion_type: s.feature,
            is_travel: s.travel,
            layer_index: self.layer_offset + s.layer,
            feedrate,
            fan_speed,
            temperature,
            volumetric_flow: c.flow,
        }
    }
}

#[derive(Clone, Copy)]
#[cfg(feature = "gui")]
pub(crate) struct Chunk<'a>(&'a Batch, &'a [Chord]);
#[cfg(feature = "gui")]
impl<'a> Chunk<'a> {
    pub fn segments(self) -> impl ExactSizeIterator<Item = Segment> + 'a {
        self.1.iter().map(move |c| self.0.segment(c))
    }
}

#[derive(Default)]
pub(crate) struct DetailedLayers {
    batches: Vec<Batch>,
    ends: Vec<(f64, usize)>,
    len: usize,
}

impl DetailedLayers {
    pub fn layer_ranges(&self) -> impl ExactSizeIterator<Item = (f64, Range<usize>)> + '_ {
        self.ends
            .iter()
            .enumerate()
            .map(|(i, &(z, end))| (z, self.start(i)..end))
    }
    fn start(&self, i: usize) -> usize {
        i.checked_sub(1).map_or(0, |i| self.ends[i].1)
    }
    #[cfg(feature = "gui")]
    pub fn height(&self, i: usize) -> Option<f64> {
        self.ends.get(i).map(|e| e.0)
    }
    #[cfg(feature = "gui")]
    pub fn chunks(&self, layers: Range<usize>) -> impl Iterator<Item = Chunk<'_>> {
        let raw = self.start(layers.start)..self.start(layers.end);
        let end = self.batches.partition_point(|b| b.first < raw.end);
        let batches = &self.batches[..end];
        let start = batches.partition_point(|b| b.first + b.chords.len() <= raw.start);
        batches[start..].iter().map(move |b| {
            Chunk(
                b,
                &b.chords
                    [raw.start.saturating_sub(b.first)..(raw.end - b.first).min(b.chords.len())],
            )
        })
    }
    #[cfg(feature = "gui")]
    pub fn endpoints(
        &self,
        indices: impl Iterator<Item = usize>,
    ) -> impl Iterator<Item = [[f64; 3]; 2]> {
        // Framing visits neighboring stroke indices. Locate each batch once.
        let mut batch: Option<&Batch> = None;
        indices.map(move |i| {
            let b = match batch {
                Some(b) if (b.first..b.first + b.chords.len()).contains(&i) => b,
                _ => &self.batches[self.batches.partition_point(|b| b.first <= i) - 1],
            };
            batch = Some(b);
            b.vertices.endpoints(b.chords[i - b.first].vertex)
        })
    }
    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        self.batches
            .iter()
            .flat_map(|b| b.chords.iter().map(|c| b.segment(c)))
    }
}

impl DetailedPrint {
    pub(crate) fn total_layers(&self) -> usize {
        self.layers.ends.len()
    }
    pub(crate) fn total_segments(&self) -> usize {
        self.layers.len
    }
    pub(crate) fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        self.layers.segments()
    }
}

impl Geometry for DetailedLayers {
    const NEEDS_FLOW: bool = true;
    fn segment(&mut self, s: Segment) {
        if self.batches.last().is_none_or(|b| b.chords.len() == 4096) {
            self.batches.push(Batch {
                first: self.len,
                ..Batch::default()
            });
        }
        let b = self.batches.last_mut().unwrap();
        let settings = Settings {
            feature: s.extrusion_type,
            travel: s.is_travel,
            layer: s.layer_index - b.layer_offset,
            values: [s.feedrate, s.fan_speed, s.temperature].map(f64::to_bits),
        };
        if b.settings.last() != Some(&settings) {
            b.settings.push(settings);
        }
        b.chords.push(Chord {
            vertex: b.vertices.push(s.start, s.end),
            settings: (b.settings.len() - 1) as u32,
            flow: s.volumetric_flow,
        });
        self.len += 1;
    }
    fn end_layer(&mut self, z: f64) {
        self.ends.push((z, self.len));
    }
    fn append_section(&mut self, part: Self) {
        let layers = self.ends.len();
        self.batches.extend(part.batches.into_iter().map(|mut b| {
            b.first += self.len;
            b.layer_offset += layers;
            b
        }));
        self.ends
            .extend(part.ends.into_iter().map(|(z, i)| (z, self.len + i)));
        self.len += part.len;
    }
}

#[cfg(feature = "gui")]
impl From<ToolpathData> for DetailedPrint {
    fn from(data: ToolpathData) -> Self {
        let mut layers = DetailedLayers::default();
        for layer in data.layers.iter() {
            for s in layer.segments {
                layers.segment(s.clone());
            }
            layers.end_layer(layer.z_height);
        }
        Self {
            layers,
            bed_x: data.bed_x,
            bed_y: data.bed_y,
            max_z: data.max_z,
            bounds: data.bounds,
            diagnostics: data.diagnostics,
        }
    }
}
