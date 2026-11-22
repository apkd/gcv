//! Whole-print feature exports: project compact arcs and paint independent row bands.
use super::{
    Frame, Legend, Pass, Projection, Raster, RenderError, RenderOptions, Role, ScreenPoint,
    Subject, canvas, colors, corners, tangent_color,
};
use crate::geometry::{FeaturePaths, Print, Stroke, StrokeBatch};
use crate::types::{Bounds, ExtrusionType};

pub(crate) fn render_features(
    print: &Print<FeaturePaths>,
    options: &RenderOptions,
    info: Option<&crate::print_info::PrintInfo>,
) -> Result<Frame, RenderError> {
    options.validate()?;
    let geom = &print.geometry;
    let subject = Subject::from_features(geom);
    let legend = Legend::new(options, info, || colors::feature_legend(&geom.counts));
    let (mut raster, p) = canvas(print.bed, options, &subject, legend.as_ref())?;
    let mut tolerance = [arc_tolerance(&p, print.bounds); 2];
    if let Some(bounds) = subject.bounds() {
        tolerance[subject.role as usize] = arc_tolerance(&p, bounds);
    }
    let serial = geom.batches.iter().map(|b| b.strokes.len()).sum::<usize>() < 16_384;
    draw_strokes(&mut raster, &p, geom, tolerance, options, serial)?;
    if let Some(legend) = legend {
        legend.draw(&mut raster);
    }
    Ok(raster.frame)
}

fn draw_strokes(
    raster: &mut Raster,
    p: &Projection,
    geom: &FeaturePaths,
    tolerance: [f64; 2],
    options: &RenderOptions,
    serial: bool,
) -> Result<(), RenderError> {
    let chunks: Vec<_> = geom.chunks().collect();
    let split =
        options.support_opacity < 1. && geom.counts[ExtrusionType::SupportMaterial as usize] != 0;
    for pass in Pass::visible(if split { options.support_opacity } else { 1. }, false) {
        let selected = |s: &&Stroke| Pass::of(s.feature, false, split) == pass;
        let opacity = pass.opacity(options);
        let width = options.line_width;
        if serial {
            for &(b, strokes) in &chunks {
                project_strokes(
                    p,
                    b,
                    strokes.iter().filter(selected),
                    tolerance,
                    |[a, end], ink| {
                        raster.band().line(
                            a,
                            end,
                            || ink.resolve(b),
                            width,
                            opacity,
                            pass == Pass::Opaque,
                        );
                    },
                );
            }
        } else {
            super::bands::draw(
                raster,
                width,
                &chunks,
                |&(geom, chunk), lines| {
                    lines.reserve(chunk.len());
                    project_strokes(
                        p,
                        geom,
                        chunk.iter().filter(selected),
                        tolerance,
                        |pts, ink| lines.push((pts, ink)),
                    );
                    Ok(())
                },
                |band, y, ([mut a, mut b], ink), &(geom, _)| {
                    a.y -= f64::from(y);
                    b.y -= f64::from(y);
                    band.line(
                        a,
                        b,
                        || ink.resolve(geom),
                        width,
                        opacity,
                        pass == Pass::Opaque,
                    );
                },
            )?;
        }
    }
    Ok(())
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

// A straight line can use its source stroke for shading once a pixel passes
// the depth test. Arc chord endpoints only exist here, so shade them now.
#[derive(Clone, Copy)]
enum Ink {
    Straight(u16, crate::types::ExtrusionType),
    Shaded([u8; 3]),
}
impl Ink {
    fn resolve(self, batch: &StrokeBatch) -> [u8; 3] {
        match self {
            Self::Straight(i, f) => {
                let [a, b] = batch.endpoints(i);
                tangent_color(colors::feature_color(f), a, b)
            }
            Self::Shaded(rgb) => rgb,
        }
    }
}
fn project_strokes<'a>(
    p: &Projection,
    geom: &StrokeBatch,
    strokes: impl IntoIterator<Item = &'a Stroke>,
    tolerance: [f64; 2],
    mut draw: impl FnMut([ScreenPoint; 2], Ink),
) {
    let mut last = None;
    for stroke in strokes {
        let color = colors::feature_color(stroke.feature);
        let [start, end] = geom.endpoints(stroke.vertex);
        let mut prev = start;
        let mut screen = match last {
            Some((index, s)) if index == stroke.vertex => s,
            _ => p.project(prev),
        };
        let mut line_to = |end| {
            let next = p.project(end);
            let color = if stroke.arc.is_some() {
                Ink::Shaded(tangent_color(color, prev, end))
            } else {
                Ink::Straight(stroke.vertex, stroke.feature)
            };
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
            let mut i = stride;
            while i < limit {
                line_to(arc.point(i as f64 / arc.steps as f64, start[2]));
                i += stride;
            }
            if corrected {
                line_to(arc.point(limit as f64 / arc.steps as f64, start[2]));
            }
        }
        line_to(end);
        last = Some((stroke.vertex + 1, screen));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{parse_features, parse_gcode};

    fn draw(text: &str, options: &RenderOptions, compact: bool) -> Frame {
        if compact {
            render_features(&parse_features(text, None).unwrap(), options, None).unwrap()
        } else {
            super::super::render(&parse_gcode(text).unwrap(), options).unwrap()
        }
    }

    fn foreground_bounds(frame: &Frame) -> ([u32; 2], [u32; 2]) {
        let mut min = [frame.width, frame.height];
        let mut max = [0, 0];
        for (i, px) in frame.rgba.as_chunks::<4>().0.iter().enumerate() {
            if px[..3] != colors::BACKGROUND {
                let point = [i as u32 % frame.width, i as u32 / frame.width];
                for k in 0..2 {
                    min[k] = min[k].min(point[k]);
                    max[k] = max[k].max(point[k]);
                }
            }
        }
        (min, max)
    }

    #[test]
    fn supports_blend_per_line_in_front_of_the_print() {
        use crate::camera::{Camera, CameraPreset};
        for compact in [false, true] {
            let pixel = |z, repeats, support_opacity| {
                // Emit supports first: the opaque model must still be drawn underneath.
                let support = format!("G0 X0 Z{z}\nG1 X10 E1\n").repeat(repeats);
                let text = format!(
                    "M83\n;TYPE:Support interface\n{support};TYPE:Perimeter\nG0 X0 Z2\nG1 X10 E1"
                );
                let options = RenderOptions {
                    width: 101,
                    height: 81,
                    show_bed: false,
                    show_legend: false,
                    line_width: 4.,
                    support_opacity,
                    camera: Camera {
                        perspective: false,
                        ..Camera::from_preset(CameraPreset::Top)
                    },
                    ..Default::default()
                };
                let frame = draw(&text, &options, compact);
                let i = (40 * 101 + 50) * 4;
                <[u8; 3]>::try_from(&frame.rgba[i..i + 3]).unwrap()
            };
            let model = pixel(3, 1, 0.);
            let support = pixel(3, 1, 1.);
            assert_ne!(model, support);
            assert_eq!(pixel(1, 2, 0.5), model);
            let mut expected = model;
            for repeats in 1..=2 {
                expected = std::array::from_fn(|i| expected[i].midpoint(support[i]));
                let actual = pixel(3, repeats, 0.5);
                assert!((0..3).all(|i| actual[i].abs_diff(expected[i]) <= 2));
            }
        }
    }

    #[test]
    fn bed_view_fits_the_plate_and_print_without_hidden_parking_moves() {
        let options = RenderOptions {
            width: 480,
            height: 390,
            show_legend: false,
            ..Default::default()
        };
        for compact in [false, true] {
            for height in [30, 420] {
                let print =
                    format!("M83\n;TYPE:Perimeter\nG0 X125 Y100 Z1\nG1 X130 E1\nG1 Z{height} E1\n");
                let frame = draw(&print, &options, compact);
                assert!(
                    frame == draw(&(print + "G0 Y290 Z800\n"), &options, compact),
                    "parking changed the fit"
                );
                let (min, max) = foreground_bounds(&frame);
                assert!(
                    min[0] > 0
                        && min[1] > 0
                        && max[0] < frame.width - 1
                        && max[1] < frame.height - 1
                );
                assert!(
                    max[0] - min[0] > frame.width * 9 / 10
                        || max[1] - min[1] > frame.height * 9 / 10
                );
            }
        }
    }

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
            let clean = draw(&format!("M83\n{object}"), &options, compact);
            let with_setup = draw(
                &format!("M83\n{purge}{object}G0 X900 Y300 Z900\n"),
                &options,
                compact,
            );
            assert_eq!(clean, with_setup);
            let (min, max) = foreground_bounds(&clean);
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
        let compact = parse_features(text, None).unwrap();
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
        let print = parse_features(&paths, None).unwrap();
        let options = RenderOptions {
            width: 320,
            height: 240,
            show_legend: false,
            ..Default::default()
        };
        let draw = |threads| {
            let (mut raster, p) = canvas(print.bed, &options, &Subject::default(), None).unwrap();
            if threads == 0 {
                project_strokes(
                    &p,
                    &print.geometry.batches[0],
                    &print.geometry.batches[0].strokes,
                    [0.; 2],
                    |[a, b], c| {
                        raster.line(
                            a,
                            b,
                            c.resolve(&print.geometry.batches[0]),
                            options.line_width,
                            1.,
                            true,
                        )
                    },
                );
            } else {
                rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap()
                    .install(|| {
                        draw_strokes(&mut raster, &p, &print.geometry, [0.; 2], &options, false)
                            .unwrap()
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
