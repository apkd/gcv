//! Geometry collected from the modal interpreter. Detailed views keep each
//! logical chord; a whole-print feature view only needs visible strokes and counts.
use crate::types::{Bounds, Diagnostic, ExtrusionType, LayerTable, Segment};

// Both consumers use the same modal interpreter and logical segment limits.
// The PNG exporter can retain planar arcs until the projection is known.
pub(crate) trait Geometry: Default + Send {
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
    pub vertex: u32,
    pub feature: ExtrusionType,
    pub arc: Option<std::num::NonZeroU32>,
}
pub(crate) struct FeaturePaths {
    pub batches: Vec<StrokeBatch>,
    pub counts: [usize; 14],
}
#[derive(Default)]
pub(crate) struct StrokeBatch {
    pub strokes: Vec<Stroke>,
    pub arcs: Vec<PlanarArc>,
    vertices: Vec<[f64; 3]>,
}
impl StrokeBatch {
    pub fn endpoints(&self, vertex: u32) -> &[[f64; 3]; 2] {
        self.vertices[vertex as usize..].first_chunk().unwrap()
    }

    pub fn push(
        &mut self,
        start: [f64; 3],
        end: [f64; 3],
        feature: ExtrusionType,
        arc: Option<std::num::NonZeroU32>,
    ) {
        if !self
            .vertices
            .last()
            .is_some_and(|&p| p.map(f64::to_bits) == start.map(f64::to_bits))
        {
            self.vertices.push(start);
        }
        let vertex = (self.vertices.len() - 1) as u32;
        self.vertices.push(end);
        self.strokes.push(Stroke {
            vertex,
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
            self.batches[0].push(s.start, s.end, s.extrusion_type, None);
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
            let batch = &mut self.batches[0];
            batch.arcs.push(arc);
            batch.push(
                start,
                end,
                feature,
                std::num::NonZeroU32::new(batch.arcs.len() as u32),
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
