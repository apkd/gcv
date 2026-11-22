use super::super::{
    Pass, Role,
    framing::index::{FramingIndex, Selection},
    tangent_shade,
};
use super::detail::Batch;
use crate::{
    colors::{ColorRanges, segment_value},
    geometry::DetailedPrint,
    types::{Bounds, ColorMode, Segment},
};
use bytemuck::{Pod, Zeroable};
use eframe::wgpu;
use std::ops::Range;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(super) struct Stroke {
    pub start: [f32; 3],
    pub end: [f32; 3],
    // One palette index per color mode, tangent shade, and stroke kind.
    pub paint: [u8; 8],
}

impl Stroke {
    fn model(segment: &Segment, origin: [f64; 3], ranges: &ColorRanges, pass: Pass) -> Self {
        let mut paint = [0; 8];
        paint[0] = segment.extrusion_type as u8;
        for mode in ColorMode::ALL.into_iter().skip(1) {
            let (lo, hi) = ranges.range(mode);
            let value = if hi > lo {
                (segment_value(segment, mode) - lo) / (hi - lo)
            } else {
                0.5
            };
            paint[mode as usize] = (value.clamp(0., 1.) * 255.).round() as u8;
        }
        paint[6] = if segment.is_travel {
            255
        } else {
            (tangent_shade(segment.start, segment.end) * 255.).round() as u8
        };
        paint[7] = pass as u8;
        Self::new(segment.start, segment.end, origin, paint)
    }

    pub fn new(a: [f64; 3], b: [f64; 3], origin: [f64; 3], paint: [u8; 8]) -> Self {
        let relative = |p: [f64; 3]| std::array::from_fn(|i| (p[i] - origin[i]) as f32);
        Self {
            start: relative(a),
            end: relative(b),
            paint,
        }
    }
}

// Bound upload scratch space and individual GPU allocations independently of file size.
const BATCH: usize = 262_144;
pub(crate) struct Scene {
    pub origin: [f64; 3],
    buffers: [Vec<Batch>; 3],
    pub object_bounds: Bounds,
    pub support_opacity: f64,
    framing: FramingIndex,
    // Exclusive stroke counts for each pass at every layer boundary.
    offsets: Vec<[u32; 3]>,
}

impl Scene {
    pub fn upload(
        device: &wgpu::Device,
        data: &DetailedPrint,
        ranges: &ColorRanges,
        support_opacity: f64,
    ) -> Self {
        let origin = data.bounds.center();
        let mut buffers: [Vec<Batch>; 3] = Default::default();
        let mut staging: [Vec<Stroke>; 3] = Default::default();
        let mut offsets = Vec::with_capacity(data.total_layers() + 1);
        let mut counts = [0u32; 3];
        let framing = FramingIndex::new(&data.layers);
        let (role, object_bounds) = framing.object().unwrap_or((Role::Object, data.bounds));
        let span = object_bounds.size().into_iter().fold(0., f64::max) as f32;
        let upload = |v: &mut Vec<Stroke>, out: &mut Vec<Batch>, pass: usize| {
            // Auxiliary moves can lie beyond the bounds used for the error
            // estimate. Keep those and all travel moves at original resolution.
            out.push(Batch::new(
                device,
                v,
                if pass == Pass::Travel as usize {
                    0.
                } else {
                    span
                },
                |s| Role::of(crate::types::ExtrusionType::ALL[s.paint[0] as usize]) == role,
            ));
            v.clear();
        };
        let mut segments = data.segments();
        for (_, layer) in data.layers.layer_ranges() {
            offsets.push(counts);
            for s in segments.by_ref().take(layer.len()) {
                let pass = Pass::of(s.extrusion_type, s.is_travel, support_opacity < 1.);
                let i = pass as usize;
                counts[i] += 1;
                staging[i].push(Stroke::model(&s, origin, ranges, pass));
                if staging[i].len() == BATCH {
                    upload(&mut staging[i], &mut buffers[i], i);
                }
            }
        }
        offsets.push(counts);
        for (pass, (v, out)) in staging.iter_mut().zip(&mut buffers).enumerate() {
            if !v.is_empty() {
                upload(v, out, pass);
            }
        }
        Self {
            origin,
            buffers,
            object_bounds,
            support_opacity: if counts[Pass::Support as usize] == 0 {
                1.
            } else {
                support_opacity
            },
            framing,
            offsets,
        }
    }

    pub fn draws(
        &self,
        layers: Range<usize>,
        pass: Pass,
    ) -> impl Iterator<Item = (&Batch, Range<u32>)> {
        let pass = pass as usize;
        let start = self.offsets[layers.start][pass] as usize;
        let end = self.offsets[layers.end][pass] as usize;
        let first = start / BATCH;
        self.buffers[pass][first..end.div_ceil(BATCH)]
            .iter()
            .enumerate()
            .filter_map(move |(i, buffer)| {
                let base = (first + i) * BATCH;
                let range = start.saturating_sub(base) as u32..(end - base).min(BATCH) as u32;
                (!range.is_empty()).then_some((buffer, range))
            })
    }

    pub fn selection<'a>(
        &'a self,
        data: &'a DetailedPrint,
        layers: Range<usize>,
        travel: bool,
    ) -> Option<Selection<'a>> {
        let offset = |i: usize| self.offsets[i].iter().sum::<u32>() as usize;
        self.framing.select(
            &data.layers,
            offset(layers.start)..offset(layers.end),
            travel,
        )
    }
}
