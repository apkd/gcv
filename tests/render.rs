use gcv::{
    camera::{Camera, CameraPreset},
    colors::{self, ColorRanges},
    renderer::{RenderOptions, render, validate_dimensions},
    types::{Bounds, ColorMode, ExtrusionType, Segment, ToolpathData},
};

fn segment(start: [f64; 3], end: [f64; 3], feature: ExtrusionType, layer_index: usize) -> Segment {
    Segment {
        start,
        end,
        extrusion_type: feature,
        layer_index,
        is_travel: feature == ExtrusionType::Travel,
        feedrate: 1800.0 + 1800.0 * layer_index as f64,
        fan_speed: 255.0 * layer_index as f64,
        temperature: 200.0 + 20.0 * layer_index as f64,
        volumetric_flow: 3.0 + 3.0 * layer_index as f64,
    }
}

fn sample() -> ToolpathData {
    ToolpathData {
        layers: [
            (
                0.2,
                vec![
                    segment(
                        [10.0, 10.0, 0.2],
                        [90.0, 10.0, 0.2],
                        ExtrusionType::Perimeter,
                        0,
                    ),
                    segment(
                        [90.0, 10.0, 0.2],
                        [90.0, 90.0, 0.2],
                        ExtrusionType::ExternalPerimeter,
                        0,
                    ),
                    segment(
                        [90.0, 90.0, 0.2],
                        [10.0, 90.0, 0.2],
                        ExtrusionType::Travel,
                        0,
                    ),
                ],
            ),
            (
                20.0,
                vec![segment(
                    [10.0, 10.0, 20.0],
                    [90.0, 90.0, 20.0],
                    ExtrusionType::Infill,
                    1,
                )],
            ),
        ]
        .into_iter()
        .collect(),
        bed_x: 100.0,
        bed_y: 100.0,
        max_z: 200.0,
        bounds: Bounds {
            min: [10.0, 10.0, 0.2],
            max: [90.0, 90.0, 20.0],
        },
        diagnostics: vec![],
    }
}

fn options() -> RenderOptions {
    RenderOptions {
        width: 320,
        height: 240,
        show_bed: false,
        show_legend: false,
        camera: Camera {
            perspective: false,
            ..Camera::from_preset(CameraPreset::Top)
        },
        ..RenderOptions::default()
    }
}

fn colored_pixels(frame: &gcv::renderer::Frame) -> usize {
    frame
        .rgba
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|pixel| pixel[..3] != colors::BACKGROUND)
        .count()
}

#[test]
fn camera_presets_have_orthonormal_bases_including_top_pole() {
    for preset in CameraPreset::ALL {
        let camera = Camera::from_preset(preset);
        let basis = camera.basis();
        for axis in basis {
            assert!((axis.into_iter().map(|v| v * v).sum::<f64>() - 1.0).abs() < 1e-12);
        }
        for a in 0..3 {
            for b in a + 1..3 {
                assert!(
                    basis[a]
                        .into_iter()
                        .zip(basis[b])
                        .map(|(a, b)| a * b)
                        .sum::<f64>()
                        .abs()
                        < 1e-12
                );
            }
        }
        assert_eq!(preset.name().parse::<CameraPreset>().unwrap(), preset);
    }
    assert_eq!(
        Camera::from_preset(CameraPreset::Front).basis()[2],
        [0.0, -1.0, 0.0]
    );
    assert!(Camera::from_preset(CameraPreset::Top).basis()[1][1] > 0.999999);
}

#[test]
fn mouse_camera_updates_are_finite_and_reversible() {
    let mut camera = Camera::default();
    camera.orbit(15.0, -10.0);
    camera.pan_by(100.0, -50.0, 1000.0, 500.0);
    camera.zoom_by(2.0);
    assert_eq!(camera.pan, [0.1, -0.1]);
    assert_eq!(camera.zoom, 2.0);
    let saved = camera;
    camera.orbit(f64::NAN, 0.0);
    camera.pan_by(f64::INFINITY, 0.0, 1.0, 1.0);
    camera.zoom_by(-1.0);
    assert_eq!(camera, saved);
    camera.reset(CameraPreset::Top);
    assert_eq!(camera, Camera::from_preset(CameraPreset::Top));
}

#[test]
fn feature_palette_and_gradients_match_upstream_values() {
    assert_eq!(
        colors::feature_color(ExtrusionType::ExternalPerimeter),
        [255, 124, 56]
    );
    assert_eq!(colors::feature_color(ExtrusionType::Travel), [0, 153, 255]);
    assert_eq!(
        colors::gradient_color(ColorMode::Height, -100.0),
        [0, 25, 229]
    );
    assert_eq!(
        colors::gradient_color(ColorMode::Height, 100.0),
        [255, 38, 0]
    );
    assert_eq!(colors::gradient_color(ColorMode::Height, 0.5), [0, 216, 51]);
}

#[test]
fn scalar_ranges_exclude_travel_and_legends_convert_units() {
    let mut toolpath = sample();
    toolpath.layers.segments_mut()[2].feedrate = 999_000.0;
    let ranges = ColorRanges::from_toolpath(&toolpath);
    assert_eq!(ranges.speed, (1800.0, 3600.0));
    assert_eq!(ranges.fan, (0.0, 255.0));
    let speed = colors::legend_entries(&toolpath, ColorMode::Speed, false);
    assert_eq!(
        speed
            .iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>(),
        ["30.0 mm/s", "60.0 mm/s"]
    );
    let fan = colors::legend_entries(&toolpath, ColorMode::Fan, false);
    assert_eq!(
        fan.iter()
            .map(|entry| entry.label.as_str())
            .collect::<Vec<_>>(),
        ["0.0 %", "100.0 %"]
    );
    assert!(
        !colors::legend_entries(&toolpath, ColorMode::Feature, false)
            .iter()
            .any(|entry| entry.label.starts_with("Travel"))
    );
    assert!(
        colors::legend_entries(&toolpath, ColorMode::Feature, true)
            .iter()
            .any(|entry| entry.label.starts_with("Travel"))
    );
}

#[test]
fn constant_gradient_uses_midpoint_and_one_legend_entry() {
    let mut toolpath = sample();
    for segment in toolpath.layers.segments_mut() {
        segment.temperature = 215.0;
    }
    let ranges = ColorRanges::from_toolpath(&toolpath);
    let segment = &toolpath.layers.segments()[0];
    assert_eq!(
        colors::color_for_segment(segment, ColorMode::Temperature, &ranges),
        colors::gradient_color(ColorMode::Temperature, 0.5)
    );
    let entries = colors::legend_entries(&toolpath, ColorMode::Temperature, false);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].label, "215.0 °C");
}

#[test]
fn all_color_modes_and_presets_render_real_opaque_images() {
    let toolpath = sample();
    for mode in ColorMode::ALL {
        for preset in CameraPreset::ALL {
            let settings = RenderOptions {
                color_mode: mode,
                camera: Camera::from_preset(preset),
                ..options()
            };
            let frame = render(&toolpath, &settings).unwrap();
            assert_eq!(frame.rgba.len(), 320 * 240 * 4);
            assert!(colored_pixels(&frame) > 50, "{mode:?} {preset:?}");
            assert!(
                frame
                    .rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| pixel[3] == 255)
            );
        }
    }
}

#[test]
fn layer_filter_travel_and_bed_change_only_requested_scene_content() {
    let toolpath = sample();
    let baseline = render(&toolpath, &options()).unwrap();
    let first_layer = render(
        &toolpath,
        &RenderOptions {
            layer_max: Some(0),
            ..options()
        },
    )
    .unwrap();
    let second_layer = render(
        &toolpath,
        &RenderOptions {
            layer_min: 1,
            ..options()
        },
    )
    .unwrap();
    let empty = render(
        &toolpath,
        &RenderOptions {
            layer_min: 10,
            ..options()
        },
    )
    .unwrap();
    assert!(colored_pixels(&baseline) > colored_pixels(&first_layer));
    assert!(colored_pixels(&second_layer) > 0);
    assert_eq!(colored_pixels(&empty), 0);
    assert_ne!(
        baseline,
        render(
            &toolpath,
            &RenderOptions {
                show_travel: true,
                ..options()
            }
        )
        .unwrap()
    );
    assert_ne!(
        baseline,
        render(
            &toolpath,
            &RenderOptions {
                show_bed: true,
                ..options()
            }
        )
        .unwrap()
    );
}

#[test]
fn export_legends_and_camera_controls_affect_the_pixels() {
    let toolpath = sample();
    let baseline = render(&toolpath, &options()).unwrap();
    assert_ne!(
        baseline,
        render(
            &toolpath,
            &RenderOptions {
                show_legend: true,
                ..options()
            }
        )
        .unwrap()
    );
    let mut settings = options();
    settings.camera.zoom_by(2.0);
    settings.camera.pan_by(30.0, 10.0, 320.0, 240.0);
    assert_ne!(baseline, render(&toolpath, &settings).unwrap());
    settings.camera.pan = [100.0, 100.0];
    assert_eq!(colored_pixels(&render(&toolpath, &settings).unwrap()), 0);
}

#[test]
fn invalid_allocations_and_nonfinite_geometry_are_rejected() {
    for (width, height) in [(0, 1080), (1920, 0), (u32::MAX, 1), (16_384, 16_384)] {
        assert!(validate_dimensions(width, height).is_err());
    }
    assert!(validate_dimensions(7680, 4320).is_ok());
    let mut settings = options();
    settings.camera.zoom = f64::NAN;
    assert!(render(&sample(), &settings).is_err());
    let mut toolpath = sample();
    toolpath.layers.segments_mut()[0].start[0] = f64::INFINITY;
    assert!(render(&toolpath, &options()).is_err());
}

#[test]
fn rendering_is_deterministic_without_window_server_or_gpu() {
    assert_eq!(
        render(&sample(), &options()).unwrap(),
        render(&sample(), &options()).unwrap()
    );
}
