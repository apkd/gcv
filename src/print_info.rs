//! Optional slicer estimates. Geometry, machine limits and profile defaults are not job statistics.
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

#[derive(Default, PartialEq, Clone, Copy)]
enum Family {
    #[default]
    Unknown,
    Prusa,
    Qidi,
    Orca,
    Bambu,
    Cura,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Section {
    #[default]
    Text,
    Header,
    Config,
    Executable,
    Thumbnail,
    ThumbnailBlock,
    BinaryFile,
    BinaryPrint,
    BinaryPrinter,
}

#[derive(Default)]
pub(crate) struct Collector {
    family: Family,
    section: Section,
    /// Skip the flattened metadata prefix when interpreting the decoded toolpath.
    pub(crate) body_offset: usize,
    fields: BTreeMap<String, (u8, String)>,
    cura_layers: Option<u32>,
    cura_groups: usize,
    initial_tools: BTreeMap<u8, f64>,
    awaiting_start_tool: bool,
    start_tool: Option<u8>,
    chamber_request: Option<f64>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct PrintInfo {
    pub time: Option<u64>,
    pub model_time: bool,
    pub filament: Option<String>,
    pub swatch: Option<[u8; 3]>,
    pub consumption: Option<(f64, &'static str)>,
    pub nozzle: Targets,
    pub bed: Targets,
    pub chamber: Option<f64>,
    pub layers: Option<u32>,
    pub nominal: Option<f64>,
    pub top_z: Option<f64>,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Targets {
    pub first: Option<f64>,
    pub later: Option<f64>,
}

const PLATES: &[(&str, &str)] = &[
    ("Textured PEI Plate", "textured_plate_temp"),
    ("Cool Plate", "cool_plate_temp"),
    ("Engineering Plate", "eng_plate_temp"),
    ("High Temp Plate", "hot_plate_temp"),
    ("Supertack Plate", "supertack_plate_temp"),
    ("Textured Cool Plate", "textured_cool_plate_temp"),
];

impl Collector {
    pub fn binary_field(&mut self, kind: u16, key: &str, value: &str) {
        let section = match kind {
            0 => Section::BinaryFile,
            2 => Section::Config,
            3 => Section::BinaryPrinter,
            4 => Section::BinaryPrint,
            _ => return,
        };
        self.field(section, key, value);
    }

    pub fn binary_json_field(
        &mut self,
        kind: u16,
        key: &str,
        json: &serde_json::Value,
        text: &str,
    ) {
        if matches!(
            key,
            "filament_settings_id" | "filament_type" | "filament_colour"
        ) && let Some(values) = json.as_array()
        {
            if text.len() <= 4096 && values.iter().all(|v| v.is_string()) {
                self.binary_field(
                    kind,
                    key,
                    &values
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(";"),
                );
            }
        } else {
            self.binary_field(kind, key, text);
        }
    }

    fn generator(&mut self, name: &str) {
        let name = name.trim_start();
        for (prefix, family) in [
            ("QIDIStudio", Family::Qidi),
            ("OrcaSlicer", Family::Orca),
            ("BambuStudio", Family::Bambu),
            ("Bambu Studio", Family::Bambu),
            ("PrusaSlicer", Family::Prusa),
            ("SuperSlicer", Family::Prusa),
            ("Cura", Family::Cura),
        ] {
            if name.starts_with(prefix) {
                self.family = family;
                break;
            }
        }
    }

    pub fn line(&mut self, raw: &str) {
        let line = raw.trim_start();
        let Some(s) = line.strip_prefix(';').map(str::trim) else {
            // Griffin can put other tools at standby temperatures. The leading
            // T command following its header identifies the starting tool.
            if self.awaiting_start_tool && !line.is_empty() {
                self.awaiting_start_tool = false;
                self.start_tool = line
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .strip_prefix('T')
                    .and_then(|s| s.parse().ok());
            }
            // A positive executable chamber command requests heat. Capability flags,
            // start-code templates and shutdown commands do not establish that.
            if matches!(
                self.family,
                Family::Qidi | Family::Bambu | Family::Orca | Family::Prusa
            ) && line.starts_with("M1")
                && matches!(self.section, Section::Text | Section::Executable)
            {
                let mut words = line
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .split_ascii_whitespace();
                if matches!(words.next(), Some("M141" | "M191"))
                    && let Some(t) = words.find_map(|w| w.strip_prefix('S').and_then(positive))
                {
                    self.chamber_request = Some(t);
                }
            }
            return;
        };
        if self.section == Section::ThumbnailBlock {
            if s == "THUMBNAIL_BLOCK_END" {
                self.section = Section::Text;
            }
            return;
        }
        if self.section == Section::Thumbnail {
            if s.starts_with("thumbnail") && s.ends_with(" end") {
                self.section = Section::Text;
            }
            return;
        }
        match s {
            "END_OF_HEADER" => {
                self.section = Section::Text;
                self.awaiting_start_tool = self.family == Family::Cura;
                return;
            }
            "HEADER_BLOCK_START" | "START_OF_HEADER" => {
                self.section = Section::Header;
                return;
            }
            "CONFIG_BLOCK_START" | "prusaslicer_config = begin" | "SuperSlicer_config = begin" => {
                self.section = Section::Config;
                return;
            }
            "THUMBNAIL_BLOCK_START" => {
                self.section = Section::ThumbnailBlock;
                return;
            }
            "EXECUTABLE_BLOCK_START" => {
                self.section = Section::Executable;
                return;
            }
            "HEADER_BLOCK_END"
            | "CONFIG_BLOCK_END"
            | "EXECUTABLE_BLOCK_END"
            | "prusaslicer_config = end"
            | "SuperSlicer_config = end" => {
                self.section = Section::Text;
                return;
            }
            _ => {}
        }
        if s.starts_with("thumbnail") {
            if s.contains(" begin ") {
                self.section = Section::Thumbnail;
            }
            return;
        }
        if matches!(self.section, Section::Thumbnail | Section::Executable) || s.len() > 4096 {
            return;
        }
        if matches!(self.section, Section::Text | Section::Header) {
            self.generator(
                s.strip_prefix("generated by ")
                    .or_else(|| s.strip_prefix("Generated with "))
                    .unwrap_or(s),
            );
        }
        // Split only at the first delimiter, never inside a quoted profile or a script setting.
        if let Some(i) = s.find(['=', ':']) {
            let (key, value) = (s[..i].trim_end(), s[i + 1..].trim());
            if key == "model printing time"
                && let Some((model, total)) = value.split_once("; total estimated time:")
            {
                self.field(self.section, key, model.trim());
                self.field(self.section, "total estimated time", total.trim());
            } else {
                self.field(self.section, key, value);
            }
        }
    }

    fn field(&mut self, section: Section, key: &str, value: &str) {
        use Section::*;
        if value.len() > 4096 || value.is_empty() || value.contains(['\n', '\r']) {
            return;
        }
        if key == "Producer" || key == "GENERATOR.NAME" {
            self.generator(value);
            return;
        }
        if section == BinaryFile {
            return;
        }
        if self.family == Family::Unknown
            && key == "FLAVOR"
            && matches!(
                value,
                "Marlin" | "Marlin(Volumetric)" | "UltiGCode" | "Griffin" | "Cheetah" | "RepRap"
            )
        {
            self.family = Family::Cura;
        }
        if key == "LAYER_COUNT" && section != Config {
            self.cura_groups += 1;
            self.cura_layers = value.parse().ok();
            return;
        }
        if let Some(tool) = key
            .strip_prefix("EXTRUDER_TRAIN.")
            .and_then(|v| v.strip_suffix(".INITIAL_TEMPERATURE"))
            && let Ok(index) = tool.parse::<u8>()
            && let Some(t) = number(value)
        {
            self.initial_tools.insert(index, t);
            return;
        }
        let stat = match key {
            "estimated printing time (normal mode)"
            | "model printing time"
            | "total estimated time"
            | "total filament weight [g]"
            | "total filament used [g]"
            | "filament used [g]"
            | "total filament length [mm]"
            | "filament used [mm]"
            | "total layer number"
            | "total layers count"
            | "max_layer_z"
            | "max_z_height"
            | "filament"
            | "TIME"
            | "PRINT.TIME"
            | "Filament used"
            | "Layer height"
            | "BUILD_PLATE.INITIAL_TEMPERATURE" => true,
            "filament_settings_id"
            | "filament_type"
            | "filament_colour"
            | "layer_height"
            | "used_extruders"
            | "nozzle_temperature"
            | "nozzle_temperature_initial_layer"
            | "temperature"
            | "first_layer_temperature"
            | "bed_temperature"
            | "first_layer_bed_temperature"
            | "bed_temperature_extruder"
            | "curr_bed_type"
            | "chamber_temperature"
            | "chamber_temperatures"
            | "activate_chamber_temp_control" => false,
            _ if PLATES
                .iter()
                .any(|(_, base)| key.strip_suffix("_initial_layer").unwrap_or(key) == *base) =>
            {
                false
            }
            _ => return,
        };
        let rank = match (stat, section) {
            (true, Config) | (false, BinaryPrint) => return,
            (true, BinaryPrint) | (false, Config) => 3,
            (_, BinaryPrinter) => 2,
            _ => 1,
        };
        if self.fields.get(key).is_none_or(|v| rank > v.0) {
            self.fields.insert(key.into(), (rank, value.into()));
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(|v| v.1.as_str())
    }

    fn cura(&self) -> PrintInfo {
        let get = |k: &str| self.get(k);
        let seconds = get("PRINT.TIME")
            .or(get("TIME"))
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&t| t > 0);
        let length = get("Filament used")
            .and_then(|s| numbers(s, |v| number(v.strip_suffix('m')?)))
            .and_then(|v| total(&v));
        PrintInfo {
            time: seconds,
            consumption: length.map(|m| (m, "M")),
            layers: self.cura_layers.filter(|&n| n > 0 && self.cura_groups == 1),
            nominal: get("Layer height").and_then(positive),
            nozzle: Targets {
                first: self
                    .start_tool
                    .and_then(|i| self.initial_tools.get(&i).copied())
                    .or_else(|| {
                        self.initial_tools
                            .values()
                            .next()
                            .copied()
                            .filter(|_| self.initial_tools.len() == 1)
                    })
                    .filter(|t| *t > 0.),
                later: None,
            },
            bed: Targets {
                first: get("BUILD_PLATE.INITIAL_TEMPERATURE").and_then(positive),
                later: None,
            },
            ..Default::default()
        }
    }

    fn consumption(&self) -> Option<(f64, &'static str)> {
        let sum = |key| total(&numbers(self.get(key)?, number)?);
        let header_first = matches!(self.family, Family::Qidi | Family::Bambu);
        let weight = if header_first {
            [
                "total filament weight [g]",
                "total filament used [g]",
                "filament used [g]",
            ]
        } else {
            [
                "total filament used [g]",
                "filament used [g]",
                "total filament weight [g]",
            ]
        };
        let length = if header_first {
            ["total filament length [mm]", "filament used [mm]"]
        } else {
            ["filament used [mm]", "total filament length [mm]"]
        };
        weight
            .iter()
            .find_map(|k| sum(k))
            .map(|v| (v, "G"))
            .or_else(|| length.iter().find_map(|k| sum(k)).map(|v| (v / 1000., "M")))
    }

    fn used_slots(&self) -> Option<Vec<usize>> {
        // Header quantities can be packed; only the footer vectors preserve zero-use gaps.
        let ids = |s: &str, sep, base| -> Option<Vec<usize>> {
            let mut ids = s
                .split(sep)
                .map(|s| s.trim().parse::<usize>().ok()?.checked_sub(base))
                .collect::<Option<Vec<_>>>()?;
            ids.sort_unstable();
            ids.dedup();
            Some(ids)
        };
        if let Some(s) = self.get("filament") {
            return Some(ids(s, ',', 1).unwrap_or_default());
        }
        if let Some(s) = self.get("used_extruders") {
            return Some(ids(s, ';', 0).unwrap_or_default());
        }
        ["filament used [mm]", "filament used [g]"]
            .iter()
            .find_map(|k| {
                let v = numbers(self.get(k)?, number)?;
                Some(
                    v.iter()
                        .enumerate()
                        .filter_map(|(i, &n)| (n > 0.).then_some(i))
                        .collect::<Vec<_>>(),
                )
                .filter(|v| !v.is_empty())
            })
    }

    fn selected_filament(&self) -> (Option<usize>, Option<String>, Option<[u8; 3]>) {
        let get = |k: &str| self.get(k);
        let used = self.used_slots();
        let profiles = get("filament_settings_id").and_then(strings);
        let materials = get("filament_type").and_then(strings);
        let colors = get("filament_colour").and_then(strings);
        let slot = match used.as_deref() {
            Some([i]) => Some(*i),
            None if [profiles.as_ref(), materials.as_ref(), colors.as_ref()]
                .into_iter()
                .flatten()
                .all(|v| v.len() <= 1)
                && ["temperature", "nozzle_temperature", "bed_temperature"]
                    .into_iter()
                    .all(|k| get(k).is_none_or(|s| !s.contains(','))) =>
            {
                Some(0)
            }
            _ => None,
        };
        let pick = |v: Option<Vec<String>>| -> Option<String> {
            v?.into_iter()
                .nth(slot?)
                .filter(|s| !s.is_empty() && s != "nil")
        };
        let filament = pick(profiles).or_else(|| pick(materials)).or_else(|| {
            used.as_ref()
                .filter(|v| v.len() > 1)
                .map(|v| format!("{} FILAMENTS", v.len()))
        });
        let swatch = pick(colors).as_deref().and_then(rgb);
        (slot, filament, swatch)
    }

    fn temperatures(&self, slot: Option<usize>) -> (Targets, Targets, Option<f64>) {
        let get = |k: &str| self.get(k);
        let target = |key: &str, i: Option<usize>| get(key)?.split(',').nth(i?).and_then(positive);
        let pair = |first: &str, later: &str, i| Targets {
            first: target(first, i),
            later: target(later, i),
        };
        let fork = matches!(self.family, Family::Qidi | Family::Orca | Family::Bambu);
        let nozzle = if fork
            && (get("nozzle_temperature").is_some()
                || get("nozzle_temperature_initial_layer").is_some())
        {
            pair(
                "nozzle_temperature_initial_layer",
                "nozzle_temperature",
                slot,
            )
        } else {
            pair("first_layer_temperature", "temperature", slot)
        };
        let bed = if fork && let Some(plate) = get("curr_bed_type") {
            PLATES
                .iter()
                .find(|(name, _)| *name == plate)
                .map(|(_, key)| pair(&format!("{key}_initial_layer"), key, slot))
                .unwrap_or_default()
        } else {
            let bed_slot = get("bed_temperature_extruder")
                .and_then(|v| v.parse::<usize>().ok()?.checked_sub(1))
                .or(slot);
            pair("first_layer_bed_temperature", "bed_temperature", bed_slot)
        };
        let chamber = self.chamber_request.or_else(|| {
            (target("activate_chamber_temp_control", slot) == Some(1.))
                .then(|| {
                    target("chamber_temperature", slot)
                        .or_else(|| target("chamber_temperatures", slot))
                })
                .flatten()
        });
        (nozzle, bed, chamber)
    }

    pub fn finish(&self) -> PrintInfo {
        if self.family == Family::Cura {
            return self.cura();
        }
        if self.family == Family::Unknown && self.body_offset == 0 {
            return PrintInfo::default();
        }
        let get = |k: &str| self.get(k);
        let full_time = [
            "total estimated time",
            "estimated printing time (normal mode)",
        ]
        .iter()
        .find_map(|k| duration(get(k)?));
        let time = full_time.or_else(|| duration(get("model printing time")?));
        let (slot, filament, swatch) = self.selected_filament();
        let (nozzle, bed, chamber) = self.temperatures(slot);
        PrintInfo {
            time,
            model_time: full_time.is_none() && time.is_some(),
            filament,
            swatch,
            consumption: self.consumption(),
            nozzle,
            bed,
            chamber,
            layers: ["total layer number", "total layers count"]
                .iter()
                .find_map(|k| get(k)?.parse().ok().filter(|&n| n > 0)),
            nominal: get("layer_height").and_then(positive),
            top_z: ["max_z_height", "max_layer_z"]
                .iter()
                .find_map(|k| positive(get(k)?)),
        }
    }
}

fn number(s: &str) -> Option<f64> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.)
}
fn positive(s: &str) -> Option<f64> {
    number(s).filter(|v| *v > 0.)
}
fn numbers(s: &str, parse: impl Fn(&str) -> Option<f64>) -> Option<Vec<f64>> {
    s.split(',').map(|v| parse(v.trim())).collect()
}
fn total(v: &[f64]) -> Option<f64> {
    let sum = v.iter().sum::<f64>();
    (sum.is_finite() && sum > 0.).then_some(sum)
}

fn rgb(s: &str) -> Option<[u8; 3]> {
    let s = s.strip_prefix('#')?;
    if s.len() != 6 || !s.is_ascii() {
        return None;
    }
    Some([
        u8::from_str_radix(&s[..2], 16).ok()?,
        u8::from_str_radix(&s[2..4], 16).ok()?,
        u8::from_str_radix(&s[4..], 16).ok()?,
    ])
}

// Slic3r string lists preserve empty slots and quote names containing semicolons.
fn strings(s: &str) -> Option<Vec<String>> {
    let mut values = Vec::new();
    let mut item = String::new();
    let mut quoted = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => quoted = !quoted,
            '\\' if quoted => item.push(match chars.next()? {
                'n' | 'r' | 't' => ' ',
                c => c,
            }),
            ';' if !quoted => {
                values.push(item.trim().to_owned());
                item.clear();
            }
            c => item.push(c),
        }
    }
    if quoted {
        return None;
    }
    values.push(item.trim().to_owned());
    Some(values)
}

fn duration(mut s: &str) -> Option<u64> {
    let mut seconds = 0u64;
    while !s.trim_start().is_empty() {
        s = s.trim_start();
        let n = s.find(|c: char| !c.is_ascii_digit())?;
        let count = s[..n].parse::<u64>().ok()?;
        let (unit, rest) = s[n..].trim_start().split_at_checked(1)?;
        let mul = match unit {
            "d" => 86400,
            "h" => 3600,
            "m" => 60,
            "s" => 1,
            _ => return None,
        };
        seconds = seconds.checked_add(count.checked_mul(mul)?)?;
        s = rest;
    }
    (seconds > 0).then_some(seconds)
}
