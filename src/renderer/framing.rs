//! Fit the selected print in screen space, keeping the legend clear.
use super::{Legend, Projection, RenderOptions, corners};
use crate::{
    geometry::PlanarArc,
    types::{Bounds, ExtrusionType, Segment},
};

#[cfg(feature = "gui")]
pub(super) mod index;

const BLOCK: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(super) enum Role {
    #[default]
    Auxiliary,
    Object,
}

impl Role {
    pub fn of(feature: ExtrusionType) -> Self {
        match feature {
            ExtrusionType::Custom
            | ExtrusionType::SkirtBrim
            | ExtrusionType::Wipe
            | ExtrusionType::Travel => Self::Auxiliary,
            _ => Self::Object,
        }
    }
}

#[derive(Default)]
pub(super) struct Subject {
    pub role: Role,
    count: usize,
    // Project small local boxes: the empty corners of one large 3D box can
    // otherwise dominate an isometric view of a flat or diagonal part.
    blocks: Vec<Bounds>,
}

impl Subject {
    pub fn from_segments(segments: &[Segment], travel: bool) -> Self {
        let mut subject = Self::default();
        for s in segments {
            if travel || !s.is_travel {
                let role = if travel {
                    Role::Object
                } else {
                    Role::of(s.extrusion_type)
                };
                subject.include(role, s.start, s.end, None);
            }
        }
        subject
    }

    pub fn include(&mut self, role: Role, start: [f64; 3], end: [f64; 3], arc: Option<PlanarArc>) {
        if role < self.role {
            return;
        }
        if role > self.role {
            self.blocks.clear();
            self.count = 0;
            self.role = role;
        }
        if self.count.is_multiple_of(BLOCK) {
            self.blocks.push(Bounds {
                min: start,
                max: start,
            });
        }
        self.count += 1;
        let block = self.blocks.last_mut().unwrap();
        block.include(start);
        block.include(end);
        if let Some(arc) = arc {
            arc.include_curve(block, start[2]);
        }
    }

    pub fn bounds(&self) -> Option<Bounds> {
        self.blocks.iter().copied().reduce(|mut a, b| {
            a.include(b.min);
            a.include(b.max);
            a
        })
    }

    pub fn fit(&self, p: &mut Projection, options: &RenderOptions, legend: Option<&Legend>) {
        let mut screen = ScreenBounds::default();
        for &b in &self.blocks {
            screen.include(p, b);
        }
        screen.fit(p, options, legend);
    }
}

struct ScreenBounds {
    min: [f64; 2],
    max: [f64; 2],
}
impl Default for ScreenBounds {
    fn default() -> Self {
        Self {
            min: [f64::INFINITY; 2],
            max: [f64::NEG_INFINITY; 2],
        }
    }
}
impl ScreenBounds {
    fn include(&mut self, p: &Projection, b: Bounds) {
        for pt in corners(b) {
            let [x, y, z] = p.camera_point(pt);
            let factor = if p.perspective { 1.0 / z } else { 1.0 };
            for (i, v) in [x * factor, -y * factor].into_iter().enumerate() {
                self.min[i] = self.min[i].min(v);
                self.max[i] = self.max[i].max(v);
            }
        }
    }
    fn fit(self, p: &mut Projection, options: &RenderOptions, legend: Option<&Legend>) {
        let Self { min, max } = self;
        if min[0] > max[0] {
            return;
        }
        let size = [max[0] - min[0], max[1] - min[1]];
        // A line viewed end-on is a dot. Keep its original scale.
        let original_scale = p.scale / options.camera.zoom;
        let end_on = size[0].max(size[1]) * original_scale < 1e-6;
        let w = f64::from(options.width);
        let h = f64::from(options.height);
        let pad = (0.04 * w.min(h)).max(options.line_width * 0.5 + 1.0);
        // Limit padding on tiny thumbnails so a usable viewport remains.
        let px = pad.min(w * 0.25);
        let py = pad.min(h * 0.25);
        let fit = |left: f64, top: f64| {
            let avail = [w - px - left, h - py - top];
            if avail[0] <= 0.0 || avail[1] <= 0.0 {
                return None;
            }
            let scale = if end_on {
                original_scale
            } else {
                (avail[0] / size[0]).min(avail[1] / size[1])
            };
            if !scale.is_finite() {
                return None;
            }
            let half = size.map(|s| s * scale * 0.5);
            let center = [
                (w * 0.5).max(left + half[0]).min(w - px - half[0]),
                (h * 0.5).max(top + half[1]).min(h - py - half[1]),
            ];
            Some((scale, center))
        };
        let frame = legend
            .and_then(|l| {
                // Use the side that fits the larger image. Keep it as close as
                // possible to the canvas center when it already clears the legend.
                [
                    fit(f64::from(l.pad + l.width) + px, py),
                    fit(px, f64::from(l.pad + l.height) + py),
                ]
                .into_iter()
                .flatten()
                .max_by(|a, b| {
                    let offset = |[x, y]: [f64; 2]| (x - w * 0.5).powi(2) + (y - h * 0.5).powi(2);
                    a.0.total_cmp(&b.0)
                        .then_with(|| offset(b.1).total_cmp(&offset(a.1)))
                })
            })
            .or_else(|| fit(px, py));
        if let Some((scale, center)) = frame {
            p.scale = scale * options.camera.zoom;
            for i in 0..2 {
                p.screen_center[i] = center[i] - (min[i] + max[i]) * 0.5 * p.scale
                    + options.camera.pan[i] * [w, h][i];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::colors;

    #[test]
    fn both_parts_stay_inside_the_frame_and_clear_of_the_legend() {
        for (width, height) in [(320, 600), (640, 360)] {
            let options = RenderOptions {
                width,
                height,
                show_bed: false,
                ..Default::default()
            };
            let legend = Legend::new(&options, || colors::feature_legend(&[1; 14])).unwrap();
            let mut subject = Subject::default();
            // Untagged models and a file containing only auxiliary extrusion
            // must both frame all deposited material, including separate parts.
            for role in [Role::Auxiliary, Role::Object] {
                let points = [
                    [10., 20., 0.],
                    [15., 30., 12.],
                    [60., 44., 0.],
                    [80., 49., 30.],
                ];
                for &[start, end] in points.as_chunks::<2>().0 {
                    subject.include(role, start, end, None);
                }
                let mut p =
                    Projection::new(subject.bounds().unwrap(), [100.; 2], &options).unwrap();
                subject.fit(&mut p, &options, Some(&legend));
                let projected = points.map(|pt| p.project(pt).unwrap());
                assert!(projected.iter().all(|pt| pt.x > 0.
                    && pt.x < f64::from(width)
                    && pt.y > 0.
                    && pt.y < f64::from(height)));
                assert!(
                    projected
                        .iter()
                        .all(|pt| pt.x > f64::from(legend.pad + legend.width))
                        || projected
                            .iter()
                            .all(|pt| pt.y > f64::from(legend.pad + legend.height))
                );
            }
        }
    }
}
