use crate::{
    geometry::{DetailedLayers, DetailedPrint},
    types::Diagnostic,
};
use serde::{Serialize, Serializer};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{self, BufWriter, Write},
    path::Path,
};

#[derive(Serialize)]
struct Stats<'a> {
    bed: Value,
    bounds: Value,
    diagnostics: &'a [Diagnostic],
    extrusion_distance_mm: f64,
    extrusion_segments: usize,
    features: BTreeMap<&'static str, usize>,
    file: &'a str,
    #[serde(serialize_with = "write_layers")]
    layers: &'a DetailedLayers,
    schema_version: u8,
    total_layers: usize,
    total_segments: usize,
    travel_distance_mm: f64,
    travel_segments: usize,
}

fn write_layers<S: Serializer>(layers: &DetailedLayers, s: S) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Layer {
        index: usize,
        segments: usize,
        z_height: f64,
    }
    s.collect_seq(
        layers
            .layer_ranges()
            .enumerate()
            .map(|(index, (z_height, range))| Layer {
                index,
                segments: range.len(),
                z_height,
            }),
    )
}

pub(super) fn write(print: &DetailedPrint, file: &Path, output: impl Write) -> io::Result<()> {
    let mut stats = Stats {
        bed: json!({"max_z": print.max_z, "x": print.bed_x, "y": print.bed_y}),
        bounds: json!({"max": print.bounds.max, "min": print.bounds.min}),
        diagnostics: &print.diagnostics,
        extrusion_distance_mm: 0.,
        extrusion_segments: 0,
        features: BTreeMap::new(),
        file: &file.to_string_lossy(),
        layers: &print.layers,
        schema_version: 1,
        total_layers: print.total_layers(),
        total_segments: print.total_segments(),
        travel_distance_mm: 0.,
        travel_segments: 0,
    };
    for segment in print.segments() {
        *stats
            .features
            .entry(segment.extrusion_type.label())
            .or_default() += 1;
        if segment.is_travel {
            stats.travel_segments += 1;
            stats.travel_distance_mm += segment.length();
        } else {
            stats.extrusion_segments += 1;
            stats.extrusion_distance_mm += segment.length();
        }
    }
    let mut out = BufWriter::new(output);
    serde_json::to_writer_pretty(&mut out, &stats)?;
    out.write_all(b"\n")?;
    out.flush()
}
