//! Deterministic PNG renderer and camera/framing shared with the GPU viewport.
//!
//! Lines are clipped before rasterization and use analytic edge coverage and a
//! depth buffer. Export needs neither a GPU, a window server, nor a font install.
use std::{error::Error, fmt};
mod bands;
mod features;
mod framing;
mod summary;
#[cfg(feature = "gui")]
pub(crate) mod viewport;
pub(crate) use features::render_features;
use framing::{Role, Subject};

use crate::{
    camera::Camera,
    colors::{self, BACKGROUND, TRAVEL_OPACITY},
    geometry::{PreviewLine, PreviewPaths, Print},
    types::{Bounds, ColorMode, ExtrusionType, ToolpathData},
};

/// Limits both the RGBA image and the equally sized f32 depth buffer (~256 MiB).
pub const MAX_PIXELS: u64 = 33_554_432;
pub const MAX_DIMENSION: u32 = 16_384;
const OPAQUE_BACKGROUND: [u8; 4] = [BACKGROUND[0], BACKGROUND[1], BACKGROUND[2], 255];

#[derive(Clone, Debug, PartialEq)]
pub struct RenderOptions {
    pub width: u32,
    pub height: u32,
    pub layer_min: usize,
    /// Inclusive maximum; `None` selects the final layer.
    pub layer_max: Option<usize>,
    pub show_travel: bool,
    pub support_opacity: f64,
    pub show_bed: bool,
    /// Leave the background transparent in PNG exports.
    pub transparent: bool,
    pub color_mode: ColorMode,
    pub camera: Camera,
    /// Extrusion stroke width in output pixels.
    pub line_width: f64,
    pub show_legend: bool,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            layer_min: 0,
            layer_max: None,
            show_travel: false,
            support_opacity: 1.,
            show_bed: true,
            transparent: false,
            color_mode: ColorMode::Feature,
            camera: Camera::default(),
            line_width: 1.5,
            show_legend: true,
        }
    }
}

impl RenderOptions {
    fn layers(&self, n: usize) -> std::ops::Range<usize> {
        let end = self
            .layer_max
            .map_or(n, |last| last.saturating_add(1).min(n));
        self.layer_min.min(end)..end
    }
    fn validate(&self) -> Result<(), RenderError> {
        validate_dimensions(self.width, self.height)?;
        if !(0.0..=1.).contains(&self.support_opacity) {
            return Err(RenderError(
                "support opacity must be between 0 and 1".into(),
            ));
        }
        if !self.camera.valid() {
            return Err(RenderError("camera values must be finite; zoom must be in (0, 1000], elevation in [-90, 90], and pan in [-100, 100]".into()));
        }
        if !self.line_width.is_finite() || !(0.1..=100.0).contains(&self.line_width) {
            return Err(RenderError(
                "line width must be finite and in 0.1..=100 pixels".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pass {
    Opaque,
    Support,
    Travel,
}

impl Pass {
    fn of(feature: ExtrusionType, travel: bool, split_support: bool) -> Self {
        if travel {
            Self::Travel
        } else if split_support && feature == ExtrusionType::SupportMaterial {
            Self::Support
        } else {
            Self::Opaque
        }
    }

    fn visible(support_opacity: f64, travel: bool) -> impl Iterator<Item = Self> {
        [Self::Opaque, Self::Support, Self::Travel]
            .into_iter()
            .filter(move |pass| match pass {
                Self::Opaque => true,
                Self::Support => support_opacity > 0. && support_opacity < 1.,
                Self::Travel => travel,
            })
    }

    fn opacity(self, options: &RenderOptions) -> f64 {
        match self {
            Self::Opaque => 1.,
            Self::Support => options.support_opacity,
            Self::Travel => TRAVEL_OPACITY,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Row-major sRGB pixels with straight (unassociated) alpha.
    pub rgba: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderError(pub String);

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Error for RenderError {}

pub fn validate_dimensions(width: u32, height: u32) -> Result<(), RenderError> {
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err(RenderError(format!(
            "invalid render size {width}x{height}: dimensions must be 1..={MAX_DIMENSION} and total pixels at most {MAX_PIXELS}"
        )));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct ScreenPoint {
    x: f64,
    y: f64,
    metric: f64,
}

struct Projection {
    center: [f64; 3],
    basis: [[f64; 3]; 3],
    distance: f64,
    near: f64,
    scale: f64,
    screen_center: [f64; 2],
    perspective: bool,
}

fn dot(a: [f64; 3], b: [f64; 3]) -> f64 {
    a.into_iter().zip(b).map(|(a, b)| a * b).sum()
}
fn subtract(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|i| a[i] - b[i])
}

fn tangent_shade(start: [f64; 3], end: [f64; 3]) -> f64 {
    let tangent = subtract(end, start);
    let length_squared = dot(tangent, tangent);
    if length_squared == 0.0 {
        return 1.0;
    }
    // The component of the light perpendicular to the filament lights its side.
    // Squaring the dot product gives the same shade in either print direction.
    let light = [-0.88, -0.36, 0.31];
    let parallel = dot(tangent, light);
    let diffuse = (1.0 - parallel * parallel / (dot(light, light) * length_squared))
        .max(0.0)
        .sqrt();
    0.44 + 0.56 * diffuse
}

fn tangent_color(color: [u8; 3], start: [f64; 3], end: [f64; 3]) -> [u8; 3] {
    let shade = tangent_shade(start, end);
    color.map(|c| (f64::from(c) * shade + 0.5) as u8)
}

fn corners(bounds: Bounds) -> [[f64; 3]; 8] {
    std::array::from_fn(|index| {
        std::array::from_fn(|axis| {
            if index & (1 << axis) == 0 {
                bounds.min[axis]
            } else {
                bounds.max[axis]
            }
        })
    })
}

impl Projection {
    fn new(
        mut bounds: Bounds,
        bed: [f64; 2],
        options: &RenderOptions,
    ) -> Result<Self, RenderError> {
        if !bounds.min.into_iter().chain(bounds.max).all(f64::is_finite)
            || (0..3).any(|axis| bounds.max[axis] < bounds.min[axis])
        {
            return Err(RenderError(
                "print bounds must be finite and ordered".into(),
            ));
        }
        if options.show_bed {
            if bed.iter().any(|d| !d.is_finite() || *d <= 0.0) {
                return Err(RenderError(
                    "bed dimensions must be finite and positive".into(),
                ));
            }
            bounds.include([0.0, 0.0, 0.0]);
            bounds.include([bed[0], bed[1], 0.0]);
        }
        let center = std::array::from_fn(|axis| bounds.min[axis] * 0.5 + bounds.max[axis] * 0.5);
        let basis = options.camera.basis();
        let diagonal = bounds.size().into_iter().fold(0.0_f64, f64::hypot).max(1.0);
        if !diagonal.is_finite() || !center.into_iter().all(f64::is_finite) {
            return Err(RenderError("print bounds are too large to project".into()));
        }
        let width = f64::from(options.width);
        let height = f64::from(options.height);
        let extent = corners(bounds).map(|corner| {
            let relative = subtract(corner, center);
            basis.map(|axis| dot(relative, axis))
        });
        // A 35-degree vertical field of view; fit all bounds with an 8% margin.
        let half_fov_tangent = 17.5_f64.to_radians().tan();
        let focal_length = height * 0.5 / half_fov_tangent;
        let mut distance = diagonal * 0.6;
        let mut max_x = 0.5_f64;
        let mut max_y = 0.5_f64;
        for point in extent {
            max_x = max_x.max(point[0].abs());
            max_y = max_y.max(point[1].abs());
            distance = distance.max(point[2] + point[0].abs() * focal_length / (width * 0.46));
            distance = distance.max(point[2] + point[1].abs() * focal_length / (height * 0.46));
        }
        distance += diagonal * 0.05;
        let scale = if options.camera.perspective {
            focal_length
        } else {
            (width * 0.46 / max_x).min(height * 0.46 / max_y)
        } * options.camera.zoom;
        if !distance.is_finite() || !scale.is_finite() {
            return Err(RenderError("print bounds are too large to project".into()));
        }
        Ok(Self {
            center,
            basis,
            distance,
            near: (distance * 1e-6).max(1e-9),
            scale,
            screen_center: [
                width * (0.5 + options.camera.pan[0]),
                height * (0.5 + options.camera.pan[1]),
            ],
            perspective: options.camera.perspective,
        })
    }

    fn camera_point(&self, point: [f64; 3]) -> [f64; 3] {
        let relative = subtract(point, self.center);
        [
            dot(relative, self.basis[0]),
            dot(relative, self.basis[1]),
            self.distance - dot(relative, self.basis[2]),
        ]
    }

    fn project_camera(&self, point: [f64; 3]) -> ScreenPoint {
        let factor = if self.perspective {
            self.scale / point[2]
        } else {
            self.scale
        };
        ScreenPoint {
            x: self.screen_center[0] + point[0] * factor,
            y: self.screen_center[1] - point[1] * factor,
            // Reciprocal camera depth interpolates linearly in screen space.
            metric: if self.perspective {
                1.0 / point[2]
            } else {
                -point[2]
            },
        }
    }

    fn project(&self, point: [f64; 3]) -> Option<ScreenPoint> {
        let point = self.camera_point(point);
        (point[2] >= self.near).then(|| self.project_camera(point))
    }

    fn project_line(&self, start: [f64; 3], end: [f64; 3]) -> Option<[ScreenPoint; 2]> {
        let mut a = self.camera_point(start);
        let mut b = self.camera_point(end);
        if a[2] < self.near && b[2] < self.near {
            return None;
        }
        if a[2] < self.near || b[2] < self.near {
            let fraction = (self.near - a[2]) / (b[2] - a[2]);
            let intersection = std::array::from_fn(|axis| a[axis] + fraction * (b[axis] - a[axis]));
            if a[2] < self.near {
                a = intersection;
            } else {
                b = intersection;
            }
        }
        Some([self.project_camera(a), self.project_camera(b)])
    }
}

struct Raster {
    frame: Frame,
    depth: Vec<f32>,
}

struct Band<'a> {
    width: u32,
    height: u32,
    rgba: &'a mut [u8],
    depth: &'a mut [f32],
}

impl Raster {
    fn new(width: u32, height: u32, background: [u8; 4]) -> Result<Self, RenderError> {
        validate_dimensions(width, height)?;
        let count = width as usize * height as usize;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(count * 4)
            .map_err(|error| RenderError(format!("image allocation failed: {error}")))?;
        for _ in 0..count {
            rgba.extend_from_slice(&background);
        }
        let mut depth = Vec::new();
        depth
            .try_reserve_exact(count)
            .map_err(|error| RenderError(format!("depth-buffer allocation failed: {error}")))?;
        depth.resize(count, f32::NEG_INFINITY);
        Ok(Self {
            frame: Frame {
                width,
                height,
                rgba,
            },
            depth,
        })
    }

    fn band(&mut self) -> Band<'_> {
        Band {
            width: self.frame.width,
            height: self.frame.height,
            rgba: &mut self.frame.rgba,
            depth: &mut self.depth,
        }
    }
}

#[inline]
fn blend_over(dst: &mut [u8; 4], color: [u8; 3], alpha: f64) {
    let [r, g, b] = color;
    if alpha >= 1. {
        *dst = [r, g, b, 255];
        return;
    }
    let mix = match dst[3] {
        255 => alpha,
        0 => {
            let a = (alpha * 255. + 0.5) as u8;
            if a != 0 {
                *dst = [r, g, b, a];
            }
            return;
        }
        a => {
            // With straight alpha, mix RGB by the source's share of the output alpha.
            let out = f64::from(a) + f64::from(255 - a) * alpha;
            dst[3] = (out + 0.5) as u8;
            alpha * 255. / out
        }
    };
    let weight = (mix * 65_536. + 0.5) as i32;
    for (d, src) in dst.iter_mut().zip(color) {
        let base = i32::from(*d);
        *d = (base + (((i32::from(src) - base) * weight + 32_768) >> 16)) as u8;
    }
}

impl Band<'_> {
    fn line(
        &mut self,
        a: ScreenPoint,
        b: ScreenPoint,
        resolve: impl FnMut() -> [u8; 3],
        width: f64,
        opacity: f64,
        write_depth: bool,
    ) {
        let Some(points) = clip_line(
            a,
            b,
            f64::from(self.width),
            f64::from(self.height),
            width * 0.5 + 1.0,
        ) else {
            return;
        };
        self.paint(points, 0, resolve, width, opacity, write_depth);
    }

    fn paint(
        &mut self,
        [a, b]: [ScreenPoint; 2],
        first_row: u32,
        mut resolve: impl FnMut() -> [u8; 3],
        width: f64,
        opacity: f64,
        write_depth: bool,
    ) {
        let mut color = None;
        let radius = width * 0.5;
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let length_squared = dx * dx + dy * dy;
        let inv_length = if length_squared > 1e-20 {
            1.0 / length_squared
        } else {
            0.0
        };
        let outer = radius + 0.5;
        let w = self.width;
        let h = self.height;
        // Use the same rounded depth interpolation as the pixel test at t=1.
        // A stroke behind this pixel at both ends cannot contribute.
        let max_metric = a.metric.max(a.metric + (b.metric - a.metric)) as f32;
        let left = ((a.x.min(b.x) - outer + 0.5) as i32).max(0);
        let right = ((a.x.max(b.x) + outer + 0.5) as i32).min(w as i32);
        let rows = ((a.y.min(b.y) - outer + 0.5) as i32).max(first_row as i32)
            ..((a.y.max(b.y) + outer + 0.5) as i32).min((first_row + h) as i32);
        // The capsule lies inside both its endpoint box and the strip around
        // its infinite center line. Short moves need only the box.
        let narrow = dy.abs() > 2.0 && dx.abs() > 2.0;
        let slope = if narrow { dx / dy } else { 0.0 };
        let extent = outer * (1.0 + slope * slope).sqrt();
        for y in rows {
            let py = f64::from(y) + 0.5 - a.y;
            let xs = if narrow {
                let center = a.x + py * slope;
                let slack = 8. * f64::EPSILON * (a.x.abs() + (py * slope).abs() + extent);
                left.max((center - extent - slack + 0.5) as i32)
                    ..right.min((center + extent + slack + 0.5) as i32)
            } else {
                left..right
            };
            for x in xs {
                let pixel = (y as usize - first_row as usize) * w as usize + x as usize;
                if max_metric < self.depth[pixel] {
                    continue;
                }
                let px = f64::from(x) + 0.5 - a.x;
                let t = ((px * dx + py * dy) * inv_length).clamp(0.0, 1.0);
                let distance2 = (px - t * dx).powi(2) + (py - t * dy).powi(2);
                if distance2 >= outer * outer {
                    continue;
                }
                let metric = (a.metric + t * (b.metric - a.metric)) as f32;
                if metric < self.depth[pixel] {
                    continue;
                }
                if write_depth {
                    self.depth[pixel] = metric;
                }
                blend_over(
                    &mut self.rgba.as_chunks_mut::<4>().0[pixel],
                    *color.get_or_insert_with(&mut resolve),
                    opacity * (outer - distance2.sqrt()).min(1.0),
                );
            }
        }
    }
}

impl Raster {
    fn overlay_pixel(&mut self, x: i32, y: i32, color: [u8; 3], alpha: f64) {
        if (x as u32) < self.frame.width && (y as u32) < self.frame.height {
            let pixel = y as usize * self.frame.width as usize + x as usize;
            blend_over(
                &mut self.frame.rgba.as_chunks_mut::<4>().0[pixel],
                color,
                alpha,
            );
        }
    }
    fn line(
        &mut self,
        a: ScreenPoint,
        b: ScreenPoint,
        color: [u8; 3],
        width: f64,
        opacity: f64,
        write_depth: bool,
    ) {
        self.band()
            .line(a, b, || color, width, opacity, write_depth);
    }
    fn world_line(
        &mut self,
        projection: &Projection,
        points: [[f64; 3]; 2],
        color: [u8; 3],
        width: f64,
        opacity: f64,
        write_depth: bool,
    ) {
        if let Some([a, b]) = projection.project_line(points[0], points[1]) {
            self.line(a, b, color, width, opacity, write_depth);
        }
    }

    fn bed_triangle(&mut self, vertices: [ScreenPoint; 3], fill: [u8; 4]) {
        if !vertices
            .iter()
            .all(|point| point.x.is_finite() && point.y.is_finite())
        {
            return;
        }
        let edge = |a: ScreenPoint, b: ScreenPoint, x: f64, y: f64| {
            (x - a.x) * (b.y - a.y) - (y - a.y) * (b.x - a.x)
        };
        let area = edge(vertices[0], vertices[1], vertices[2].x, vertices[2].y);
        if area.abs() < 1e-12 {
            return;
        }
        let minimum_x = vertices
            .iter()
            .map(|p| p.x)
            .fold(f64::INFINITY, f64::min)
            .floor()
            .max(0.0) as i32;
        let maximum_x = vertices
            .iter()
            .map(|p| p.x)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil()
            .min(f64::from(self.frame.width) - 1.0) as i32;
        let minimum_y = vertices
            .iter()
            .map(|p| p.y)
            .fold(f64::INFINITY, f64::min)
            .floor()
            .max(0.0) as i32;
        let maximum_y = vertices
            .iter()
            .map(|p| p.y)
            .fold(f64::NEG_INFINITY, f64::max)
            .ceil()
            .min(f64::from(self.frame.height) - 1.0) as i32;
        for y in minimum_y..=maximum_y {
            for x in minimum_x..=maximum_x {
                let px = f64::from(x) + 0.5;
                let py = f64::from(y) + 0.5;
                let a = edge(vertices[1], vertices[2], px, py) / area;
                let b = edge(vertices[2], vertices[0], px, py) / area;
                let c = 1.0 - a - b;
                if a >= -1e-9 && b >= -1e-9 && c >= -1e-9 {
                    let i = y as usize * self.frame.width as usize + x as usize;
                    self.frame.rgba.as_chunks_mut::<4>().0[i] = fill;
                }
            }
        }
    }

    fn rectangle(&mut self, x: i32, y: i32, width: i32, height: i32, color: [u8; 3], opacity: f64) {
        let end_x = (x.saturating_add(width)).min(self.frame.width as i32);
        let end_y = (y.saturating_add(height)).min(self.frame.height as i32);
        for py in y.max(0)..end_y {
            for px in x.max(0)..end_x {
                self.overlay_pixel(px, py, color, opacity);
            }
        }
    }

    fn text(&mut self, x: i32, y: i32, text: &str, color: [u8; 3], scale: i32) {
        let text = text.replace('³', "3").to_ascii_uppercase();
        for (index, character) in text.chars().enumerate() {
            let origin_x = x + index as i32 * 6 * scale;
            if origin_x >= self.frame.width as i32 {
                break;
            }
            for (row, bits) in glyph(character).into_iter().enumerate() {
                for column in 0..5 {
                    if bits & (1 << (4 - column)) != 0 {
                        self.rectangle(
                            origin_x + column * scale,
                            y + row as i32 * scale,
                            scale,
                            scale,
                            color,
                            1.0,
                        );
                    }
                }
            }
        }
    }
}

/// Liang–Barsky clipping bounds all raster loops, including giant offscreen
/// coordinates and camera pans. The depth attribute is clipped consistently.
#[inline]
fn clip_line(
    a: ScreenPoint,
    b: ScreenPoint,
    width: f64,
    height: f64,
    margin: f64,
) -> Option<[ScreenPoint; 2]> {
    if [a, b].into_iter().all(|p| {
        (-margin..=width + margin).contains(&p.x)
            && (-margin..=height + margin).contains(&p.y)
            && p.metric.is_finite()
    }) {
        return Some([a, b]);
    }
    clip_crossing(a, b, width, height, margin)
}

fn clip_crossing(
    a: ScreenPoint,
    b: ScreenPoint,
    width: f64,
    height: f64,
    margin: f64,
) -> Option<[ScreenPoint; 2]> {
    if ![a.x, a.y, a.metric, b.x, b.y, b.metric]
        .into_iter()
        .all(f64::is_finite)
    {
        return None;
    }
    let dx = b.x - a.x;
    let dy = b.y - a.y;
    if !dx.is_finite() || !dy.is_finite() {
        return None;
    }
    let mut low = 0.0_f64;
    let mut high = 1.0_f64;
    for (p, q) in [
        (-dx, a.x + margin),
        (dx, width + margin - a.x),
        (-dy, a.y + margin),
        (dy, height + margin - a.y),
    ] {
        if p.abs() < 1e-30 {
            if q < 0.0 {
                return None;
            }
        } else {
            let ratio = q / p;
            if p < 0.0 {
                low = low.max(ratio);
            } else {
                high = high.min(ratio);
            }
            if low > high {
                return None;
            }
        }
    }
    Some([low, high].map(|t| ScreenPoint {
        x: a.x + t * dx,
        y: a.y + t * dy,
        metric: a.metric + t * (b.metric - a.metric),
    }))
}

fn draw_bed(raster: &mut Raster, projection: &Projection, [x, y]: [f64; 2], background: [u8; 4]) {
    let points = [[0.0, 0.0, 0.0], [x, 0.0, 0.0], [x, y, 0.0], [0.0, y, 0.0]];
    let projected = points.map(|point| projection.project(point));
    if let [Some(a), Some(b), Some(c), Some(d)] = projected {
        // Fill both triangles with the same composite so the shared diagonal blends once.
        let mut fill = background;
        blend_over(&mut fill, [216; 3], 0.3);
        raster.bed_triangle([a, b, c], fill);
        raster.bed_triangle([a, c, d], fill);
    }
    let labels = bed_lines([x, y], |ends, color, width, opacity| {
        raster.world_line(projection, ends, color, width, opacity, true);
    });
    for (pos, label, color) in labels {
        if let Some(p) = projection.project(pos) {
            raster.text(p.x as i32, p.y as i32, label, color, 1);
        }
    }
}

fn bed_lines(
    [x, y]: [f64; 2],
    mut line: impl FnMut([[f64; 3]; 2], [u8; 3], f64, f64),
) -> [([f64; 3], &'static str, [u8; 3]); 2] {
    let points = [[0., 0., 0.], [x, 0., 0.], [x, y, 0.], [0., y, 0.]];
    // Preserve the usual 10 mm grid while bounding work for giant beds.
    let spacing = 10.0_f64.max((x.max(y) / 500.0 / 10.0).ceil() * 10.0);
    for index in 0..=(x / spacing).floor() as usize {
        let value = index as f64 * spacing;
        line(
            [[value, 0.0, 0.0], [value, y, 0.0]],
            [178, 178, 178],
            0.6,
            0.55,
        );
    }
    for index in 0..=(y / spacing).floor() as usize {
        let value = index as f64 * spacing;
        line(
            [[0.0, value, 0.0], [x, value, 0.0]],
            [178, 178, 178],
            0.6,
            0.55,
        );
    }
    for index in 0..4 {
        line(
            [points[index], points[(index + 1) % 4]],
            [178, 178, 178],
            1.5,
            1.0,
        );
    }
    let length = x.min(y).min(150.0) * 0.2;
    [(0, [235, 72, 72], "X"), (1, [80, 215, 100], "Y")].map(|(axis, color, label)| {
        let mut end = [0.0, 0.0, 0.02];
        end[axis] = length;
        line([[0.0, 0.0, 0.02], end], color, 2.0, 1.0);
        for sign in [-1.0, 1.0] {
            let mut arrow = end;
            arrow[axis] = length * 0.8;
            arrow[1 - axis] = length * 0.07 * sign;
            line([arrow, end], color, 2.0, 1.0);
        }
        end[axis] *= 1.10;
        (end, label, color)
    })
}

struct Legend {
    entries: Vec<colors::LegendEntry>,
    summary: Vec<summary::Row>,
    title: String,
    pad: i32,
    scale: i32,
    width: i32,
    height: i32,
}

impl Legend {
    fn new(
        options: &RenderOptions,
        info: Option<&crate::print_info::PrintInfo>,
        entries: impl FnOnce() -> Vec<colors::LegendEntry>,
    ) -> Option<Self> {
        if (!options.show_legend && info.is_none()) || options.width < 240 || options.height < 100 {
            return None;
        }
        let mut entries = if options.show_legend {
            entries()
        } else {
            Vec::new()
        };
        let scale = (options.width / 640).min(options.height / 450).clamp(1, 4) as i32;
        let pad = 10 * scale;
        let title = if entries.is_empty() {
            "PRINT INFO".into()
        } else {
            options
                .color_mode
                .label()
                .replace('°', "o")
                .replace('³', "3")
        };
        let available_rows = ((options.height as i32 - 5 * pad) / (12 * scale)).max(0) as usize;
        let row_count = entries.len().min(available_rows);
        let columns = ((options.width as i32 * 3 / 5 - 2 * pad) / (6 * scale)).max(1) as usize;
        let mut summary = info.map(|i| summary::rows(i, columns)).unwrap_or_default();
        let gap = if row_count > 0 { 6 * scale } else { 0 };
        let remaining = options.height as i32 - 5 * pad - row_count as i32 * 12 * scale - gap;
        summary.retain(|row| {
            row.text.chars().count() + 2 * usize::from(row.swatch.is_some()) <= columns
        });
        summary.truncate((remaining.max(0) / (12 * scale)) as usize);
        if entries.is_empty() && summary.is_empty() {
            return None;
        }
        let max_length = entries
            .iter()
            .map(|entry| entry.label.chars().count() + 3)
            .chain(
                summary
                    .iter()
                    .map(|r| r.text.chars().count() + usize::from(r.swatch.is_some()) * 2),
            )
            .chain([title.chars().count()])
            .max()
            .unwrap_or(0) as i32;
        let width = (max_length * 6 * scale + 2 * pad).min(options.width as i32 - 2 * pad);
        let height = 3 * pad
            + (row_count + summary.len()) as i32 * 12 * scale
            + if summary.is_empty() { 0 } else { gap };
        entries.truncate(row_count);
        Some(Self {
            entries,
            summary,
            title,
            pad,
            scale,
            width,
            height,
        })
    }

    fn draw(self, raster: &mut Raster) {
        let Self {
            entries,
            summary,
            title,
            pad,
            scale,
            width,
            height,
        } = self;
        raster.rectangle(pad, pad, width, height, [22, 25, 32], 0.95);
        raster.rectangle(pad, pad, 2 * scale, height, [92, 109, 132], 1.0);
        raster.text(pad * 2, pad * 2, &title, [235, 238, 245], scale);
        let summary_y = pad * 4
            + entries.len() as i32 * 12 * scale
            + if entries.is_empty() { 0 } else { 6 * scale };
        for (index, entry) in entries.into_iter().enumerate() {
            let y = pad * 4 + index as i32 * 12 * scale;
            raster.rectangle(pad * 2, y, 7 * scale, 7 * scale, entry.color, 1.0);
            let max_characters = ((width - 2 * pad - 12 * scale) / (6 * scale)).max(0) as usize;
            let label: String = entry.label.chars().take(max_characters).collect();
            raster.text(
                pad * 2 + 12 * scale,
                y,
                &label.replace('°', "o"),
                [218, 223, 232],
                scale,
            );
        }
        for (index, row) in summary.into_iter().enumerate() {
            let y = summary_y + index as i32 * 12 * scale;
            let mut x = pad * 2;
            if let Some(color) = row.swatch {
                raster.rectangle(x, y, 7 * scale, 7 * scale, color, 1.);
                x += 12 * scale;
            }
            raster.text(x, y, &row.text, [218, 223, 232], scale);
        }
    }
}

fn canvas(
    bed: [f64; 2],
    options: &RenderOptions,
    subject: &Subject,
    legend: Option<&Legend>,
) -> Result<(Raster, Projection), RenderError> {
    let projection = Projection::framed(subject.bounds(), bed, options, legend, |p, s| {
        subject.project(p, s)
    })?;
    let background = if options.transparent {
        [0; 4]
    } else {
        OPAQUE_BACKGROUND
    };
    let mut raster = Raster::new(options.width, options.height, background)?;
    if options.show_bed {
        draw_bed(&mut raster, &projection, bed, background);
    }
    Ok((raster, projection))
}

pub fn render(toolpath: &ToolpathData, options: &RenderOptions) -> Result<Frame, RenderError> {
    let chunks: Vec<_> = toolpath
        .layers
        .range(options.layers(toolpath.total_layers()))
        .chunks(4096)
        .collect();
    render_lines(
        [toolpath.bed_x, toolpath.bed_y],
        toolpath.bounds,
        options,
        None,
        toolpath.segments().map(|s| {
            (
                s.extrusion_type,
                s.is_travel,
                colors::segment_value(s, options.color_mode),
            )
        }),
        &chunks,
        |&chunk| {
            chunk
                .iter()
                .map(|s| PreviewLine::from_segment(s, options.color_mode))
        },
    )
}

#[cfg(feature = "gui")]
pub(crate) fn render_detailed(
    print: &crate::geometry::DetailedPrint,
    options: &RenderOptions,
) -> Result<Frame, RenderError> {
    let chunks: Vec<_> = print
        .layers
        .chunks(options.layers(print.total_layers()))
        .collect();
    render_lines(
        [print.bed_x, print.bed_y],
        print.bounds,
        options,
        None,
        print.segments().map(|s| {
            (
                s.extrusion_type,
                s.is_travel,
                colors::segment_value(&s, options.color_mode),
            )
        }),
        &chunks,
        |&chunk| {
            chunk
                .segments()
                .map(|s| PreviewLine::from_segment(&s, options.color_mode))
        },
    )
}

pub(crate) fn render_preview(
    print: &Print<PreviewPaths>,
    options: &RenderOptions,
    info: Option<&crate::print_info::PrintInfo>,
) -> Result<Frame, RenderError> {
    let chunks: Vec<_> = print
        .geometry
        .chunks(options.layers(print.layers))
        .collect();
    let values = print
        .geometry
        .chunks(0..print.layers)
        .flat_map(|c| c.values());
    render_lines(
        print.bed,
        print.bounds,
        options,
        info,
        values,
        &chunks,
        |chunk| chunk.lines(),
    )
}

fn render_lines<C: Sync, I: ExactSizeIterator<Item = PreviewLine>>(
    bed: [f64; 2],
    bounds: Bounds,
    options: &RenderOptions,
    info: Option<&crate::print_info::PrintInfo>,
    values: impl Iterator<Item = (crate::types::ExtrusionType, bool, f64)> + Send,
    chunks: &[C],
    lines: impl Fn(&C) -> I + Sync,
) -> Result<Frame, RenderError> {
    options.validate()?;
    let frame = || Subject::from_lines(chunks, &lines, options.show_travel);
    let colors = || {
        colors::preview_colors(
            values,
            bounds,
            options.color_mode,
            options.show_legend,
            options.show_travel,
        )
    };
    let (subject, (range, entries)) = if chunks.len() >= 4 {
        rayon::join(frame, colors)
    } else {
        (frame(), colors())
    };
    let legend = Legend::new(options, info, || entries);
    let (mut raster, projection) = canvas(bed, options, &subject, legend.as_ref())?;
    let serial = chunks.iter().map(|c| lines(c).len()).sum::<usize>() < 16_384;
    // Translucent strokes test the opaque depth but do not write it.
    let split_support = options.support_opacity < 1.
        && chunks
            .iter()
            .any(|c| lines(c).any(|s| s.feature == ExtrusionType::SupportMaterial));
    for pass in Pass::visible(
        if split_support {
            options.support_opacity
        } else {
            1.
        },
        options.show_travel,
    ) {
        let travel_pass = pass == Pass::Travel;
        let selected = |s: &PreviewLine| Pass::of(s.feature, s.travel, split_support) == pass;
        let width = if travel_pass {
            (options.line_width * 0.45).max(0.5)
        } else {
            options.line_width
        };
        let opacity = pass.opacity(options);
        let (viewport_width, viewport_height) = (
            f64::from(raster.frame.width),
            f64::from(raster.frame.height),
        );
        let project = |segment: PreviewLine| -> Result<_, RenderError> {
            if !segment
                .start
                .into_iter()
                .chain(segment.end)
                .all(f64::is_finite)
            {
                return Err(RenderError("segment coordinates must be finite".into()));
            }
            let Some(points) = projection
                .project_line(segment.start, segment.end)
                .and_then(|[a, b]| {
                    clip_line(a, b, viewport_width, viewport_height, width * 0.5 + 1.)
                })
            else {
                return Ok(None);
            };
            let color = match options.color_mode {
                ColorMode::Feature => colors::feature_color(segment.feature),
                mode => colors::value_color(mode, range, segment.value),
            };
            let color = if travel_pass {
                color
            } else {
                tangent_color(color, segment.start, segment.end)
            };
            Ok(Some((points, color)))
        };
        let paint = |band: &mut Band<'_>, y, (points, color)| {
            band.paint(points, y, || color, width, opacity, pass == Pass::Opaque)
        };
        if serial {
            for s in chunks.iter().flat_map(&lines).filter(selected) {
                if let Some(line) = project(s)? {
                    paint(&mut raster.band(), 0, line);
                }
            }
        } else {
            bands::draw(
                &mut raster,
                width,
                chunks,
                |chunk, projected| {
                    for s in lines(chunk).filter(selected) {
                        if let Some(line) = project(s)? {
                            projected.push(line);
                        }
                    }
                    Ok(())
                },
                |band, y, line, _| paint(band, y, line),
            )?;
        }
    }
    if let Some(legend) = legend {
        legend.draw(&mut raster);
    }
    Ok(raster.frame)
}

// Authored 5x7 bitmap glyphs. No external assets, system fonts, or font license
// are required for deterministic PNG legends. Lowercase is rendered uppercase.
fn glyph(character: char) -> [u8; 7] {
    match character {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 18, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        '.' => [0, 0, 0, 0, 0, 12, 12],
        ',' => [0, 0, 0, 0, 4, 4, 8],
        ':' => [0, 12, 12, 0, 12, 12, 0],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '+' => [0, 4, 4, 31, 4, 4, 0],
        '/' => [1, 2, 2, 4, 8, 8, 16],
        '%' => [25, 25, 2, 4, 8, 19, 19],
        '°' => [6, 9, 6, 0, 0, 0, 0],
        '→' => [0, 4, 2, 31, 2, 4, 0],
        '@' => [14, 17, 23, 21, 23, 16, 14],
        '&' => [12, 18, 20, 8, 21, 18, 13],
        '#' => [10, 10, 31, 10, 31, 10, 10],
        '(' => [2, 4, 8, 8, 8, 4, 2],
        ')' => [8, 4, 2, 2, 2, 4, 8],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        _ => [0; 7],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_translucent_strokes_composite_on_any_background() {
        let mut layer = [0; 4];
        blend_over(&mut layer, [255, 0, 0], 0.5);
        assert_eq!(layer, [255, 0, 0, 128]);
        blend_over(&mut layer, [0, 0, 255], 0.5);
        // The top stroke contributes half the light, the lower stroke a quarter.
        for background in [[255u8; 3], [12, 34, 56]] {
            let [r, g, b] = background;
            let mut pixel = [r, g, b, 255];
            let [red, green, blue, alpha] = layer;
            blend_over(&mut pixel, [red, green, blue], f64::from(alpha) / 255.);
            for (i, stroke) in [63.75, 0., 127.5].into_iter().enumerate() {
                let expected = (stroke + f64::from(background[i]) * 0.25).round() as u8;
                assert!(pixel[i].abs_diff(expected) <= 1);
            }
        }
    }

    #[test]
    fn scanlines_cover_the_caps_and_sides_of_a_stroke() {
        let a = ScreenPoint {
            x: 3.25,
            y: 2.75,
            metric: 1.,
        };
        for (x, y) in [
            (3.25, 2.75),
            (31.5, 3.),
            (32., 5.1),
            (7., 26.8),
            (1., 38.),
            (30., 31.),
        ] {
            let b = ScreenPoint { x, y, ..a };
            let mut raster = Raster::new(34, 40, OPAQUE_BACKGROUND).unwrap();
            raster.line(a, b, [255; 3], 2.3, 1., true);
            let dx = b.x - a.x;
            let dy = b.y - a.y;
            let length = dx.hypot(dy);
            for (i, depth) in raster.depth.iter().enumerate() {
                let px = (i % 34) as f64 + 0.5 - a.x;
                let py = (i / 34) as f64 + 0.5 - a.y;
                // Independent distance to the start cap, end cap, or side.
                let along = px * dx + py * dy;
                let distance = if along <= 0. {
                    px.hypot(py)
                } else if along >= length * length {
                    (px - dx).hypot(py - dy)
                } else {
                    (px * dy - py * dx).abs() / length
                };
                assert_eq!(depth.is_finite(), distance < 2.3 / 2. + 0.5);
            }
        }
    }

    #[test]
    fn filament_shading_depends_on_orientation_but_not_print_direction() {
        let base = [45, 180, 0];
        let start = [0.; 3];
        let shades = [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.], [1., 1., 1.]].map(|end| {
            let shade = tangent_color(base, start, end);
            assert_eq!(shade, tangent_color(base, end, start));
            assert_eq!(shade, tangent_color(base, start, end.map(|v| 2. * v)));
            assert_eq!(shade[2], 0);
            shade
        });
        assert!(shades.iter().any(|shade| *shade != shades[0]));
    }

    #[test]
    fn clipping_preserves_depth_interpolation_and_bounds() {
        let a = ScreenPoint {
            x: -100.0,
            y: 50.0,
            metric: 0.0,
        };
        let b = ScreenPoint {
            x: 200.0,
            y: 50.0,
            metric: 3.0,
        };
        let [start, end] = clip_line(a, b, 100.0, 100.0, 0.0).unwrap();
        assert_eq!((start.x, end.x), (0.0, 100.0));
        assert!((start.metric - 1.0).abs() < 1e-12);
        assert!((end.metric - 2.0).abs() < 1e-12);
        assert!(
            clip_line(
                ScreenPoint { y: -5.0, ..a },
                ScreenPoint { y: -5.0, ..b },
                100.0,
                100.0,
                0.0
            )
            .is_none()
        );
    }

    #[test]
    fn depth_buffer_prefers_nearer_line_independent_of_submission_order() {
        for reverse in [false, true] {
            let mut raster = Raster::new(64, 64, OPAQUE_BACKGROUND).unwrap();
            let mut lines = [(0.2, [255, 0, 0]), (0.5, [0, 255, 0])];
            if reverse {
                lines.reverse();
            }
            for (metric, color) in lines {
                raster.line(
                    ScreenPoint {
                        x: 2.0,
                        y: 32.5,
                        metric,
                    },
                    ScreenPoint {
                        x: 62.0,
                        y: 32.5,
                        metric,
                    },
                    color,
                    3.0,
                    1.0,
                    true,
                );
            }
            let offset = (32 * 64 + 32) * 4;
            assert_eq!(&raster.frame.rgba[offset..offset + 3], &[0, 255, 0]);
        }
    }

    #[test]
    fn raster_line_clips_a_million_pixel_segment() {
        let mut raster = Raster::new(32, 32, OPAQUE_BACKGROUND).unwrap();
        raster.line(
            ScreenPoint {
                x: -1_000_000.0,
                y: 16.5,
                metric: 1.0,
            },
            ScreenPoint {
                x: 1_000_000.0,
                y: 16.5,
                metric: 1.0,
            },
            [255, 0, 0],
            1.0,
            1.0,
            true,
        );
        assert_eq!(
            &raster.frame.rgba[(16 * 32) * 4..(16 * 32) * 4 + 3],
            &[255, 0, 0]
        );
    }
}
