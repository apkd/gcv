//! Frame any layer range in groups of 64 visible strokes, as in the PNG export.
//! Precomputed world bounds reject interior groups before we visit their strokes.
use super::{BLOCK, Bounds, Projection, RenderOptions, Role, ScreenBounds, Segment};
use std::ops::Range;

const GROUP: usize = BLOCK * BLOCK;

fn union(mut a: Bounds, b: Bounds) -> Bounds {
    a.include(b.min);
    a.include(b.max);
    a
}

fn extent(s: &Segment) -> Bounds {
    let mut b = Bounds {
        min: s.start,
        max: s.start,
    };
    b.include(s.end);
    b
}

#[derive(Default)]
struct Boxes {
    fine: Vec<Bounds>,
    coarse: Vec<Bounds>,
}

impl Boxes {
    fn push(&mut self, i: usize, b: Bounds) {
        if i.is_multiple_of(BLOCK) {
            self.fine.push(b);
        } else {
            let last = self.fine.last_mut().unwrap();
            *last = union(*last, b);
        }
    }
    fn finish(&mut self) {
        self.coarse = self
            .fine
            .chunks(BLOCK)
            .map(|g| g.iter().copied().reduce(union).unwrap())
            .collect();
    }
}

#[derive(Default)]
pub(crate) struct FramingIndex {
    all: Boxes,
    // Raw segment indices in deposition order for each framing role.
    deposited: [(Vec<u32>, Boxes); 2],
}

impl FramingIndex {
    pub fn new(segments: &[Segment]) -> Self {
        let mut index = Self::default();
        for (i, s) in segments.iter().enumerate() {
            let b = extent(s);
            index.all.push(i, b);
            if !s.is_travel {
                let (ids, boxes) = &mut index.deposited[Role::of(s.extrusion_type) as usize];
                boxes.push(ids.len(), b);
                ids.push(i as u32);
            }
        }
        index.all.finish();
        for (_, boxes) in &mut index.deposited {
            boxes.finish();
        }
        index
    }

    pub fn object(&self) -> Option<(Role, Bounds)> {
        [Role::Object, Role::Auxiliary]
            .into_iter()
            .find_map(|role| {
                self.deposited[role as usize]
                    .1
                    .coarse
                    .iter()
                    .copied()
                    .reduce(union)
                    .map(|b| (role, b))
            })
    }

    pub fn select<'a>(
        &'a self,
        segments: &'a [Segment],
        raw: Range<usize>,
        travel: bool,
    ) -> Option<Selection<'a>> {
        if raw.is_empty() {
            return None;
        }
        if travel {
            return Some(Selection {
                segments,
                ids: None,
                range: raw,
                boxes: &self.all,
            });
        }
        [Role::Object, Role::Auxiliary]
            .into_iter()
            .find_map(|role| {
                let (ids, boxes) = &self.deposited[role as usize];
                let rank = |i| ids.partition_point(|&id| (id as usize) < i);
                let range = rank(raw.start)..rank(raw.end);
                (!range.is_empty()).then_some(Selection {
                    segments,
                    ids: Some(ids),
                    range,
                    boxes,
                })
            })
    }
}

pub(crate) struct Selection<'a> {
    segments: &'a [Segment],
    ids: Option<&'a [u32]>,
    range: Range<usize>,
    boxes: &'a Boxes,
}

impl Selection<'_> {
    fn exact(&self, range: Range<usize>) -> Bounds {
        range
            .map(|i| extent(&self.segments[self.ids.map_or(i, |ids| ids[i] as usize)]))
            .reduce(union)
            .unwrap()
    }

    pub fn bounds(&self) -> Bounds {
        let mut result = None;
        let mut i = self.range.start;
        while i < self.range.end {
            let (n, b) = if i.is_multiple_of(GROUP) && i + GROUP <= self.range.end {
                (GROUP, self.boxes.coarse[i / GROUP])
            } else if i.is_multiple_of(BLOCK) && i + BLOCK <= self.range.end {
                (BLOCK, self.boxes.fine[i / BLOCK])
            } else {
                let end = (i + 1).next_multiple_of(BLOCK).min(self.range.end);
                (end - i, self.exact(i..end))
            };
            result = Some(result.map_or(b, |old| union(old, b)));
            i += n;
        }
        result.unwrap()
    }

    pub fn fit(&self, p: &mut Projection, options: &RenderOptions) {
        let shift = self.range.start % BLOCK;
        let first = self.range.start / BLOCK;
        let end = first + self.range.len().div_ceil(BLOCK);
        let fine = &self.boxes.fine;
        let exact = |i: usize| {
            let start = i * BLOCK + shift;
            if shift == 0 && start + BLOCK <= self.range.end {
                fine[i]
            } else {
                self.exact(start..(start + BLOCK).min(self.range.end))
            }
        };
        let extends = |screen: &ScreenBounds, b| {
            super::corners(b).into_iter().any(|pt| {
                let [x, y, z] = p.camera_point(pt);
                // An outer box can extend beyond the selection and cross the camera.
                if p.perspective && z <= 0. {
                    return true;
                }
                let scale = if p.perspective { 1. / z } else { 1. };
                [x * scale, -y * scale]
                    .into_iter()
                    .enumerate()
                    .any(|(i, v)| v < screen.min[i] || v > screen.max[i])
            })
        };
        let mut screen = ScreenBounds::default();
        for i in (first..end).step_by((end - first).div_ceil(32)) {
            screen.include(p, exact(i));
        }
        for group in first / BLOCK..end.div_ceil(BLOCK) {
            let next = (group + 1) * BLOCK;
            let mut outer = self.boxes.coarse[group];
            // Selection-relative boxes may reach into the next indexed block.
            if shift != 0
                && let Some(&b) = fine.get(next)
            {
                outer = union(outer, b);
            }
            if !extends(&screen, outer) {
                continue;
            }
            for i in (group * BLOCK).max(first)..next.min(end) {
                if shift != 0
                    && !extends(
                        &screen,
                        fine.get(i + 1).map_or(fine[i], |&b| union(fine[i], b)),
                    )
                {
                    continue;
                }
                screen.include(p, exact(i));
            }
        }
        screen.fit(p, options, None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parser::parse_gcode, renderer::framing::Subject, types::ExtrusionType};

    #[test]
    fn arbitrary_layer_cuts_match_the_export_camera() {
        let template = parse_gcode("G1 X1 E1").unwrap().layers.segments()[0].clone();
        let segments: Vec<_> = (0..18000)
            .map(|i| {
                let t = i as f64 * 0.017;
                let x = if i < 517 { 9000. } else { 18. * t.cos() };
                Segment {
                    start: [x, 12. * t.sin(), t / 20.],
                    end: [x + 0.3, 13. * t.sin(), t / 20.],
                    is_travel: i % 9 == 0,
                    extrusion_type: if i < 600 || i % 7 == 0 {
                        ExtrusionType::Custom
                    } else {
                        ExtrusionType::Infill
                    },
                    ..template.clone()
                }
            })
            .collect();
        let index = FramingIndex::new(&segments);
        for raw in [0..0, 0..1, 200..516, 517..600, 713..17113, 3671..18000] {
            for travel in [true, false] {
                let cpu = Subject::from_segments(&segments[raw.clone()], travel);
                let selected = index.select(&segments, raw.clone(), travel);
                assert_eq!(selected.as_ref().map(Selection::bounds), cpu.bounds());
                let Some(selected) = selected else { continue };
                for perspective in [true, false] {
                    let mut o = RenderOptions {
                        show_bed: false,
                        ..Default::default()
                    };
                    o.camera.perspective = perspective;
                    let camera = || Projection::new(selected.bounds(), [200.; 2], &o).unwrap();
                    let (mut a, mut b) = (camera(), camera());
                    cpu.fit(&mut a, &o, None);
                    selected.fit(&mut b, &o);
                    assert_eq!((a.scale, a.screen_center), (b.scale, b.screen_center));
                }
            }
        }
    }
}
