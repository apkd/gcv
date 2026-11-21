//! Simplify connected strokes for camera movement. Original strokes remain
//! available at either end of a layer selection and for the final still frame.
use super::scene::Stroke;
use eframe::wgpu::{self, util::DeviceExt};
use rayon::prelude::*;
use std::ops::Range;

fn upload(device: &wgpu::Device, strokes: &[Stroke]) -> wgpu::Buffer {
    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("toolpath strokes"),
        contents: bytemuck::cast_slice(strokes),
        usage: wgpu::BufferUsages::VERTEX,
    })
}

struct Level {
    error: f32,
    buffer: wgpu::Buffer,
    // Original stroke index at each boundary, including the exclusive end.
    boundaries: Vec<u32>,
}

pub(super) struct Batch {
    full: wgpu::Buffer,
    levels: Vec<Level>,
}

impl Batch {
    pub fn new(
        device: &wgpu::Device,
        strokes: &[Stroke],
        extent: f32,
        object: impl Fn(&Stroke) -> bool + Sync,
    ) -> Self {
        let full = upload(device, strokes);
        let mut levels = Vec::new();
        if strokes.len() >= 4096 && extent > 0. {
            // Build the finer level once; simplify its results for the coarser level.
            // The sum of the two world-space error bounds limits total deviation.
            let levels_cpu: Vec<_> = strokes
                .par_chunks(256)
                .map(|chunk| {
                    let fine = simplify(chunk, extent / 4096., &object);
                    let fine_strokes: Vec<_> = fine.iter().map(|(s, _)| *s).collect();
                    let coarse = simplify(&fine_strokes, extent / 768., &object)
                        .into_iter()
                        .map(|(s, end)| (s, fine[end - 1].1))
                        .collect::<Vec<_>>();
                    [fine, coarse]
                })
                .collect();
            let mut previous = strokes.len();
            for (level, error) in [extent / 4096., extent / 4096. + extent / 768.]
                .into_iter()
                .enumerate()
            {
                let n = levels_cpu
                    .iter()
                    .map(|pair| pair[level].len())
                    .sum::<usize>();
                if n * 4 > previous * 3 {
                    continue;
                }
                let mut reduced = Vec::with_capacity(n);
                let mut boundaries = Vec::with_capacity(n + 1);
                boundaries.push(0);
                for (i, pair) in levels_cpu.iter().enumerate() {
                    for &(s, end) in &pair[level] {
                        reduced.push(s);
                        boundaries.push((i * 256 + end) as u32);
                    }
                }
                levels.push(Level {
                    error,
                    buffer: upload(device, &reduced),
                    boundaries,
                });
                previous = n;
            }
        }
        Self { full, levels }
    }

    pub fn draw(
        &self,
        range: Range<u32>,
        error: f32,
        mut draw: impl FnMut(&wgpu::Buffer, Range<u32>),
    ) {
        if let Some(level) = self.levels.iter().rev().find(|l| l.error <= error) {
            let b = &level.boundaries;
            let first = b.partition_point(|&i| i < range.start);
            let last = b.partition_point(|&i| i <= range.end) - 1;
            if first < last {
                if range.start < b[first] {
                    draw(&self.full, range.start..b[first]);
                }
                draw(&level.buffer, first as u32..last as u32);
                if b[last] < range.end {
                    draw(&self.full, b[last]..range.end);
                }
                return;
            }
        }
        draw(&self.full, range);
    }
}

fn simplify(
    strokes: &[Stroke],
    tolerance: f32,
    object: &impl Fn(&Stroke) -> bool,
) -> Vec<(Stroke, usize)> {
    let mut out = Vec::new();
    let mut start = 0;
    while start < strokes.len() {
        let first = &strokes[start];
        let mut end = start + 1;
        if object(first) {
            while end < strokes.len() {
                let next = &strokes[end];
                if strokes[end - 1].end != next.start
                    || next.paint[0] != first.paint[0]
                    || (1..6).any(|i| next.paint[i].abs_diff(first.paint[i]) > 4)
                {
                    break;
                }
                end += 1;
            }
        }
        reduce(&strokes[start..end], start, tolerance * tolerance, &mut out);
        start = end;
    }
    out
}

fn reduce(s: &[Stroke], base: usize, error2: f32, out: &mut Vec<(Stroke, usize)>) {
    let mut chord = s[0];
    chord.end = s.last().unwrap().end;
    let diff = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let d = diff(chord.end, chord.start);
    let inv = 1. / dot(d, d).max(f32::MIN_POSITIVE);
    let mut worst = error2;
    let mut split = 0;
    for (i, line) in s[..s.len() - 1].iter().enumerate() {
        let v = diff(line.end, chord.start);
        let t = (dot(v, d) * inv).clamp(0., 1.);
        let residual = diff(v, d.map(|v| v * t));
        let deviation = dot(residual, residual);
        if deviation > worst {
            worst = deviation;
            split = i + 1;
        }
    }
    if split == 0 {
        if s.len() > 1 {
            // Keep sharp reversals and shading transitions even on tiny curves.
            let shade =
                super::super::tangent_shade(chord.start.map(f64::from), chord.end.map(f64::from));
            chord.paint[6] = (255. * shade).round() as u8;
            if let Some(i) = s
                .iter()
                .position(|line| line.paint[6].abs_diff(chord.paint[6]) > 24)
            {
                split = i.clamp(1, s.len() - 1);
            }
        }
        if split == 0 {
            out.push((chord, base + s.len()));
            return;
        }
    }
    reduce(&s[..split], base, error2, out);
    reduce(&s[split..], base + split, error2, out);
}
