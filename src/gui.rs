//! Native desktop frontend. Parse and upload replacement models off the UI thread.
use crate::{
    camera::{Camera, CameraPreset},
    geometry::DetailedPrint,
    renderer::{
        self, RenderOptions,
        viewport::{Model, Viewport},
    },
    save_png,
    types::{ColorMode, ToolpathData},
};
use eframe::egui::{self, Color32, Key, PointerButton, RichText, Sense, Vec2};
use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

enum Event {
    Loaded(PathBuf, Result<Box<Model>, String>),
    Screenshot(Result<PathBuf, String>),
}

pub struct Gcv {
    model: Box<Model>,
    viewport: Viewport,
    filename: PathBuf,
    input_path: String,
    options: RenderOptions,
    render_scale: f32,
    export_size: [u32; 2],
    export_legend: bool,
    events_tx: mpsc::Sender<Event>,
    events_rx: mpsc::Receiver<Event>,
    dirty: bool,
    refine_at: Option<Instant>,
    single_layer: bool,
    loading: bool,
    saving: bool,
    status: String,
}

impl Gcv {
    pub fn new(
        gpu: eframe::egui_wgpu::RenderState,
        data: ToolpathData,
        filename: PathBuf,
        options: RenderOptions,
        render_scale: f32,
    ) -> Self {
        Self::from_print(gpu, data.into(), filename, options, render_scale)
    }

    fn from_print(
        gpu: eframe::egui_wgpu::RenderState,
        data: DetailedPrint,
        filename: PathBuf,
        mut options: RenderOptions,
        render_scale: f32,
    ) -> Self {
        let (events_tx, events_rx) = mpsc::channel();
        let export_size = [options.width, options.height];
        let export_legend = options.show_legend;
        options.show_legend = false;
        options.layer_max = Some(
            options
                .layer_max
                .unwrap_or_else(|| data.total_layers().saturating_sub(1)),
        );
        Self {
            model: Box::new(Model::new(&gpu.device, data, options.support_opacity)),
            viewport: Viewport::new(gpu),
            input_path: filename.to_string_lossy().into_owned(),
            filename,
            options,
            render_scale,
            export_size,
            export_legend,
            events_tx,
            events_rx,
            dirty: true,
            refine_at: None,
            single_layer: false,
            loading: false,
            saving: false,
            status: "Drag to orbit · right-drag to pan · scroll to zoom".into(),
        }
    }

    fn set_camera(&mut self, preset: CameraPreset) {
        let perspective = self.options.camera.perspective;
        self.options.camera = Camera::from_preset(preset);
        self.options.camera.perspective = perspective;
        self.dirty = true;
    }

    fn request_motion_frame(&mut self) {
        self.dirty = true;
        self.refine_at = Some(Instant::now() + Duration::from_millis(100));
    }

    fn request_load(&mut self, path: PathBuf, ctx: &egui::Context) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.status = format!("Loading {}…", path.display());
        let device = self.viewport.state.device.clone();
        let sender = self.events_tx.clone();
        let repaint = ctx.clone();
        let opacity = self.options.support_opacity;
        std::thread::spawn(move || {
            let result = crate::load_with(&path, None, crate::parser::parse_compact)
                .map(|data| Box::new(Model::new(&device, data, opacity)))
                .map_err(|e| e.to_string());
            let _ = sender.send(Event::Loaded(path, result));
            repaint.request_repaint();
        });
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        if self.saving {
            return;
        }
        self.saving = true;
        self.status = "Rendering screenshot…".into();
        let data = Arc::clone(&self.model.data);
        let mut options = self.options.clone();
        options.width = self.export_size[0];
        options.height = self.export_size[1];
        options.show_legend = self.export_legend;
        if self.single_layer {
            options.layer_min = options.layer_max.unwrap_or(0);
        }
        let name = self
            .filename
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let sender = self.events_tx.clone();
        let repaint = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<PathBuf, String> {
                let frame =
                    renderer::render_detailed(&data, &options).map_err(|e| e.to_string())?;
                let base = if name.is_empty() { "gcode" } else { &name };
                for suffix in 0..10_000 {
                    let path = PathBuf::from(if suffix == 0 {
                        format!("{base}_screenshot.png")
                    } else {
                        format!("{base}_screenshot_{suffix}.png")
                    });
                    // Reserve a new name atomically, preserving existing screenshots.
                    match std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                    {
                        Ok(file) => {
                            drop(file);
                            if let Err(error) = save_png(&frame, &path) {
                                let _ = std::fs::remove_file(&path);
                                return Err(error.to_string());
                            }
                            return Ok(path);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                        Err(error) => return Err(error.to_string()),
                    }
                }
                Err("no available screenshot filename".into())
            })();
            let _ = sender.send(Event::Screenshot(result));
            repaint.request_repaint();
        });
    }

    fn receive(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                Event::Loaded(path, result) => {
                    self.loading = false;
                    match result {
                        Ok(model) => {
                            self.model = model;
                            self.filename = path;
                            self.input_path = self.filename.to_string_lossy().into_owned();
                            self.options.layer_min = 0;
                            self.options.layer_max =
                                Some(self.model.data.total_layers().saturating_sub(1));
                            self.single_layer = false;
                            self.set_camera(CameraPreset::Isometric);
                            self.status = format!("Loaded {}", self.filename.display());
                            ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                                "gcv — {}",
                                self.filename.display()
                            )));
                        }
                        Err(error) => self.status = format!("Load failed: {error}"),
                    }
                }
                Event::Screenshot(result) => {
                    self.saving = false;
                    self.status = match result {
                        Ok(path) => format!("Screenshot saved: {}", path.display()),
                        Err(error) => format!("Screenshot failed: {error}"),
                    };
                }
            }
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        for (key, preset) in [
            (Key::Num1, CameraPreset::Isometric),
            (Key::Num2, CameraPreset::Top),
            (Key::Num3, CameraPreset::Front),
            (Key::Num4, CameraPreset::Side),
            (Key::R, CameraPreset::Isometric),
        ] {
            if ctx.input(|i| i.key_pressed(key)) {
                self.set_camera(preset);
            }
        }
        if ctx.input(|i| i.key_pressed(Key::T)) {
            self.options.show_travel = !self.options.show_travel;
            self.dirty = true;
        }
        if ctx.input(|i| i.key_pressed(Key::B)) {
            self.options.show_bed = !self.options.show_bed;
            self.dirty = true;
        }
        if ctx.input(|i| i.key_pressed(Key::V)) {
            let index = ColorMode::ALL
                .iter()
                .position(|m| *m == self.options.color_mode)
                .unwrap_or(0);
            self.options.color_mode = ColorMode::ALL[(index + 1) % ColorMode::ALL.len()];
            self.dirty = true;
        }
        if ctx.input(|i| i.key_pressed(Key::S)) {
            self.screenshot(ctx);
        }
    }

    fn controls(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.heading("Toolpath");
        ui.label(format!(
            "{} Z groups · {} segments",
            self.model.data.total_layers(),
            self.model.data.total_segments()
        ));
        ui.label(format!(
            "Bed {:.0} × {:.0} mm",
            self.model.data.bed_x, self.model.data.bed_y
        ));
        ui.separator();
        ui.strong("Camera");
        ui.horizontal_wrapped(|ui| {
            for (label, preset) in [
                ("1 Iso", CameraPreset::Isometric),
                ("2 Top", CameraPreset::Top),
                ("3 Front", CameraPreset::Front),
                ("4 Side", CameraPreset::Side),
            ] {
                if ui.button(label).clicked() {
                    self.set_camera(preset);
                }
            }
        });
        self.dirty |= ui
            .checkbox(&mut self.options.camera.perspective, "Perspective")
            .changed();
        if ui.button("Reset camera  [R]").clicked() {
            self.set_camera(CameraPreset::Isometric);
        }
        ui.separator();
        ui.strong("Layers");
        let last = self.model.data.total_layers().saturating_sub(1);
        let mut max = self.options.layer_max.unwrap_or(last);
        if ui
            .add(egui::Slider::new(&mut max, 0..=last).text("Through"))
            .changed()
        {
            self.options.layer_max = Some(max);
            self.options.layer_min = self.options.layer_min.min(max);
            self.request_motion_frame();
        }
        self.dirty |= ui
            .checkbox(&mut self.single_layer, "Only this layer")
            .changed();
        if !self.single_layer
            && ui
                .add(egui::Slider::new(&mut self.options.layer_min, 0..=max).text("From"))
                .changed()
        {
            self.request_motion_frame();
        }
        if let Some(z) = self.model.data.layers.height(max) {
            ui.label(format!("Z = {z:.3} mm"));
        }
        ui.small("Zero-based Z groups include startup, hops and parking.");
        ui.separator();
        ui.strong("Display");
        let before = self.options.color_mode;
        egui::ComboBox::from_id_salt("color-mode")
            .selected_text(self.options.color_mode.label())
            .show_ui(ui, |ui| {
                for mode in ColorMode::ALL {
                    ui.selectable_value(&mut self.options.color_mode, mode, mode.label());
                }
            });
        let travel_changed = ui
            .checkbox(&mut self.options.show_travel, "Travel moves  [T]")
            .changed();
        if before != self.options.color_mode || travel_changed {
            self.dirty = true;
        }
        self.dirty |= ui
            .checkbox(&mut self.options.show_bed, "Print bed  [B]")
            .changed();
        self.dirty |= ui
            .add(egui::Slider::new(&mut self.options.line_width, 0.5..=5.0).text("Line width"))
            .changed();
        ui.separator();
        ui.strong(self.options.color_mode.label());
        for entry in self
            .model
            .legends
            .get(self.options.color_mode, self.options.show_travel)
        {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(11.0), Sense::hover());
                ui.painter().rect_filled(
                    rect,
                    2.0,
                    Color32::from_rgb(entry.color[0], entry.color[1], entry.color[2]),
                );
                ui.label(&entry.label);
            });
        }
        ui.separator();
        if ui
            .add_enabled(!self.saving, egui::Button::new("Save screenshot  [S]"))
            .clicked()
        {
            self.screenshot(ctx);
        }
        ui.small(format!(
            "{} × {} PNG · current directory",
            self.export_size[0], self.export_size[1]
        ));
        if !self.model.data.diagnostics.is_empty() {
            ui.separator();
            ui.collapsing(
                format!("{} parser diagnostic(s)", self.model.data.diagnostics.len()),
                |ui| {
                    for diagnostic in &self.model.data.diagnostics {
                        ui.label(format!("Line {}: {}", diagnostic.line, diagnostic.message));
                    }
                },
            );
        }
        ui.separator();
        ui.small("Drag: orbit\nRight / middle drag: pan\nScroll: zoom\nV: next color mode\nDrop a file to open it");
    }
}

impl eframe::App for Gcv {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive(ctx);
        self.shortcuts(ctx);
        let dropped = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone()));
        if let Some(path) = dropped {
            self.request_load(path, ctx);
        }
        egui::TopBottomPanel::top("file-bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("gcv")
                        .strong()
                        .color(Color32::from_rgb(255, 165, 90)),
                );
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.input_path)
                        .desired_width((ui.available_width() - 80.0).max(100.0)),
                );
                let enter = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
                if (ui
                    .add_enabled(!self.loading, egui::Button::new("Open"))
                    .clicked()
                    || enter)
                    && !self.input_path.trim().is_empty()
                {
                    self.request_load(PathBuf::from(self.input_path.trim()), ctx);
                }
            });
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.loading || self.saving {
                    ui.spinner();
                }
                ui.label(&self.status);
            });
        });
        egui::SidePanel::left("controls")
            .default_width(275.0)
            .min_width(230.0)
            .resizable(true)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.controls(ui, ctx));
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(Color32::from_rgb(30, 30, 35)))
            .show(ctx, |ui| {
                let size = ui.available_size().max(Vec2::splat(1.0));
                let (rect, response) = ui.allocate_exact_size(size, Sense::drag());
                let before = self.options.camera;
                let delta = ctx.input(|i| i.pointer.delta());
                if response.dragged_by(PointerButton::Primary) && delta != Vec2::ZERO {
                    self.options
                        .camera
                        .orbit(-f64::from(delta.x) * 0.4, f64::from(delta.y) * 0.4);
                }
                if (response.dragged_by(PointerButton::Secondary)
                    || response.dragged_by(PointerButton::Middle))
                    && delta != Vec2::ZERO
                {
                    self.options.camera.pan_by(
                        delta.x as f64,
                        delta.y as f64,
                        size.x as f64,
                        size.y as f64,
                    );
                }
                if response.hovered() {
                    let scroll = ctx.input(|i| i.smooth_scroll_delta.y);
                    if scroll.abs() > 0.01 {
                        self.options.camera.zoom_by((scroll as f64 * 0.002).exp());
                    }
                }
                if response.double_clicked() {
                    self.set_camera(CameraPreset::Isometric);
                }
                let max = self.viewport.state.device.limits().max_texture_dimension_2d as f32;
                let factor =
                    (ctx.pixels_per_point() * self.render_scale).min(max / size.max_elem());
                let width = (size.x * factor).round().max(1.0) as u32;
                let height = (size.y * factor).round().max(1.0) as u32;
                if self.options.width != width || self.options.height != height {
                    self.options.width = width;
                    self.options.height = height;
                    self.dirty = true;
                }
                let mut options = self.options.clone();
                if self.single_layer {
                    options.layer_min = options.layer_max.unwrap_or(0);
                }
                if before != self.options.camera {
                    self.request_motion_frame();
                }
                let now = Instant::now();
                if let Some(deadline) = self.refine_at {
                    if now >= deadline {
                        self.refine_at = None;
                        self.dirty = true;
                    } else {
                        ctx.request_repaint_after(deadline - now);
                    }
                }
                match self.viewport.image(
                    &self.model,
                    &options,
                    self.dirty,
                    self.refine_at.is_some(),
                ) {
                    Ok(id) => {
                        ui.painter().image(
                            id,
                            rect,
                            egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1., 1.)),
                            Color32::WHITE,
                        );
                        self.dirty = false;
                    }
                    Err(e) => {
                        ui.painter().text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            e.to_string(),
                            egui::FontId::default(),
                            Color32::LIGHT_RED,
                        );
                    }
                }
            });
    }
}

#[cfg(test)]
pub(crate) mod tests;

pub fn run_app(
    data: ToolpathData,
    filename: PathBuf,
    options: RenderOptions,
    render_scale: f32,
) -> eframe::Result {
    run_print(data.into(), filename, options, render_scale)
}

pub(crate) fn run_print(
    data: DetailedPrint,
    filename: PathBuf,
    options: RenderOptions,
    render_scale: f32,
) -> eframe::Result {
    let title = format!(
        "gcv — {}",
        filename.file_name().unwrap_or_default().to_string_lossy()
    );
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        &title,
        native,
        Box::new(move |cc| {
            cc.egui_ctx.set_visuals(egui::Visuals::dark());
            Ok(Box::new(Gcv::from_print(
                cc.wgpu_render_state.clone().expect("wgpu renderer"),
                data,
                filename,
                options,
                render_scale,
            )))
        }),
    )
}
