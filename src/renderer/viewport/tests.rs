use super::*;
use crate::{
    camera::{Camera, CameraPreset},
    gui::tests::gpu,
    parser::parse_gcode,
    renderer::render,
};

fn pixels(v: &Viewport) -> Vec<u8> {
    let target = v.target.as_ref().unwrap();
    let [w, h] = target.size;
    let stride = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let buffer = v.state.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("viewport test readback"),
        size: u64::from(stride) * u64::from(h),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut enc = v.state.device.create_command_encoder(&Default::default());
    let sampled = target.sampled_copy.as_ref().unwrap_or(&target.image);
    enc.copy_texture_to_buffer(
        sampled.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride),
                rows_per_image: None,
            },
        },
        sampled.size(),
    );
    v.state.queue.submit([enc.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| tx.send(result).unwrap());
    v.state.device.poll(wgpu::Maintain::Wait);
    rx.recv().unwrap().unwrap();
    buffer
        .slice(..)
        .get_mapped_range()
        .chunks_exact(stride as usize)
        .flat_map(|row| row[..w as usize * 4].iter().copied())
        .collect()
}

#[test]
fn viewport_agrees_with_png_across_display_controls() {
    let Some(state) = gpu() else { return };
    let model = Model::new(
        &state.device,
        parse_gcode(include_str!(
            "../../../tests/fixtures/bgcode/reference.gcode"
        ))
        .unwrap(),
    );
    let mut view = Viewport::new(state);
    let mut o = RenderOptions {
        width: 311,
        height: 225,
        show_legend: false,
        show_bed: false,
        line_width: 3.,
        ..Default::default()
    };
    for (i, mode) in ColorMode::ALL.into_iter().enumerate() {
        o.color_mode = mode;
        o.camera = Camera::from_preset(if i % 2 == 0 {
            CameraPreset::Top
        } else {
            CameraPreset::Isometric
        });
        o.camera.perspective = i % 2 == 0;
        o.show_travel = i % 3 != 0;
        assert_eq!(
            model.legends.get(mode, o.show_travel),
            colors::legend_entries(&model.data, mode, o.show_travel)
        );
        o.show_bed = i == 3;
        if i == 5 {
            o.layer_min = model.data.total_layers() - 1;
        }
        view.image(&model, &o, true, false).unwrap();
        let device = pixels(&view);
        let cpu = render(&model.data, &o).unwrap();
        let mut error = 0u64;
        let mut signal = 0u64;
        for (a, b) in device
            .as_chunks::<4>()
            .0
            .iter()
            .zip(cpu.rgba.as_chunks::<4>().0)
        {
            for c in 0..3 {
                error += u64::from(a[c].abs_diff(b[c]));
                signal += u64::from(b[c].abs_diff(BACKGROUND[c]));
            }
        }
        assert!(
            signal > 0 && error * 10 < signal,
            "{mode:?}: image error {error} / foreground signal {signal}"
        );
    }
}

#[test]
fn short_crossing_strokes_use_centerline_depth() {
    let Some(state) = gpu() else { return };
    let x = |px: f64| 50. + (px - 50.) / 0.92;
    let y = |py: f64| 50. - (py - 50.) / 0.92;
    let mut data=parse_gcode(&format!("G90\nM83\n;TYPE:Perimeter\nG0 X{} Y{} Z2\nG1 X{} Z0 E1\n;TYPE:Infill\nG0 X{} Y{} Z1.6\nG1 Y{} E1",x(50.),y(50.5),x(54.),x(50.5),y(45.),y(55.))).unwrap();
    data.bed_x = 100.;
    data.bed_y = 100.;
    let model = Model::new(&state.device, data);
    let mut v = Viewport::new(state);
    let mut o = RenderOptions {
        width: 100,
        height: 100,
        line_width: 3.,
        show_legend: false,
        camera: Camera::from_preset(CameraPreset::Top),
        ..Default::default()
    };
    o.camera.perspective = false;
    v.image(&model, &o, true, false).unwrap();
    let index = (50 * 100 + 50) * 4;
    let cpu = render(&model.data, &o).unwrap();
    let gpu = pixels(&v);
    for c in index..index + 3 {
        assert!(
            gpu[c].abs_diff(cpu.rgba[c]) < 3,
            "crossing pixel: GPU {:?}, CPU {:?}",
            &gpu[index..index + 3],
            &cpu.rgba[index..index + 3]
        );
    }
}

#[test]
fn thin_diagonal_print_fills_the_same_view_as_the_export() {
    use std::fmt::Write;
    let Some(state) = gpu() else {
        return;
    };
    for count in [256, 8192] {
        let mut text = String::from("G90\nM83\nG0 X0 Y100\n;TYPE:Perimeter\n");
        for i in 1..=count {
            let x = 100. * i as f64 / count as f64;
            writeln!(text, "G1 X{x} Y{} E0.01", 100. - x).unwrap();
        }
        let model = Model::new(&state.device, parse_gcode(&text).unwrap());
        let mut v = Viewport::new(state.clone());
        let mut o = RenderOptions {
            width: 500,
            height: 500,
            show_bed: false,
            show_legend: false,
            ..Default::default()
        };
        o.camera.perspective = false;
        v.image(&model, &o, true, false).unwrap();
        let height = |pixels: &[u8]| {
            pixels
                .as_chunks::<{ 500 * 4 }>()
                .0
                .iter()
                .filter(|row| {
                    row.as_chunks::<4>()
                        .0
                        .iter()
                        .any(|p| p[0].abs_diff(BACKGROUND[0]) > 30)
                })
                .count()
        };
        let export = height(&render(&model.data, &o).unwrap().rgba);
        let viewport = height(&pixels(&v));
        assert!(
            viewport > 0 && viewport.abs_diff(export) <= 2,
            "{count}: {viewport} vs {export}"
        );
    }
}

#[test]
fn hidden_travel_between_separate_islands_does_not_shrink_the_view() {
    let Some(state) = gpu() else { return };
    let mut text = String::from("G90\nM83\n;TYPE:Perimeter\n");
    for (x, y) in [(0, 100), (100, 0)] {
        use std::fmt::Write;
        writeln!(text, "G0 X{x} Y{y}").unwrap();
        text.push_str(&format!("G1 X{} E0.1\nG1 X{x} E0.1\n", x + 1).repeat(1024));
    }
    let model = Model::new(&state.device, parse_gcode(&text).unwrap());
    let mut view = Viewport::new(state);
    let mut options = RenderOptions {
        width: 500,
        height: 500,
        show_legend: false,
        show_bed: false,
        ..Default::default()
    };
    options.camera.perspective = false;
    view.image(&model, &options, true, false).unwrap();
    let span = |rgba: &[u8]| {
        let mut rows = rgba
            .chunks_exact(options.width as usize * 4)
            .enumerate()
            .filter(|(_, row)| {
                row.as_chunks::<4>()
                    .0
                    .iter()
                    .any(|p| p[0].abs_diff(BACKGROUND[0]) > 30)
            })
            .map(|(y, _)| y);
        let first = rows.next().unwrap();
        rows.next_back().unwrap() - first
    };
    let cpu = span(&render(&model.data, &options).unwrap().rgba);
    let gpu = span(&pixels(&view));
    assert!(
        cpu.abs_diff(gpu) <= 2,
        "island span: viewport {gpu}, export {cpu}"
    );
}

#[test]
fn moving_view_keeps_layer_endpoints_and_restores_full_detail() {
    use std::fmt::Write;
    let Some(state) = gpu() else { return };
    let mut gcode = String::from("G90\nM83\n;TYPE:Perimeter\n");
    for i in 1..=8500 {
        let x = i as f64 * 0.01;
        writeln!(
            gcode,
            "G1 X{x:.3} Y{:.4} Z{x:.4} E0.01 F1200",
            (x * 0.2).sin() * 3.
        )
        .unwrap();
    }
    let model = Model::new(&state.device, parse_gcode(&gcode).unwrap());
    let mut v = Viewport::new(state);
    let mut o = RenderOptions {
        width: 640,
        height: 360,
        show_legend: false,
        show_bed: false,
        line_width: 4.,
        layer_min: 102,
        layer_max: Some(7901),
        ..Default::default()
    };
    o.camera = Camera::from_preset(CameraPreset::Top);
    v.image(&model, &o, true, false).unwrap();
    let original = pixels(&v);
    let full = v.drawn_strokes;
    v.image(&model, &o, true, true).unwrap();
    assert!(v.drawn_strokes < full / 2, "moving view did not simplify");
    let moving = pixels(&v);
    let mask = |p: &[u8; 4]| p[0].abs_diff(BACKGROUND[0]) > 40;
    let pairs = original
        .as_chunks::<4>()
        .0
        .iter()
        .zip(moving.as_chunks::<4>().0);
    let changed = pairs.clone().filter(|(a, b)| mask(a) != mask(b)).count();
    let foreground = pairs.filter(|(a, b)| mask(a) || mask(b)).count();
    assert!(
        foreground > 0 && changed * 4 < foreground,
        "moving silhouette: {changed}/{foreground}"
    );
    v.image(&model, &o, true, false).unwrap();
    assert_eq!(pixels(&v), original);
}

// Run with `GCV_BENCH_FILE=<file> cargo test --release orbit_timing -- --ignored --nocapture`.
#[test]
#[ignore = "requires a local printer file and a graphics adapter"]
fn orbit_timing() {
    use std::time::Instant;
    let state = gpu().expect("no graphics adapter");
    let path = std::env::var("GCV_BENCH_FILE").expect("set GCV_BENCH_FILE");
    let parse_started = Instant::now();
    let data = crate::load_toolpath(&path).unwrap();
    let parse = parse_started.elapsed();
    let now = Instant::now();
    let model = Model::new(&state.device, data);
    let upload = now.elapsed();
    let adapter = state.adapter.get_info();
    let mut v = Viewport::new(state);
    let mut o = RenderOptions {
        width: 1280,
        height: 900,
        show_bed: false,
        show_legend: false,
        ..Default::default()
    };
    let mut submit = Vec::new();
    let mut complete = Vec::new();
    let moving = std::env::var_os("GCV_BENCH_FULL").is_none();
    for i in 0..10 {
        o.camera.orbit(5., 0.);
        let start = Instant::now();
        v.image(&model, &o, true, moving).unwrap();
        let sent = start.elapsed();
        v.state.device.poll(wgpu::Maintain::Wait);
        if i >= 3 {
            submit.push(sent);
            complete.push(start.elapsed());
        }
    }
    submit.sort();
    complete.sort();
    println!(
        "{} ({:?}), {path}, {} segments ({} drawn): parse {parse:.2?}, prepare {upload:.2?}, median submit {:.2?}, median complete {:.2?}",
        adapter.name,
        adapter.backend,
        model.data.total_segments(),
        v.drawn_strokes,
        submit[3],
        complete[3]
    );
    if model.data.total_layers() >= 10 {
        let mut frame = Vec::new();
        for i in 1..=7 {
            o.layer_min = i;
            let start = Instant::now();
            v.image(&model, &o, true, moving).unwrap();
            v.state.device.poll(wgpu::Maintain::Wait);
            frame.push(start.elapsed());
        }
        frame.sort();
        println!(
            "median layer selection and render ({} detail): {:.2?}",
            if moving { "moving" } else { "full" },
            frame[3]
        );
    }
}
