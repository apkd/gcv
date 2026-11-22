//! Feature colors and continuous palettes, preserving the upstream palette.
use crate::types::{ColorMode, ExtrusionType, Segment, ToolpathData};

pub const BACKGROUND: [u8; 3] = [30, 30, 35];
pub const TRAVEL_OPACITY: f64 = 0.15;

pub fn feature_color(feature: ExtrusionType) -> [u8; 3] {
    use ExtrusionType::*;
    match feature {
        ExternalPerimeter => [255, 124, 56],
        Perimeter => [255, 229, 76],
        Infill => [178, 56, 56],
        SolidInfill => [209, 86, 86],
        TopSolidInfill => [255, 102, 102],
        BridgeInfill => [76, 127, 186],
        SupportMaterial => [0, 255, 0],
        SkirtBrim => [0, 135, 109],
        Custom => [165, 12, 229],
        GapFill => [255, 255, 255],
        Ironing => [255, 191, 204],
        Travel => [0, 153, 255],
        Wipe => [127, 127, 0],
        Unknown => [153, 153, 153],
    }
}

/// Scalar ranges use the same native units as `Segment`. They are calculated
/// over the complete print, so changing the layer slider does not change colors.
#[derive(Clone, Debug, PartialEq)]
pub struct ColorRanges {
    pub height: (f64, f64),
    pub speed: (f64, f64),
    pub fan: (f64, f64),
    pub temperature: (f64, f64),
    pub flow: (f64, f64),
}

impl ColorRanges {
    pub fn from_toolpath(toolpath: &ToolpathData) -> Self {
        Self::collect(toolpath, |_| {})
    }
    fn collect(toolpath: &ToolpathData, mut observe: impl FnMut(&Segment)) -> Self {
        let mut ranges = [(f64::INFINITY, f64::NEG_INFINITY); 4];
        for segment in toolpath.segments() {
            observe(segment);
            if segment.is_travel {
                continue;
            }
            let values = [
                segment.feedrate,
                segment.fan_speed,
                segment.temperature,
                segment.volumetric_flow,
            ];
            for (index, value) in values.into_iter().enumerate() {
                if value.is_finite() && (value > 0.0 || index == 1) {
                    ranges[index].0 = ranges[index].0.min(value);
                    ranges[index].1 = ranges[index].1.max(value);
                }
            }
        }
        for (range, mode) in ranges.iter_mut().zip([
            ColorMode::Speed,
            ColorMode::Fan,
            ColorMode::Temperature,
            ColorMode::Flow,
        ]) {
            if !range.0.is_finite() {
                *range = default_range(mode);
            }
        }
        Self {
            height: (toolpath.bounds.min[2], toolpath.bounds.max[2]),
            speed: ranges[0],
            fan: ranges[1],
            temperature: ranges[2],
            flow: ranges[3],
        }
    }

    pub fn range(&self, mode: ColorMode) -> (f64, f64) {
        match mode {
            ColorMode::Feature => (0.0, 1.0),
            ColorMode::Height => self.height,
            ColorMode::Speed => self.speed,
            ColorMode::Fan => self.fan,
            ColorMode::Temperature => self.temperature,
            ColorMode::Flow => self.flow,
        }
    }
}

fn stops(mode: ColorMode) -> &'static [(f64, [f64; 3])] {
    match mode {
        ColorMode::Height | ColorMode::Feature => &[
            (0.0, [0.0, 0.10, 0.90]),
            (0.25, [0.0, 0.70, 0.90]),
            (0.50, [0.0, 0.85, 0.20]),
            (0.75, [1.0, 0.85, 0.0]),
            (1.0, [1.0, 0.15, 0.0]),
        ],
        ColorMode::Speed => &[
            (0.0, [0.0, 0.20, 0.80]),
            (0.33, [0.0, 0.80, 0.30]),
            (0.66, [1.0, 0.85, 0.0]),
            (1.0, [1.0, 0.10, 0.0]),
        ],
        ColorMode::Fan => &[
            (0.0, [0.40, 0.40, 0.40]),
            (0.33, [0.30, 0.65, 0.90]),
            (0.66, [0.10, 0.40, 0.90]),
            (1.0, [0.0, 0.15, 0.70]),
        ],
        ColorMode::Temperature => &[
            (0.0, [0.10, 0.20, 0.80]),
            (0.33, [0.20, 0.80, 0.40]),
            (0.66, [1.0, 0.85, 0.0]),
            (1.0, [1.0, 0.10, 0.0]),
        ],
        ColorMode::Flow => &[
            (0.0, [0.0, 0.50, 0.35]),
            (0.33, [0.30, 0.80, 0.10]),
            (0.66, [1.0, 0.75, 0.0]),
            (1.0, [1.0, 0.0, 0.20]),
        ],
    }
}

pub fn gradient_color(mode: ColorMode, fraction: f64) -> [u8; 3] {
    let fraction = if fraction.is_finite() {
        fraction.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let palette = stops(mode);
    for window in palette.windows(2) {
        let (start, a) = window[0];
        let (end, b) = window[1];
        if fraction <= end {
            let weight = (fraction - start) / (end - start);
            return std::array::from_fn(|i| ((a[i] + weight * (b[i] - a[i])) * 255.0) as u8);
        }
    }
    palette.last().unwrap().1.map(|value| (value * 255.0) as u8)
}

pub fn segment_value(segment: &Segment, mode: ColorMode) -> f64 {
    match mode {
        ColorMode::Feature => 0.0,
        ColorMode::Height => (segment.start[2] + segment.end[2]) * 0.5,
        ColorMode::Speed => segment.feedrate,
        ColorMode::Fan => segment.fan_speed,
        ColorMode::Temperature => segment.temperature,
        ColorMode::Flow => segment.volumetric_flow,
    }
}

pub fn color_for_segment(segment: &Segment, mode: ColorMode, ranges: &ColorRanges) -> [u8; 3] {
    if mode == ColorMode::Feature {
        return feature_color(segment.extrusion_type);
    }
    value_color(mode, ranges.range(mode), segment_value(segment, mode))
}

pub(crate) fn value_color(mode: ColorMode, (low, high): (f64, f64), value: f64) -> [u8; 3] {
    let fraction = if high > low {
        (value - low) / (high - low)
    } else {
        0.5
    };
    gradient_color(mode, fraction)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LegendEntry {
    pub label: String,
    pub color: [u8; 3],
}

fn display_value(mode: ColorMode, value: f64) -> f64 {
    match mode {
        ColorMode::Speed => value / 60.0,
        ColorMode::Fan => value / 255.0 * 100.0,
        _ => value,
    }
}

pub fn legend_entries(
    toolpath: &ToolpathData,
    mode: ColorMode,
    show_travel: bool,
) -> Vec<LegendEntry> {
    preview_colors(
        toolpath
            .segments()
            .map(|s| (s.extrusion_type, s.is_travel, segment_value(s, mode))),
        toolpath.bounds,
        mode,
        true,
        show_travel,
    )
    .1
}

pub(crate) fn feature_legend(counts: &[usize; 14]) -> Vec<LegendEntry> {
    use ExtrusionType::*;
    let order = [
        Perimeter,
        ExternalPerimeter,
        Infill,
        SolidInfill,
        TopSolidInfill,
        BridgeInfill,
        GapFill,
        SkirtBrim,
        SupportMaterial,
        Ironing,
        Wipe,
        Custom,
        Travel,
        Unknown,
    ];
    let total = counts.iter().sum::<usize>().max(1) as f64;
    order
        .into_iter()
        .map(|f| (f, counts[f as usize]))
        .filter(|(_, count)| *count > 0)
        .map(|(feature, count)| LegendEntry {
            label: format!("{}  {:.1}%", feature.label(), count as f64 * 100.0 / total),
            color: feature_color(feature),
        })
        .collect()
}

pub(crate) fn preview_colors(
    values: impl Iterator<Item = (ExtrusionType, bool, f64)>,
    bounds: crate::types::Bounds,
    mode: ColorMode,
    legend: bool,
    travel: bool,
) -> ((f64, f64), Vec<LegendEntry>) {
    if mode == ColorMode::Feature {
        let mut counts = [0; 14];
        if legend {
            for (feature, is_travel, _) in values {
                if travel || !is_travel {
                    counts[feature as usize] += 1;
                }
            }
        }
        return ((0., 1.), feature_legend(&counts));
    }
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    let mut distinct = Vec::new();
    for (_, is_travel, value) in values {
        if !is_travel
            && value.is_finite()
            && (value > 0. || matches!(mode, ColorMode::Fan | ColorMode::Height))
        {
            low = low.min(value);
            high = high.max(value);
            if legend {
                observe_value(&mut distinct, value, mode);
            }
        }
    }
    let range = if mode == ColorMode::Height {
        (bounds.min[2], bounds.max[2])
    } else if low.is_finite() {
        (low, high)
    } else {
        default_range(mode)
    };
    (
        range,
        if legend {
            scalar_entries(range, mode, distinct)
        } else {
            Vec::new()
        },
    )
}

fn default_range(mode: ColorMode) -> (f64, f64) {
    match mode {
        ColorMode::Fan => (0., 255.),
        ColorMode::Temperature => (0., 300.),
        _ => (0., 1.),
    }
}

// Nine distinct display values are enough to switch to a continuous scale.
fn observe_value(distinct: &mut Vec<f64>, raw: f64, mode: ColorMode) {
    if distinct.len() > 8 {
        return;
    }
    if !raw.is_finite() || (raw <= 0. && !matches!(mode, ColorMode::Fan | ColorMode::Height)) {
        return;
    }
    let value = (display_value(mode, raw) * 10.).round() / 10.;
    if !distinct.contains(&value) {
        distinct.push(value);
    }
}

#[cfg(feature = "gui")]
pub(crate) struct Legends {
    entries: [Vec<LegendEntry>; 6],
    with_travel: Vec<LegendEntry>,
}

#[cfg(feature = "gui")]
impl Legends {
    pub fn collect(data: &ToolpathData) -> (ColorRanges, Self) {
        let mut counts = [0; 14];
        let mut visible = [0; 14];
        let mut distinct: [Vec<f64>; 6] = Default::default();
        let ranges = ColorRanges::collect(data, |s| {
            counts[s.extrusion_type as usize] += 1;
            if !s.is_travel {
                visible[s.extrusion_type as usize] += 1;
                for mode in ColorMode::ALL.into_iter().skip(1) {
                    observe_value(&mut distinct[mode as usize], segment_value(s, mode), mode);
                }
            }
        });
        let with_travel = feature_legend(&counts);
        let entries = ColorMode::ALL.map(|mode| {
            if mode == ColorMode::Feature {
                feature_legend(&visible)
            } else {
                scalar_entries(
                    ranges.range(mode),
                    mode,
                    std::mem::take(&mut distinct[mode as usize]),
                )
            }
        });
        (
            ranges,
            Self {
                entries,
                with_travel,
            },
        )
    }
    pub fn get(&self, mode: ColorMode, travel: bool) -> &[LegendEntry] {
        if mode == ColorMode::Feature && travel {
            &self.with_travel
        } else {
            &self.entries[mode as usize]
        }
    }
}

#[cfg(all(test, feature = "gui"))]
#[test]
fn hiding_travel_keeps_extrusions_with_a_travel_feature_tag() {
    let data = crate::parser::parse_gcode(";TYPE:Travel\nG1 X10 E1\nG0 X11").unwrap();
    let (_, legends) = Legends::collect(&data);
    for show in [true, false] {
        assert_eq!(
            legends.get(ColorMode::Feature, show),
            legend_entries(&data, ColorMode::Feature, show)
        );
    }
}

fn scalar_entries(
    (low, high): (f64, f64),
    mode: ColorMode,
    mut distinct: Vec<f64>,
) -> Vec<LegendEntry> {
    let display_low = display_value(mode, low);
    let display_high = display_value(mode, high);
    let unit = match mode {
        ColorMode::Height => "mm",
        ColorMode::Speed => "mm/s",
        ColorMode::Fan => "%",
        ColorMode::Temperature => "°C",
        ColorMode::Flow => "mm³/s",
        ColorMode::Feature => "",
    };
    distinct.sort_by(f64::total_cmp);
    let values = if (display_high - display_low).abs() < 1e-6 {
        vec![display_low]
    } else if !distinct.is_empty() && distinct.len() <= 8 {
        distinct
    } else {
        (0..8)
            .map(|index| display_low + (display_high - display_low) * index as f64 / 7.0)
            .collect()
    };
    let mut entries = Vec::<LegendEntry>::new();
    for value in values {
        let label = format!("{value:.1} {unit}");
        if entries.iter().any(|entry| entry.label == label) {
            continue;
        }
        let fraction = if display_high > display_low {
            (value - display_low) / (display_high - display_low)
        } else {
            0.5
        };
        entries.push(LegendEntry {
            label,
            color: gradient_color(mode, fraction),
        });
    }
    entries
}
