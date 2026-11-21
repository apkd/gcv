//! Whole-print feature exports: project compact arcs and paint independent row bands.
use super::{
    Band, Frame, Legend, Projection, Raster, RenderError, RenderOptions, Role, ScreenPoint,
    Subject, canvas, colors, corners, tangent_color,
};
use crate::geometry::{FeaturePaths, Print, Stroke};
use crate::types::Bounds;

pub(crate) fn render_features(
    print: &Print<FeaturePaths>,
    options: &RenderOptions,
) -> Result<Frame, RenderError> {
    options.validate()?;
    let geom = &print.geometry;
    let mut subject = Subject::default();
    if !options.show_bed {
        for s in &geom.strokes {
            subject.include(
                Role::of(s.feature),
                s.start,
                s.end,
                s.arc.map(|i| geom.arcs[i.get() as usize - 1]),
            );
        }
    }
    let legend = Legend::new(options, || colors::feature_legend(&geom.counts));
    let (mut raster, p) = canvas(print.bounds, print.bed, options, &subject, legend.as_ref())?;
    let mut tolerance = [arc_tolerance(&p, print.bounds); 2];
    if let Some(bounds) = subject.bounds() {
        tolerance[subject.role as usize] = arc_tolerance(&p, bounds);
    }
    if geom.strokes.len() < 16_384 {
        project_strokes(&p, geom, &geom.strokes, tolerance, |[a, b], color| {
            raster.line(a, b, color, options.line_width, 1., true)
        });
    } else {
        draw_bands(&mut raster, &p, geom, tolerance, options.line_width);
    }
    if let Some(legend) = legend {
        legend.draw(&mut raster);
    }
    Ok(raster.frame)
}

pub(super) fn arc_tolerance(p: &Projection, bounds: Bounds) -> f64 {
    // Bound the change in screen position per world unit over the whole box,
    // including the change in perspective scale as depth varies.
    if p.perspective {
        let mut near = f64::INFINITY;
        let mut radial = 0f64;
        for point in corners(bounds).map(|pt| p.camera_point(pt)) {
            near = near.min(point[2]);
            radial = radial.max(point[0].hypot(point[1]));
        }
        if near <= p.near {
            0.0
        } else {
            0.25 * near / (p.scale * (1.0 + radial / near))
        }
    } else {
        0.25 / p.scale
    }
}

fn project_strokes(
    p: &Projection,
    geom: &FeaturePaths,
    strokes: &[Stroke],
    tolerance: [f64; 2],
    mut draw: impl FnMut([ScreenPoint; 2], [u8; 3]),
) {
    let mut last = None;
    for stroke in strokes {
        let color = colors::feature_color(stroke.feature);
        let mut prev = stroke.start;
        let mut screen = match last {
            Some((pt, s)) if pt == prev => s,
            _ => p.project(prev),
        };
        let mut line_to = |end| {
            let next = p.project(end);
            let color = tangent_color(color, prev, end);
            if let (Some(a), Some(b)) = (screen, next) {
                draw([a, b], color);
            } else if let Some(points) = p.project_line(prev, end) {
                draw(points, color);
            }
            prev = end;
            screen = next;
        };
        if let Some(index) = stroke.arc {
            let tolerance = tolerance[Role::of(stroke.feature) as usize];
            let arc = geom.arcs[index.get() as usize - 1];
            // A circular span has sagitta <= radius * angle² / 8. Merge only
            // canonical chords, keeping the final radius-corrected endpoint.
            let corrected = arc.end_error > tolerance * 0.5;
            let curve_tolerance = if corrected {
                tolerance
            } else {
                tolerance - arc.end_error
            };
            let max_angle = (8.0 * curve_tolerance / arc.radius)
                .sqrt()
                .min(std::f64::consts::PI);
            let stride = ((max_angle / arc.sweep.abs()) * arc.steps as f64) as usize;
            let stride = stride.clamp(1, arc.steps);
            let limit = arc.steps - usize::from(corrected);
            for i in (stride..limit).step_by(stride) {
                line_to(arc.point(i as f64 / arc.steps as f64, stroke.start[2]));
            }
            if corrected {
                line_to(arc.point(limit as f64 / arc.steps as f64, stroke.start[2]));
            }
        }
        line_to(stroke.end);
        last = Some((stroke.end, screen));
    }
}

// Keep each projected line once. Neighboring moves usually visit the same
// rows, so one bin entry covers a packet of lines. Workers own disjoint rows
// and always see the original stroke order, including at equal depths.
fn draw_bands(
    raster: &mut Raster,
    p: &Projection,
    geom: &FeaturePaths,
    tolerance: [f64; 2],
    width: f64,
) {
    use rayon::prelude::*;
    const PACKET: usize = 32;
    let w = raster.frame.width as usize;
    let h = raster.frame.height as usize;
    let rows = h.div_ceil(128).max(4);
    let bins = h.div_ceil(rows);
    let margin = width * 0.5 + 1.;
    for part in geom.strokes.chunks(131_072) {
        let batches: Vec<_> = part
            .par_chunks(4096)
            .map(|chunk| {
                let mut lines = Vec::with_capacity(chunk.len());
                project_strokes(p, geom, chunk, tolerance, |pts, color| {
                    lines.push((pts, color))
                });
                let mut index = vec![Vec::new(); bins];
                for (i, packet) in lines.chunks(PACKET).enumerate() {
                    let mut lo = f64::INFINITY;
                    let mut hi = f64::NEG_INFINITY;
                    for ([a, b], _) in packet {
                        lo = lo.min(a.y.min(b.y));
                        hi = hi.max(a.y.max(b.y));
                    }
                    let first = ((lo - margin) as usize / rows).min(bins);
                    let end = (((hi + margin) as usize / rows) + 1).min(bins);
                    if first < end {
                        for bin in &mut index[first..end] {
                            bin.push(i);
                        }
                    }
                }
                (lines, index)
            })
            .collect();
        raster
            .frame
            .rgba
            .par_chunks_mut(rows * w * 4)
            .zip(raster.depth.par_chunks_mut(rows * w))
            .enumerate()
            .for_each(|(i, (rgba, depth))| {
                let y = (i * rows) as f64;
                let height = (depth.len() / w) as u32;
                let mut band = Band {
                    width: w as u32,
                    height,
                    rgba,
                    depth,
                };
                for (lines, index) in &batches {
                    for &packet in &index[i] {
                        let start = packet * PACKET;
                        for &([mut a, mut b], color) in
                            &lines[start..(start + PACKET).min(lines.len())]
                        {
                            if a.y.min(b.y) > y + f64::from(height) + margin
                                || a.y.max(b.y) < y - margin
                            {
                                continue;
                            }
                            a.y -= y;
                            b.y -= y;
                            band.line(a, b, color, width, 1., true);
                        }
                    }
                }
            });
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse_features, parse_gcode};

    #[test]
    fn bedless_fit_preserves_the_full_circle_despite_purge_and_park_moves() {
        use crate::camera::{Camera, CameraPreset};
        let object = "G0 X100 Y100 Z1\n;TYPE:External perimeter\nG2 I10 E4\n";
        let purge = ";TYPE:Custom\nG0 X500 Y700\nG1 X850 E1\n";
        let options = RenderOptions {
            width: 480,
            height: 360,
            show_bed: false,
            show_legend: false,
            camera: Camera::from_preset(CameraPreset::Top),
            ..Default::default()
        };
        for compact in [false, true] {
            let draw = |text: &str| {
                if compact {
                    render_features(&parse_features(text).unwrap(), &options).unwrap()
                } else {
                    super::super::render(&parse_gcode(text).unwrap(), &options).unwrap()
                }
            };
            let clean = draw(&format!("M83\n{object}"));
            let with_setup = draw(&format!("M83\n{purge}{object}G0 X900 Y300 Z900\n"));
            assert_eq!(clean, with_setup);
            let mut min = [clean.width, clean.height];
            let mut max = [0, 0];
            for (i, px) in clean.rgba.as_chunks::<4>().0.iter().enumerate() {
                if px[..3] != colors::BACKGROUND {
                    let point = [i as u32 % clean.width, i as u32 / clean.width];
                    for k in 0..2 {
                        min[k] = min[k].min(point[k]);
                        max[k] = max[k].max(point[k]);
                    }
                }
            }
            // A complete centered circle fills most of the shorter dimension.
            for (k, dim) in [clean.width, clean.height].into_iter().enumerate() {
                assert!(max[k] - min[k] > clean.height * 3 / 4);
                assert!(min[k] > 0 && max[k] < dim - 1);
                assert!((max[k] + min[k]).abs_diff(dim - 1) <= 2);
            }
        }
    }

    #[test]
    fn simplified_arcs_keep_original_counts_and_legend() {
        let text = "M83\nG0 X10 Z.2\n;TYPE:Perimeter\nG2 X0 Y10 I-10 P2 E1\n;TYPE:Infill\nG3 X10.001 Y0 I0 J-10 E2\nG2 Z2 I1 P2\nG1 X6 E1";
        let detailed = parse_gcode(text).unwrap();
        let compact = parse_features(text).unwrap();
        assert_eq!(compact.layers, detailed.total_layers());
        assert_eq!(compact.segments, detailed.total_segments());
        assert_eq!(compact.diagnostics, detailed.diagnostics);
        assert_eq!(
            colors::feature_legend(&compact.geometry.counts),
            colors::legend_entries(&detailed, crate::types::ColorMode::Feature, false)
        );
    }

    #[test]
    fn row_bands_preserve_crossing_strokes_and_draw_order() {
        use std::fmt::Write;

        // Two sets of zigzags cross at the same depth across multiple batches.
        let mut paths = String::from("M83\n");
        for (offset, feature) in ["Perimeter", "Infill"].into_iter().enumerate() {
            writeln!(paths, ";TYPE:{feature}").unwrap();
            for y in 0..64 {
                writeln!(paths, "G0 X0 Y{}", y + offset).unwrap();
                for x in 1..=64 {
                    writeln!(paths, "G1 X{x} Y{} E1", y + (x + offset) % 2).unwrap();
                }
            }
        }
        let print = parse_features(&paths).unwrap();
        let options = RenderOptions {
            width: 320,
            height: 240,
            show_legend: false,
            ..Default::default()
        };
        let draw = |threads| {
            let (mut raster, p) =
                canvas(print.bounds, print.bed, &options, &Subject::default(), None).unwrap();
            if threads == 0 {
                project_strokes(
                    &p,
                    &print.geometry,
                    &print.geometry.strokes,
                    [0.; 2],
                    |[a, b], c| raster.line(a, b, c, options.line_width, 1., true),
                );
            } else {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap()
                    .install(|| {
                        draw_bands(
                            &mut raster,
                            &p,
                            &print.geometry,
                            [0.; 2],
                            options.line_width,
                        )
                    });
            }
            raster.frame.rgba
        };
        let parallel = draw(4);
        assert_eq!(parallel, draw(1));
        // Band-edge clipping can round the last bit of a coverage value.
        let error: u64 = parallel
            .iter()
            .zip(draw(0))
            .map(|(a, b)| u64::from(a.abs_diff(b)))
            .sum();
        assert!(
            error < parallel.len() as u64 / 100,
            "band seams: total channel error {error}"
        );
    }
}
