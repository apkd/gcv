//! Geometry collected from the modal interpreter. Detailed views keep each
//! logical chord; a whole-print feature view only needs visible strokes and counts.
use crate::types::{Bounds, Diagnostic, ExtrusionType, LayerTable, Segment};

// Both consumers use the same modal interpreter and logical segment limits.
// The PNG exporter can retain planar arcs until the projection is known.
pub(crate) trait Geometry: Default {
    const NEEDS_FLOW: bool;
    fn segment(&mut self, segment: Segment);
    fn end_layer(&mut self, z: f64);
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
}

#[derive(Clone, Copy)]
pub(crate) struct PlanarArc {
    pub center: [f64; 2],
    pub radius: f64,
    pub end_error: f64,
    pub angle: f64,
    pub sweep: f64,
    pub steps: usize,
}
impl PlanarArc {
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
        if self.end_error > 0.0 {
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
                && ((angle - self.angle) * self.sweep.signum()).rem_euclid(TAU) <= self.sweep.abs()
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
    pub start: [f64; 3],
    pub end: [f64; 3],
    pub feature: ExtrusionType,
    pub arc: Option<std::num::NonZeroU32>,
}
#[derive(Default)]
pub(crate) struct FeaturePaths {
    pub strokes: Vec<Stroke>,
    pub arcs: Vec<PlanarArc>,
    pub counts: [usize; 14],
}
impl Geometry for FeaturePaths {
    const NEEDS_FLOW: bool = false;
    fn end_layer(&mut self, _: f64) {}
    fn segment(&mut self, s: Segment) {
        if !s.is_travel {
            self.counts[s.extrusion_type as usize] += 1;
            self.strokes.push(Stroke {
                start: s.start,
                end: s.end,
                feature: s.extrusion_type,
                arc: None,
            });
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
            self.arcs.push(arc);
            self.strokes.push(Stroke {
                start,
                end,
                feature,
                arc: std::num::NonZeroU32::new(self.arcs.len() as u32),
            });
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
