use super::*;

pub(crate) fn gpu() -> Option<eframe::egui_wgpu::RenderState> {
    use eframe::{
        egui_wgpu::{RenderState, WgpuError},
        wgpu,
    };
    static STATE: std::sync::OnceLock<Option<RenderState>> = std::sync::OnceLock::new();
    STATE
        .get_or_init(|| {
            match pollster::block_on(RenderState::create(
                &Default::default(),
                &wgpu::Instance::new(&wgpu::InstanceDescriptor::from_env_or_default()),
                None,
                None,
                1,
                false,
            )) {
                Ok(state) => Some(state),
                Err(WgpuError::NoSuitableAdapterFound(_))
                    if std::env::var_os("GCV_REQUIRE_GPU").is_none_or(|v| v.is_empty()) =>
                {
                    eprintln!("No graphics adapter; skipping GPU viewer test");
                    None
                }
                Err(e) => panic!("GPU initialization: {e}"),
            }
        })
        .clone()
}

fn viewer() -> Option<Gcv> {
    let data = crate::parser::parse_gcode("G90\nG1 Z0.2\nG1 X10 E1\nG1 Z0.4\nG1 X20 E2\n").unwrap();
    Some(Gcv::new(
        gpu()?,
        data,
        PathBuf::from("old.gcode"),
        RenderOptions {
            width: 320,
            height: 240,
            color_mode: ColorMode::Speed,
            show_travel: true,
            ..Default::default()
        },
        1.0,
    ))
}

#[test]
fn render_scale_resizes_the_texture_without_resizing_the_panel() {
    use eframe::{App, egui::pos2};
    let Some(mut app) = viewer() else { return };
    let ctx = egui::Context::default();
    let mut frame = eframe::Frame::_new_kittest();
    let mut draw = |scale| {
        app.render_scale = scale;
        let mut input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                pos2(0., 0.),
                Vec2::new(800., 500.),
            )),
            ..Default::default()
        };
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .native_pixels_per_point = Some(2.);
        let out = ctx.run(input, |ctx| app.update(ctx, &mut frame));
        let panel = out
            .shapes
            .iter()
            .find_map(|s| match &s.shape {
                egui::Shape::Mesh(m) if matches!(m.texture_id, egui::TextureId::User(_)) => {
                    Some(m.calc_bounds())
                }
                _ => None,
            })
            .unwrap();
        (panel, app.viewport.texture_size())
    };
    let (panel, full) = draw(1.);
    let (scaled_panel, half) = draw(0.5);
    assert_eq!(panel, scaled_panel);
    let dimensions = |density: f32| {
        [panel.width(), panel.height()].map(|points| (points * density).round() as u32)
    };
    assert_eq!(full, dimensions(2.0));
    assert_eq!(half, dimensions(1.0));
}

#[test]
fn initial_options_and_keyboard_shortcuts_are_preserved() {
    let ctx = egui::Context::default();
    let Some(mut app) = viewer() else {
        return;
    };
    assert_eq!(app.export_size, [320, 240]);
    assert!(app.options.show_travel);
    assert!(!app.options.show_legend);
    assert_eq!(app.options.color_mode, ColorMode::Speed);
    let mut input = egui::RawInput::default();
    input.events.push(egui::Event::Key {
        key: Key::Num2,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    let _ = ctx.run(input, |ctx| app.shortcuts(ctx));
    assert_eq!(app.options.camera.elevation_deg, 90.0);
    let mut input = egui::RawInput::default();
    input.events.push(egui::Event::Key {
        key: Key::T,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    });
    let _ = ctx.run(input, |ctx| app.shortcuts(ctx));
    assert!(!app.options.show_travel);
}

#[test]
fn file_reload_replaces_geometry_and_resets_layers() {
    let ctx = egui::Context::default();
    let Some(mut app) = viewer() else {
        return;
    };
    let previous = app.model.data.clone();
    let new_data = crate::parser::parse_gcode("G1 X99 E1\n").unwrap();
    app.events_tx
        .send(Event::Loaded(
            PathBuf::from("new.gcode"),
            Ok(Box::new(Model::new(
                &app.viewport.state.device,
                new_data,
                app.options.support_opacity,
            ))),
        ))
        .unwrap();
    app.receive(&ctx);
    assert_eq!(app.filename, PathBuf::from("new.gcode"));
    assert!(!Arc::ptr_eq(&app.model.data, &previous));
    assert!(app.dirty);
    assert_eq!(app.options.layer_max, Some(0));
    assert_eq!(app.options.color_mode, ColorMode::Speed);
}

#[test]
fn failed_reload_keeps_current_model() {
    let ctx = egui::Context::default();
    let Some(mut app) = viewer() else {
        return;
    };
    let original = Arc::clone(&app.model.data);
    app.loading = true;
    app.events_tx
        .send(Event::Loaded(
            PathBuf::from("bad.bgcode"),
            Err("CRC mismatch".into()),
        ))
        .unwrap();
    app.receive(&ctx);
    assert!(Arc::ptr_eq(&original, &app.model.data));
    assert_eq!(app.filename, PathBuf::from("old.gcode"));
    assert!(!app.loading);
    assert!(app.status.contains("CRC mismatch"));
}
