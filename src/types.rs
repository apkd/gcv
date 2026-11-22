//! Renderer-independent data shared by the parser, CLI and desktop viewer.
use std::{fmt, str::FromStr};

/// Slicer feature classification. Travel is separate from an unknown extrusion.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ExtrusionType {
    ExternalPerimeter,
    Perimeter,
    Infill,
    SolidInfill,
    TopSolidInfill,
    BridgeInfill,
    SupportMaterial,
    SkirtBrim,
    Custom,
    GapFill,
    Ironing,
    Travel,
    Wipe,
    Unknown,
}

impl ExtrusionType {
    pub const ALL: [Self; 14] = [
        Self::ExternalPerimeter,
        Self::Perimeter,
        Self::Infill,
        Self::SolidInfill,
        Self::TopSolidInfill,
        Self::BridgeInfill,
        Self::SupportMaterial,
        Self::SkirtBrim,
        Self::Custom,
        Self::GapFill,
        Self::Ironing,
        Self::Travel,
        Self::Wipe,
        Self::Unknown,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::ExternalPerimeter => "External perimeter",
            Self::Perimeter => "Perimeter",
            Self::Infill => "Infill",
            Self::SolidInfill => "Solid infill",
            Self::TopSolidInfill => "Top solid infill",
            Self::BridgeInfill => "Bridge infill",
            Self::SupportMaterial => "Support material",
            Self::SkirtBrim => "Skirt / brim",
            Self::Custom => "Custom",
            Self::GapFill => "Gap fill",
            Self::Ironing => "Ironing",
            Self::Travel => "Travel",
            Self::Wipe => "Wipe",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum ColorMode {
    #[default]
    Feature,
    Height,
    Speed,
    Fan,
    Temperature,
    Flow,
}

impl ColorMode {
    pub const ALL: [Self; 6] = [
        Self::Feature,
        Self::Height,
        Self::Speed,
        Self::Fan,
        Self::Temperature,
        Self::Flow,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Feature => "feature",
            Self::Height => "height",
            Self::Speed => "speed",
            Self::Fan => "fan",
            Self::Temperature => "temperature",
            Self::Flow => "flow",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Feature => "Feature type",
            Self::Height => "Height (mm)",
            Self::Speed => "Speed (mm/s)",
            Self::Fan => "Fan speed (%)",
            Self::Temperature => "Temperature (°C)",
            Self::Flow => "Volumetric flow (mm³/s)",
        }
    }
}

impl fmt::Display for ColorMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for ColorMode {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|mode| mode.name().eq_ignore_ascii_case(value))
            .ok_or_else(|| format!("unknown color mode {value:?}; expected feature, height, speed, fan, temperature or flow"))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub start: [f64; 3],
    pub end: [f64; 3],
    pub extrusion_type: ExtrusionType,
    pub layer_index: usize,
    pub is_travel: bool,
    /// Requested feed rate in mm/min, without acceleration or firmware overrides.
    pub feedrate: f64,
    /// Fan PWM in [0, 255].
    pub fan_speed: f64,
    /// Target hotend temperature in °C.
    pub temperature: f64,
    /// Estimated deposited volume per second at the requested constant feed rate.
    pub volumetric_flow: f64,
}

impl Segment {
    pub fn length(&self) -> f64 {
        self.start
            .iter()
            .zip(self.end)
            .map(|(a, b)| (b - a).powi(2))
            .sum::<f64>()
            .sqrt()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerData<'a> {
    pub z_height: f64,
    pub layer_index: usize,
    pub segments: &'a [Segment],
}

/// Contiguous toolpaths with an exclusive segment end for each Z group.
/// A helical arc may have only one segment per group.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LayerTable {
    ends: Vec<(f64, usize)>,
    segments: Vec<Segment>,
}

impl LayerTable {
    pub fn len(&self) -> usize {
        self.ends.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }
    pub fn get(&self, layer_index: usize) -> Option<LayerData<'_>> {
        let &(z_height, end) = self.ends.get(layer_index)?;
        Some(LayerData {
            z_height,
            layer_index,
            segments: &self.segments[self.start(layer_index)..end],
        })
    }
    pub fn iter(&self) -> impl ExactSizeIterator<Item = LayerData<'_>> {
        (0..self.len()).map(|i| self.get(i).unwrap())
    }
    fn start(&self, i: usize) -> usize {
        if i == 0 { 0 } else { self.ends[i - 1].1 }
    }
    pub fn range(&self, layers: std::ops::Range<usize>) -> &[Segment] {
        &self.segments[self.start(layers.start)..self.start(layers.end)]
    }
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }
    pub fn segments_mut(&mut self) -> &mut [Segment] {
        &mut self.segments
    }
    pub(crate) fn push(&mut self, segment: Segment) {
        self.segments.push(segment);
    }
    pub(crate) fn finish(&mut self, z: f64) {
        self.ends.push((z, self.segments.len()));
    }
    pub(crate) fn append(&mut self, part: Self) {
        let layers = self.ends.len();
        let segments = self.segments.len();
        self.ends
            .extend(part.ends.into_iter().map(|(z, end)| (z, end + segments)));
        self.segments.extend(part.segments.into_iter().map(|mut s| {
            s.layer_index += layers;
            s
        }));
    }
}

impl<I: IntoIterator<Item = Segment>> FromIterator<(f64, I)> for LayerTable {
    fn from_iter<T: IntoIterator<Item = (f64, I)>>(layers: T) -> Self {
        let mut table = Self::default();
        for (z, segments) in layers {
            table.segments.extend(segments);
            table.finish(z);
        }
        table
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Bounds {
    pub min: [f64; 3],
    pub max: [f64; 3],
}

impl Bounds {
    pub fn center(self) -> [f64; 3] {
        std::array::from_fn(|i| (self.min[i] + self.max[i]) * 0.5)
    }
    pub fn size(self) -> [f64; 3] {
        std::array::from_fn(|i| (self.max[i] - self.min[i]).max(0.0))
    }
    pub fn include(&mut self, point: [f64; 3]) {
        for (i, value) in point.into_iter().enumerate() {
            self.min[i] = self.min[i].min(value);
            self.max[i] = self.max[i].max(value);
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    /// One-based input line number; 0 denotes a whole-file diagnostic.
    pub line: usize,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolpathData {
    pub layers: LayerTable,
    pub bed_x: f64,
    pub bed_y: f64,
    pub max_z: f64,
    pub bounds: Bounds,
    pub diagnostics: Vec<Diagnostic>,
}

impl ToolpathData {
    pub fn total_layers(&self) -> usize {
        self.layers.len()
    }
    pub fn total_segments(&self) -> usize {
        self.layers.segments.len()
    }
    pub fn segments(&self) -> impl Iterator<Item = &Segment> {
        self.layers.segments.iter()
    }
}
