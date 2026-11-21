//! Command-line interface. Export and statistics work with `--no-default-features`.
use crate::{
    camera::{Camera, CameraPreset},
    load_toolpath,
    renderer::{self, RenderOptions},
    save_png,
    types::ColorMode,
};
use clap::Parser;
use std::{collections::BTreeMap, error::Error, path::PathBuf};

#[derive(Clone, Debug, Parser)]
#[command(
    name = "gcv",
    version,
    about,
    after_help = "Examples:\n  gcv part.gcode\n  gcv part.bgcode -e part.png --color-mode speed\n  gcv part.gcode -e top.png -c top -r 1280x720 --scale 2\n  gcv part.gcode --stats\n\nGUI: drag to orbit; right-drag to pan; wheel to zoom.\nKeys: 1/2/3/4 cameras, T travel, V color, S screenshot, R reset."
)]
pub struct Args {
    /// Path to a .gcode or .bgcode file (format is detected from content)
    pub file: PathBuf,
    /// Launch the interactive desktop viewer (the default)
    #[arg(long, conflicts_with_all = ["export", "stats"])]
    pub gui: bool,
    /// Export a PNG image and exit; no display server or GPU required
    #[arg(long, short = 'e', value_name = "OUTPUT.png", conflicts_with = "stats")]
    pub export: Option<PathBuf>,
    /// Camera preset: isometric, top, front, side
    #[arg(long, short = 'c', default_value = "isometric", value_parser = parse_camera)]
    pub camera: CameraPreset,
    /// Custom azimuth in degrees (may be used independently of elevation)
    #[arg(long, allow_hyphen_values = true, value_parser = finite_number)]
    pub camera_azimuth: Option<f64>,
    /// Custom elevation in degrees, from -90 to 90
    #[arg(long, allow_hyphen_values = true, value_parser = elevation)]
    pub camera_elevation: Option<f64>,
    /// Maximum zero-based Z group to show, inclusive (default: all)
    #[arg(long, short = 'l')]
    pub layer: Option<usize>,
    /// Minimum zero-based Z group to show, inclusive
    #[arg(long, default_value_t = 0)]
    pub min_layer: usize,
    /// Export resolution before scaling, WIDTHxHEIGHT
    #[arg(long, short = 'r', default_value = "1920x1080", value_parser = parse_resolution)]
    pub resolution: Resolution,
    /// Display non-extruding moves
    #[arg(long)]
    pub show_travel: bool,
    /// Hide the print bed and its grid
    #[arg(long)]
    pub no_bed: bool,
    /// feature, height, speed, fan, temperature, flow
    #[arg(long, default_value = "feature")]
    pub color_mode: ColorMode,
    /// Scale the viewport render resolution and PNG export dimensions
    #[arg(long, short = 's', default_value = "1", value_parser = positive_number)]
    pub scale: f64,
    /// Hide the legend in exported PNGs
    #[arg(long)]
    pub no_legend: bool,
    /// Use orthographic projection instead of perspective projection
    #[arg(long)]
    pub orthographic: bool,
    /// Print machine-readable JSON statistics and exit
    #[arg(long)]
    pub stats: bool,
    /// Fail if the parser reports unsupported or approximate commands
    #[arg(long)]
    pub strict: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

fn finite_number(value: &str) -> Result<f64, String> {
    value
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .ok_or_else(|| "expected a finite number".into())
}

fn positive_number(value: &str) -> Result<f64, String> {
    let n = finite_number(value)?;
    if n > 0.0 {
        Ok(n)
    } else {
        Err("expected a number greater than zero".into())
    }
}

fn elevation(value: &str) -> Result<f64, String> {
    let n = finite_number(value)?;
    if (-90.0..=90.0).contains(&n) {
        Ok(n)
    } else {
        Err("elevation must be between -90 and 90 degrees".into())
    }
}

fn parse_camera(value: &str) -> Result<CameraPreset, String> {
    match value {
        "isometric" => Ok(CameraPreset::Isometric),
        "top" => Ok(CameraPreset::Top),
        "front" => Ok(CameraPreset::Front),
        "side" => Ok(CameraPreset::Side),
        _ => Err("expected isometric, top, front or side".into()),
    }
}

fn parse_resolution(value: &str) -> Result<Resolution, String> {
    let (w, h) = value
        .split_once(['x', 'X'])
        .ok_or("expected WIDTHxHEIGHT, e.g. 1920x1080")?;
    let width = w.parse::<u32>().map_err(|_| "invalid image width")?;
    let height = h.parse::<u32>().map_err(|_| "invalid image height")?;
    if width == 0 || height == 0 {
        return Err("image dimensions must be greater than zero".into());
    }
    Ok(Resolution { width, height })
}

impl Args {
    /// Validates scaled sizes before any input is read or image memory allocated.
    pub fn render_options(&self) -> Result<RenderOptions, String> {
        let w = (self.resolution.width as f64 * self.scale).floor();
        let h = (self.resolution.height as f64 * self.scale).floor();
        if !w.is_finite()
            || !h.is_finite()
            || w < 1.0
            || h < 1.0
            || w > u32::MAX as f64
            || h > u32::MAX as f64
        {
            return Err(
                "scaled image dimensions must be positive finite integers within the u32 range"
                    .into(),
            );
        }
        renderer::validate_dimensions(w as u32, h as u32).map_err(|e| e.to_string())?;
        let mut camera = Camera::from_preset(self.camera);
        if let Some(azimuth) = self.camera_azimuth {
            camera.azimuth_deg = azimuth.rem_euclid(360.0);
        }
        if let Some(elevation) = self.camera_elevation {
            camera.elevation_deg = elevation;
        }
        camera.perspective = !self.orthographic;
        Ok(RenderOptions {
            width: w as u32,
            height: h as u32,
            layer_min: self.min_layer,
            layer_max: self.layer,
            show_travel: self.show_travel,
            show_bed: !self.no_bed,
            color_mode: self.color_mode,
            camera,
            show_legend: !self.no_legend,
            ..Default::default()
        })
    }
}

fn check_selection(
    args: &Args,
    layers: usize,
    diagnostics: &[crate::types::Diagnostic],
) -> Result<(), Box<dyn Error>> {
    if args.strict && !diagnostics.is_empty() {
        return Err(format!(
            "strict parsing rejected {} diagnostic(s); first at line {}: {}",
            diagnostics.len(),
            diagnostics[0].line,
            diagnostics[0].message
        )
        .into());
    }
    let last = layers.saturating_sub(1);
    if args.min_layer > last
        || args
            .layer
            .is_some_and(|layer| layer > last || layer < args.min_layer)
    {
        return Err(format!("layer range must satisfy 0 <= min-layer <= layer <= {last}").into());
    }
    Ok(())
}

pub fn run(args: Args) -> Result<(), Box<dyn Error>> {
    let options = args.render_options()?;
    if let Some(destination) = &args.export
        && options.color_mode == ColorMode::Feature
        && !options.show_travel
        && options.layer_min == 0
        && options.layer_max.is_none()
    {
        let print = crate::load_with(&args.file, crate::parser::parse_features)?;
        check_selection(&args, print.layers, &print.diagnostics)?;
        report(print.layers, print.segments, print.bed, &print.diagnostics);
        return export(renderer::render_features(&print, &options)?, destination);
    }
    let toolpath = load_toolpath(&args.file)?;
    check_selection(&args, toolpath.total_layers(), &toolpath.diagnostics)?;
    if args.stats {
        let mut features = BTreeMap::<&str, usize>::new();
        let mut extrusion_segments = 0;
        let mut travel_segments = 0;
        let mut extrusion_distance = 0.0;
        let mut travel_distance = 0.0;
        for segment in toolpath.segments() {
            *features.entry(segment.extrusion_type.label()).or_default() += 1;
            if segment.is_travel {
                travel_segments += 1;
                travel_distance += segment.length();
            } else {
                extrusion_segments += 1;
                extrusion_distance += segment.length();
            }
        }
        let stats = serde_json::json!({
            "schema_version": 1,
            "file": args.file.to_string_lossy(),
            "total_layers": toolpath.total_layers(), "total_segments": toolpath.total_segments(),
            "extrusion_segments": extrusion_segments, "travel_segments": travel_segments,
            "extrusion_distance_mm": extrusion_distance, "travel_distance_mm": travel_distance,
            "bed": {"x": toolpath.bed_x, "y": toolpath.bed_y, "max_z": toolpath.max_z},
            "bounds": {"min": toolpath.bounds.min, "max": toolpath.bounds.max},
            "features": features,
            "layers": toolpath.layers.iter().map(|l| serde_json::json!({"index":l.layer_index,"z_height":l.z_height,"segments":l.segments.len()})).collect::<Vec<_>>(),
            "diagnostics": toolpath.diagnostics.iter().map(|d| serde_json::json!({"line":d.line,"message":d.message})).collect::<Vec<_>>()
        });
        println!("{}", serde_json::to_string_pretty(&stats)?);
        return Ok(());
    }
    report(
        toolpath.total_layers(),
        toolpath.total_segments(),
        [toolpath.bed_x, toolpath.bed_y],
        &toolpath.diagnostics,
    );
    if let Some(destination) = &args.export {
        let frame = renderer::render(&toolpath, &options)?;
        return export(frame, destination);
    }
    #[cfg(feature = "gui")]
    {
        crate::gui::run_app(toolpath, args.file, options, args.scale as f32)
            .map_err(|e| format!("desktop viewer: {e}"))?;
        Ok(())
    }
    #[cfg(not(feature = "gui"))]
    {
        Err("this build has no GUI; use --export output.png / --stats, or rebuild with the default gui feature".into())
    }
}

fn report(
    layers: usize,
    segments: usize,
    [x, y]: [f64; 2],
    diagnostics: &[crate::types::Diagnostic],
) {
    eprintln!("{layers} layers, {segments} segments; bed {x} x {y} mm");
    for d in diagnostics.iter().take(20) {
        eprintln!("warning (line {}): {}", d.line, d.message);
    }
    if diagnostics.len() > 20 {
        eprintln!(
            "{} further diagnostics; use --stats to inspect all",
            diagnostics.len() - 20
        );
    }
}

fn export(frame: renderer::Frame, destination: &std::path::Path) -> Result<(), Box<dyn Error>> {
    save_png(&frame, destination)?;
    println!(
        "Saved: {} ({} x {})",
        destination.display(),
        frame.width,
        frame.height
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_cli_flags_and_scale() {
        let args = Args::try_parse_from([
            "gcv",
            "a.gcode",
            "-e",
            "a.png",
            "-c",
            "top",
            "-l",
            "2",
            "-r",
            "640x480",
            "-s",
            "2",
            "--show-travel",
            "--no-bed",
            "--color-mode",
            "flow",
        ])
        .unwrap();
        let options = args.render_options().unwrap();
        assert_eq!((options.width, options.height), (1280, 960));
        assert_eq!(options.color_mode, ColorMode::Flow);
        assert!(options.show_travel);
        assert!(!options.show_bed);
    }

    #[test]
    fn rejects_bad_dimensions_and_nonfinite_controls() {
        for value in ["0x100", "axb", "10", "-1x40", "10x10x10"] {
            assert!(Args::try_parse_from(["g", "a", "--resolution", value]).is_err());
        }
        for value in ["NaN", "inf", "0", "-1"] {
            assert!(Args::try_parse_from(["g", "a", "--scale", value]).is_err());
        }
        let args =
            Args::try_parse_from(["g", "a", "-s", "100000000000000000000000000000000000000000"])
                .unwrap();
        assert!(args.render_options().is_err());
    }

    #[test]
    fn accepts_independent_custom_angles_and_rejects_conflicting_modes() {
        let args = Args::try_parse_from(["g", "a", "--camera-azimuth", "-45"]).unwrap();
        assert_eq!(args.render_options().unwrap().camera.azimuth_deg, 315.0);
        assert!(Args::try_parse_from(["g", "a", "--gui", "-e", "x.png"]).is_err());
        assert!(Args::try_parse_from(["g", "a", "--camera-elevation", "91"]).is_err());
    }
}
