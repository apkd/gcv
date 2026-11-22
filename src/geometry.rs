//! Geometry collected from the modal interpreter. Detailed views keep each
//! logical chord; a whole-print feature view only needs visible strokes and counts.
use crate::types::{Bounds, Diagnostic, ExtrusionType, LayerTable, Segment};
mod preview;
pub(crate) use preview::{PreviewLine, PreviewPaths};
mod detailed;
pub(crate) use detailed::{DetailedLayers, DetailedPrint};

// Both consumers use the same modal interpreter and logical segment limits.
// The PNG exporter can retain planar arcs until the projection is known.
pub(crate) trait Geometry: Send {
    const NEEDS_FLOW: bool;
    fn segment(&mut self, segment: Segment);
    fn end_layer(&mut self, z: f64);
    fn append_section(&mut self, part: Self);
    fn store_arc(
        &mut self,
        _arc: PlanarArc,
        _start: [f64; 3],
        _end: [f64; 3],
        _feature: ExtrusionType,
        _travel: bool,
    ) -> bool {
        false
    }
}

impl Geometry for LayerTable {
    const NEEDS_FLOW: bool = true;
    fn segment(&mut self, segment: Segment) {
        self.push(segment);
    }
    fn end_layer(&mut self, z: f64) {
        self.finish(z);
    }
    fn append_section(&mut self, part: Self) {
        self.append(part);
    }
}

#[derive(Clone, Copy)]
#[cfg_attr(test, derive(PartialEq))]
pub(crate) struct PlanarArc {
    pub center: [f64; 2],
    pub radius: f64,
    pub end_error: f64,
    pub angle: f64,
    pub sweep: f64,
    pub steps: usize,
}
impl PlanarArc {
    // Input angles come from atan2 or the four cardinal directions. Most
    // differences already lie within one turn and need no division.
    pub fn phase(delta: f64) -> f64 {
        use std::f64::consts::TAU;
        if delta.abs() < TAU {
            if delta < 0. { delta + TAU } else { delta }
        } else {
            delta.rem_euclid(TAU)
        }
    }

    pub fn point(self, t: f64, z: f64) -> [f64; 3] {
        let (s, c) = (self.angle + self.sweep * t).sin_cos();
        [
            self.center[0] + self.radius * c,
            self.center[1] + self.radius * s,
            z,
        ]
    }

    /// Extend endpoint bounds over the swept circle and its corrected final chord.
    pub fn include_curve(self, bounds: &mut Bounds, z: f64) {
        use std::f64::consts::TAU;
        if (0..2).all(|i| {
            self.center[i] - self.radius >= bounds.min[i]
                && self.center[i] + self.radius <= bounds.max[i]
        }) {
            return;
        }
        // On full turns the four extrema already cover the last chord.
        if self.end_error > 0.0 && self.sweep.abs() < TAU {
            bounds.include(self.point((self.steps - 1) as f64 / self.steps as f64, z));
        }
        for (axis, positive, angle) in [
            (0, true, 0.),
            (1, true, TAU * 0.25),
            (0, false, TAU * 0.5),
            (1, false, TAU * 0.75),
        ] {
            let value = self.center[axis] + if positive { self.radius } else { -self.radius };
            if (positive && value > bounds.max[axis] || !positive && value < bounds.min[axis])
                && Self::phase((angle - self.angle) * self.sweep.signum()) <= self.sweep.abs()
            {
                if positive {
                    bounds.max[axis] = value
                } else {
                    bounds.min[axis] = value
                }
            }
        }
    }
}

pub(crate) struct Stroke {
    // The endpoints are consecutive vertices. Connected strokes share one.
    pub vertex: u16,
    pub feature: ExtrusionType,
    pub arc: Option<std::num::NonZeroU16>,
}
pub(crate) struct FeaturePaths {
    pub batches: Vec<StrokeBatch>,
    pub counts: [usize; 14],
}
#[derive(Default)]
pub(crate) struct StrokeBatch {
    pub strokes: Vec<Stroke>,
    pub arcs: Vec<PlanarArc>,
    vertices: Vertices,
}
// XY changes on most moves. Keep a single exact height for each planar
// block; expand just that block if a Z hop or a helical move changes it.
#[derive(Default)]
struct Vertices {
    xy: Vec<[f64; 2]>,
    blocks: Vec<HeightBlock>,
    z: Vec<f64>,
    last: [f64; 3],
}
struct HeightBlock {
    first: u32,
    // Zero for a flat block; otherwise use the vertex's slot in the block.
    mask: u32,
}
impl Vertices {
    const BLOCK: usize = 64;
    fn endpoints(&self, vertex: u32) -> [[f64; 3]; 2] {
        let i = vertex as usize;
        let &[a, b] = self.xy[i..].first_chunk().unwrap();
        let block = &self.blocks[i / Self::BLOCK];
        let slot = vertex % Self::BLOCK as u32;
        let first = (block.first + (slot & block.mask)) as usize;
        // Mixed blocks store every Z. A flat block advances only when the
        // second endpoint starts the next block.
        let next = first + usize::from((slot | block.mask) == Self::BLOCK as u32 - 1);
        [[a[0], a[1], self.z[first]], [b[0], b[1], self.z[next]]]
    }
    fn point(&mut self, [x, y, z]: [f64; 3]) {
        let slot = self.xy.len() % Self::BLOCK;
        if slot == 0 {
            self.blocks.push(HeightBlock {
                first: self.z.len() as u32,
                mask: 0,
            });
            self.z.push(z);
        } else {
            let b = self.blocks.last_mut().unwrap();
            if b.mask != 0 {
                self.z.push(z);
            } else if self.z[b.first as usize].to_bits() != z.to_bits() {
                self.z
                    .resize(b.first as usize + slot, self.z[b.first as usize]);
                b.mask = Self::BLOCK as u32 - 1;
                self.z.push(z);
            }
        }
        self.xy.push([x, y]);
    }
    fn push(&mut self, start: [f64; 3], end: [f64; 3]) -> u32 {
        if self.xy.is_empty() || self.last.map(f64::to_bits) != start.map(f64::to_bits) {
            self.point(start);
        }
        let vertex = (self.xy.len() - 1) as u32;
        self.point(end);
        self.last = end;
        vertex
    }
}
impl StrokeBatch {
    pub fn endpoints(&self, vertex: u16) -> [[f64; 3]; 2] {
        self.vertices.endpoints(vertex.into())
    }
    pub fn push(
        &mut self,
        start: [f64; 3],
        end: [f64; 3],
        feature: ExtrusionType,
        arc: Option<std::num::NonZeroU16>,
    ) {
        let vertex = self.vertices.push(start, end);
        self.strokes.push(Stroke {
            vertex: vertex as u16,
            feature,
            arc,
        });
    }
}
impl Default for FeaturePaths {
    fn default() -> Self {
        Self {
            batches: vec![StrokeBatch::default()],
            counts: [0; 14],
        }
    }
}
impl FeaturePaths {
    fn batch(&mut self) -> &mut StrokeBatch {
        // Even disconnected strokes need at most two vertices each.
        const MAX_STROKES: usize = 1 << 15;
        if self.batches.last().unwrap().strokes.len() == MAX_STROKES {
            self.batches.push(StrokeBatch::default());
        }
        self.batches.last_mut().unwrap()
    }
    pub fn chunks(&self) -> impl Iterator<Item = (&StrokeBatch, &[Stroke])> {
        self.batches
            .iter()
            .flat_map(|b| b.strokes.chunks(4096).map(move |s| (b, s)))
    }
}
impl Geometry for FeaturePaths {
    const NEEDS_FLOW: bool = false;
    fn end_layer(&mut self, _: f64) {}
    fn append_section(&mut self, part: Self) {
        self.batches.extend(part.batches);
        for (total, n) in self.counts.iter_mut().zip(part.counts) {
            *total += n;
        }
    }
    fn segment(&mut self, s: Segment) {
        if !s.is_travel {
            self.counts[s.extrusion_type as usize] += 1;
            self.batch().push(s.start, s.end, s.extrusion_type, None);
        }
    }
    fn store_arc(
        &mut self,
        arc: PlanarArc,
        start: [f64; 3],
        end: [f64; 3],
        feature: ExtrusionType,
        travel: bool,
    ) -> bool {
        if !travel {
            self.counts[feature as usize] += arc.steps;
            let batch = self.batch();
            batch.arcs.push(arc);
            batch.push(
                start,
                end,
                feature,
                std::num::NonZeroU16::new(batch.arcs.len() as u16),
            );
        }
        true
    }
}

pub(crate) struct Print<G> {
    pub geometry: G,
    pub bounds: Bounds,
    pub bed: [f64; 2],
    pub max_z: f64,
    pub diagnostics: Vec<Diagnostic>,
    pub layers: usize,
    pub segments: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_disconnected_and_helical_paths_preserve_their_endpoints() {
        let mut paths = FeaturePaths::default();
        let points = |i: usize| {
            let x = i as f64 * 2.;
            let z = match i % 341 {
                0..=99 => -0.,
                100..=220 => 0.,
                n => n as f64 / 13.,
            };
            let end_z = if i % 341 > 220 && i.is_multiple_of(7) {
                -z
            } else {
                z
            };
            [[x, 1., z], [x + 1., -3., end_z]]
        };
        for i in 0..80_000 {
            let [start, end] = points(i);
            paths.segment(Segment {
                start,
                end,
                extrusion_type: ExtrusionType::Perimeter,
                layer_index: 0,
                is_travel: false,
                feedrate: 0.,
                fan_speed: 0.,
                temperature: 0.,
                volumetric_flow: 0.,
            });
        }
        let bits = |p: [[f64; 3]; 2]| p.map(|v| v.map(f64::to_bits));
        let actual = paths
            .chunks()
            .flat_map(|(b, strokes)| strokes.iter().map(|s| bits(b.endpoints(s.vertex))));
        assert!(actual.eq((0..80_000).map(|i| bits(points(i)))));
    }
}
