//! Project chunks independently. Paint disjoint rows in source order, including equal depths.
use super::{Band, Raster, RenderError, ScreenPoint};

pub(super) type Line<C> = ([ScreenPoint; 2], C);

pub(super) fn draw<S: Sync, C: Copy + Send + Sync>(
    raster: &mut Raster,
    width: f64,
    chunks: &[S],
    project: impl Fn(&S, &mut Vec<Line<C>>) -> Result<(), RenderError> + Sync,
    paint: impl Fn(&mut Band<'_>, u32, Line<C>, &S) + Sync,
) -> Result<(), RenderError> {
    use rayon::prelude::*;
    const PACKET: usize = 32;
    let w = raster.frame.width as usize;
    let h = raster.frame.height as usize;
    let rows = h.div_ceil(128).max(4);
    let bins = h.div_ceil(rows);
    let shift = rows.trailing_zeros();
    let bin = |row: usize| {
        if rows.is_power_of_two() {
            row >> shift
        } else {
            row / rows
        }
    };
    let radius = width * 0.5;
    let mut scratch: Vec<_> = (0..chunks.len().min(32))
        .map(|_| (Vec::new(), Vec::new()))
        .collect();
    for part in chunks.chunks(32) {
        let batches = &mut scratch[..part.len()];
        batches
            .par_iter_mut()
            .zip(part)
            .try_for_each(|((lines, index), chunk)| {
                lines.clear();
                project(chunk, lines)?;
                let packets = lines.len().div_ceil(PACKET);
                index.clear();
                index.resize(packets * bins, 0u32);
                for (i, ([a, b], _)) in lines.iter().enumerate() {
                    let lo = a.y.min(b.y);
                    let hi = a.y.max(b.y);
                    // Count rows by their pixel centers, as in Band::line.
                    // Keep a rounding margin for clipping in band coordinates.
                    let slack = 16. * f64::EPSILON * (lo.abs() + hi.abs() + h as f64);
                    let first = bin((lo - radius - slack) as usize).min(bins);
                    let end = bin(((hi + radius + 1. + slack) as usize).min(h) + rows - 1);
                    if first < end {
                        let packet = i / PACKET;
                        let bit = 1 << (i % PACKET);
                        for bin in first..end {
                            index[bin * packets + packet] |= bit;
                        }
                    }
                }
                Ok::<_, RenderError>(())
            })?;
        raster
            .frame
            .rgba
            .par_chunks_mut(rows * w * 4)
            .zip(raster.depth.par_chunks_mut(rows * w))
            .enumerate()
            .for_each(|(i, (rgba, depth))| {
                let y = (i * rows) as u32;
                let height = (depth.len() / w) as u32;
                let mut band = Band {
                    width: w as u32,
                    height,
                    rgba,
                    depth,
                };
                for ((lines, index), chunk) in batches.iter().zip(part) {
                    let n = lines.len().div_ceil(PACKET);
                    for (packet, mut mask) in index[i * n..(i + 1) * n].iter().copied().enumerate()
                    {
                        while mask != 0 {
                            let line = lines[packet * PACKET + mask.trailing_zeros() as usize];
                            mask &= mask - 1;
                            paint(&mut band, y, line, chunk);
                        }
                    }
                }
            });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::clip_line;

    #[test]
    fn full_frame_clipping_preserves_depth_and_translucent_line_order() {
        let mut reference = Raster::new(117, 93, [0; 4]).unwrap();
        let mut parallel = Raster::new(117, 93, [0; 4]).unwrap();
        for (width, opacity, write_depth) in [(1.5, 1., true), (0.6, 0.35, false)] {
            let mut lines = Vec::new();
            for i in 0..150 {
                let y = (i * 37 % 123) as f64;
                let z = (i % 7) as f64;
                let a = ScreenPoint {
                    x: -8.1,
                    y,
                    metric: z,
                };
                let b = ScreenPoint {
                    x: 121.9,
                    y: 103. - y,
                    metric: 7. - z,
                };
                let color = [i as u8, 255 - i as u8, 50];
                reference.line(a, b, color, width, opacity, write_depth);
                if let Some(pts) = clip_line(a, b, 117., 93., width * 0.5 + 1.) {
                    lines.push((pts, color));
                }
            }
            let chunks: Vec<_> = lines.chunks(17).collect();
            draw(
                &mut parallel,
                width,
                &chunks,
                |s, lines| {
                    lines.extend_from_slice(s);
                    Ok(())
                },
                |band, y, (pts, color), _| {
                    band.paint(pts, y, || color, width, opacity, write_depth);
                },
            )
            .unwrap();
        }
        assert_eq!(reference.frame.rgba, parallel.frame.rgba);
        assert_eq!(reference.depth, parallel.depth);
    }
}
