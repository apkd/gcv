use gcv::parser::{parse_gcode, parse_type_comment};
use gcv::types::{ColorMode, ExtrusionType};

fn near(a: f64, b: f64) {
    assert!((a - b).abs() < 1e-7, "{a} != {b}");
}

#[test]
fn absolute_relative_reset_and_units() {
    let data = parse_gcode("G90\nM82\nG1 X10 Y20 E1 F600\nG91\nM83\nG1 X5 Y-2 E0.5\nG92 E0\nG90\nM82\nG1 X20 E1\nG20\nG1 X1 E2 F60\nG21\nG1 X30 E51.8").unwrap();
    let moves: Vec<_> = data.segments().collect();
    assert_eq!(moves.len(), 5);
    assert_eq!(moves[0].end, [10.0, 20.0, 0.0]);
    assert_eq!(moves[1].start, moves[0].end);
    assert_eq!(moves[1].end, [15.0, 18.0, 0.0]);
    near(
        moves[2].volumetric_flow,
        std::f64::consts::PI * (1.75_f64 / 2.0).powi(2) * 2.0,
    );
    near(moves[3].end[0], 25.4);
    near(moves[3].feedrate, 1524.0);
    assert!(moves.iter().all(|s| !s.is_travel));
}

#[test]
fn g90_and_g91_do_not_override_extrusion_mode() {
    let data = parse_gcode("M83\nG90\nG1 X10 E1\nG1 X20 E1\nG91\nG1 X10 E1").unwrap();
    assert!(data.segments().all(|s| !s.is_travel));
}

#[test]
fn zero_based_z_groups_preserve_header_and_hops() {
    let data =
        parse_gcode("; header\nG1 Z0.2\nG1 X10 E1\nG1 Z0.6\nG0 X20\nG1 Z0.2\nG1 X30 E2\nG1 Z1")
            .unwrap();
    assert_eq!(
        data.layers.iter().map(|l| l.z_height).collect::<Vec<_>>(),
        vec![0.0, 0.2, 0.6, 0.2, 1.0]
    );
    assert!(data.layers.get(0).unwrap().segments.is_empty());
    for (i, layer) in data.layers.iter().enumerate() {
        assert_eq!(i, layer.layer_index);
        assert!(layer.segments.iter().all(|s| s.layer_index == i));
    }
    assert_eq!(data.total_segments(), 7);
    assert!(data.layers.get(1).unwrap().segments[0].is_travel);
}

#[test]
fn first_line_z_has_real_height() {
    let data = parse_gcode("G1 Z0.2\nG1 X10 E1").unwrap();
    assert_eq!(data.total_layers(), 1);
    near(data.layers.get(0).unwrap().z_height, 0.2);
}

#[test]
fn empty_and_comment_only_files_are_valid() {
    let empty = parse_gcode("").unwrap();
    assert_eq!(empty.total_layers(), 0);
    assert_eq!(empty.total_segments(), 0);
    assert_eq!(empty.bounds.max, [250.0, 210.0, 220.0]);
    let comments = parse_gcode("; hello\nM104 S210").unwrap();
    assert_eq!(comments.total_layers(), 1);
    assert_eq!(comments.total_segments(), 0);
}

#[test]
fn retraction_and_unretraction_without_motion_do_not_draw() {
    let data = parse_gcode("M83\nG1 E-2\nG1 E2\nG1 X10 E1\nG1 X20 E-1\nG1 X30 E0").unwrap();
    let moves: Vec<_> = data.segments().collect();
    assert_eq!(moves.len(), 3);
    assert!(!moves[0].is_travel);
    assert!(moves[1].is_travel && moves[2].is_travel);
    assert_eq!(moves[1].extrusion_type, ExtrusionType::Travel);
}

#[test]
fn comments_compact_words_line_numbers_and_checksum() {
    let data=parse_gcode("N1 g1x10y20E1f600*42\n; TYPE:Infill\nG1 X20 (X999 (Y999)) Y30 E2 ; X2000\nG1X30Y30E3\nX40Y30E4").unwrap();
    assert_eq!(data.total_segments(), 4);
    let moves: Vec<_> = data.segments().collect();
    assert_eq!(moves[0].end, [10.0, 20.0, 0.0]);
    assert_eq!(moves[1].end, [20.0, 30.0, 0.0]);
    assert_eq!(moves[2].end, [30.0, 30.0, 0.0]);
    assert_eq!(moves[3].end, [40.0, 30.0, 0.0]);
    assert_eq!(moves[1].extrusion_type, ExtrusionType::Infill);
}

#[test]
fn scientific_notation_preserves_uppercase_extrusion_word() {
    let data = parse_gcode("G1X1e1Y0E1F6e2\nG1X20e1.5").unwrap();
    let s: Vec<_> = data.segments().collect();
    near(s[0].end[0], 10.0);
    near(s[0].feedrate, 600.0);
    assert!(!s[1].is_travel);
    near(s[1].end[0], 20.0);
}

#[test]
fn fan_temperature_feed_and_feature_persist() {
    let data=parse_gcode("; TYPE:External perimeter\nM104 S215\nM106 s127.5\nM83\nG1 X10 E1 F600\nG1 X20 E1\nM109 R205\nM107\n; TYPE:Perimeter\nG1 X30 E1\nM106\nG1 X40 E1").unwrap();
    let s: Vec<_> = data.segments().collect();
    near(s[0].temperature, 215.0);
    near(s[1].temperature, 215.0);
    near(s[0].fan_speed, 127.5);
    near(s[1].feedrate, 600.0);
    assert_eq!(s[0].extrusion_type, ExtrusionType::ExternalPerimeter);
    near(s[2].temperature, 205.0);
    near(s[2].fan_speed, 0.0);
    assert_eq!(s[2].extrusion_type, ExtrusionType::Perimeter);
    near(s[3].fan_speed, 255.0);
    near(
        s[0].volumetric_flow,
        std::f64::consts::PI * (1.75_f64 / 2.0).powi(2),
    );
}

#[test]
fn all_upstream_feature_names() {
    let cases = [
        ("External perimeter", ExtrusionType::ExternalPerimeter),
        ("Overhang perimeter", ExtrusionType::ExternalPerimeter),
        ("Perimeter", ExtrusionType::Perimeter),
        ("Infill", ExtrusionType::Infill),
        ("Solid infill", ExtrusionType::SolidInfill),
        ("Top solid infill", ExtrusionType::TopSolidInfill),
        ("Bridge infill", ExtrusionType::BridgeInfill),
        ("Support material", ExtrusionType::SupportMaterial),
        ("Support material interface", ExtrusionType::SupportMaterial),
        ("Skirt/Brim", ExtrusionType::SkirtBrim),
        ("Skirt", ExtrusionType::SkirtBrim),
        ("Custom", ExtrusionType::Custom),
        ("Gap fill", ExtrusionType::GapFill),
        ("Ironing", ExtrusionType::Ironing),
        ("Wipe tower", ExtrusionType::Wipe),
    ];
    for (text, expected) in cases {
        assert_eq!(
            parse_type_comment(&format!(" ; TYPE:{text} ")),
            Some(expected)
        );
    }
    assert_eq!(
        parse_type_comment("; TYPE:Unrecognized"),
        Some(ExtrusionType::Unknown)
    );
    assert_eq!(parse_type_comment("; not a type"), None);
    assert_eq!(
        parse_type_comment(";TYPE:WALL-OUTER"),
        Some(ExtrusionType::ExternalPerimeter)
    );
    assert_eq!(
        parse_type_comment("; FEATURE:Outer wall"),
        Some(ExtrusionType::ExternalPerimeter)
    );
}

#[test]
fn ccw_cw_full_circles_and_extrusion_are_continuous() {
    for command in ["G2", "G3"] {
        let data = parse_gcode(&format!("G1 X10 Y0\nM83\n{command} I-10 J0 E6.28 F600")).unwrap();
        let arcs: Vec<_> = data.segments().skip(1).collect();
        assert!(arcs.len() > 300);
        assert_eq!(arcs.last().unwrap().end, [10.0, 0.0, 0.0]);
        assert!(arcs.iter().all(|s| s.length() <= 0.2000001));
        assert!(
            arcs.iter()
                .all(|s| !s.is_travel && s.volumetric_flow.is_finite())
        );
        assert!(arcs.windows(2).all(|pair| pair[0].end == pair[1].start));
        assert_eq!(arcs[0].end[1].is_sign_negative(), command == "G2");
    }
}

#[test]
fn radius_sign_selects_major_arc() {
    let small = parse_gcode("G1 X10\nG3 X0 Y10 R10 E1").unwrap();
    let big = parse_gcode("G1 X10\nG3 X0 Y10 R-10 E1").unwrap();
    assert!(big.total_segments() > 2 * small.total_segments());
    assert_eq!(small.segments().last().unwrap().end, [0.0, 10.0, 0.0]);
    assert_eq!(big.segments().last().unwrap().end, [0.0, 10.0, 0.0]);
}

#[test]
fn absolute_centers_and_relative_endpoints() {
    let data = parse_gcode("G1 X10 Y0\nG90.1\nG91\nG3 X-10 Y10 I0 J0 E1").unwrap();
    assert_eq!(data.segments().last().unwrap().end, [0.0, 10.0, 0.0]);
    let length: f64 = data.segments().skip(1).map(|s| s.length()).sum();
    assert!((length - 5.0 * std::f64::consts::PI).abs() < 0.001);
}

#[test]
fn all_arc_planes_and_helices() {
    for (code, end) in [
        ("G17\nG1 X10\nG3 X0 Y10 Z2 I-10 J0 E1", [0.0, 10.0, 2.0]),
        ("G18\nG1 Z10\nG3 X10 Z0 Y2 K-10 I0 E1", [10.0, 2.0, 0.0]),
        ("G19\nG1 Y10\nG3 Y0 Z10 X2 J-10 K0 E1", [2.0, 0.0, 10.0]),
    ] {
        let data = parse_gcode(code).unwrap();
        assert_eq!(data.segments().last().unwrap().end, end);
        assert!(data.total_segments() > 70);
        assert!(data.total_layers() > 70);
    }
}

#[test]
fn arc_p_adds_complete_turns() {
    let a = parse_gcode("G1 X10\nG3 X0 Y10 I-10 J0 E1").unwrap();
    let b = parse_gcode("G1 X10\nG3 X0 Y10 I-10 J0 P2 E1").unwrap();
    assert!(b.total_segments() > 4 * a.total_segments());
    assert!(a.diagnostics.is_empty());
    assert!(
        b.diagnostics
            .iter()
            .any(|d| d.line == 2 && d.message.contains("total-turn count"))
    );
}

#[test]
fn malformed_values_and_impossible_arcs_fail_with_line_numbers() {
    for code in [
        "G1 XNaN",
        "G1 Xinf",
        "G1 X1e309",
        "G1 X1.2.3",
        "G1 X",
        "G1 F-1 X10",
        "G2 X10 R1",
        "G3 X10",
        "G3 X0 R10",
        "G2 X10 I0 J0",
        "G2 X10 I1 J1",
        "G2 X10 I5 P0",
    ] {
        let result = parse_gcode(&format!(";header\n{code}"));
        assert!(result.is_err(), "accepted {code}");
        assert_eq!(result.unwrap_err().line, 2);
    }
    assert!(parse_gcode("G1 X10\nX").is_err());
    assert!(parse_gcode("\0").is_err());
}

#[test]
fn metadata_bed_presets_overrides_and_filament() {
    let data=parse_gcode("M862.3 P \"XL\"\n; bed_shape = 0x0,300x0,300x200,0x200\n; max_print_height = 320\n; filament_diameter = 2.85\nM83\nG1 X10 E1 F600").unwrap();
    assert_eq!((data.bed_x, data.bed_y, data.max_z), (300.0, 200.0, 320.0));
    near(
        data.segments().next().unwrap().volumetric_flow,
        std::f64::consts::PI * (2.85_f64 / 2.0).powi(2),
    );
    let prusa = parse_gcode("M862.3 P\"COREONE\"\nG1 X10").unwrap();
    assert_eq!(
        (prusa.bed_x, prusa.bed_y, prusa.max_z),
        (250.0, 220.0, 250.0)
    );
    let invalid=parse_gcode("; filament_diameter = NaN\n; max_print_height = inf\n; bed_shape = NaNx0,10x0,10x20\nG1 X1").unwrap();
    assert_eq!(
        (invalid.bed_x, invalid.bed_y, invalid.max_z),
        (250.0, 210.0, 220.0)
    );
    assert!(
        !data
            .diagnostics
            .iter()
            .any(|d| d.message.contains("bed_shape"))
    );
    let centered = parse_gcode("; bed_shape = -100x-100,100x-100,100x100,-100x100\nG1 X1").unwrap();
    assert_eq!((centered.bed_x, centered.bed_y), (200.0, 200.0));
    assert!(
        centered
            .diagnostics
            .iter()
            .any(|d| d.message.contains("bed_shape"))
    );
}

#[test]
fn m200_volume_mode() {
    let data = parse_gcode("M83\nM200 D1.75\nG1 X10 E1 F600\nM200 D0\nG1 X20 E1").unwrap();
    let s: Vec<_> = data.segments().collect();
    near(s[0].volumetric_flow, 1.0);
    near(
        s[1].volumetric_flow,
        std::f64::consts::PI * (1.75_f64 / 2.0).powi(2),
    );
    let inches = parse_gcode("M83\nM200 D1.75\nG20\nG1 X1 E0.001 F60").unwrap();
    near(
        inches.segments().next().unwrap().volumetric_flow,
        0.001 * 25.4_f64.powi(3),
    );
}

#[test]
fn huge_arc_is_rejected_before_tessellation() {
    let error = parse_gcode("G1 X1000000000\nG3 I-1000000000 E1").unwrap_err();
    assert_eq!(error.line, 2);
    assert!(error.message.contains("segment budget"));
}

#[test]
fn unsupported_geometry_is_visible_and_warnings_are_deduplicated() {
    let data =
        parse_gcode("G54\nG54\nG10 L2 X10\nG1 X1\nG28 X\nG1 X2\nG92 X0\nT1\nSET_GCODE_OFFSET X=10")
            .unwrap();
    assert!(data.diagnostics.len() >= 5);
    assert_eq!(
        data.diagnostics
            .iter()
            .filter(|d| d.message.contains("Unsupported G54"))
            .count(),
        1
    );
    assert!(data.diagnostics.iter().all(|d| d.line > 0));
}

#[test]
fn display_and_version_strings_are_not_motion_words() {
    let data =
        parse_gcode("M117 Printing X0.2.3 NaN\nM115 U6.4.0+11974\nM862.3 P\"MK4S\"\nG1 X10 E1")
            .unwrap();
    assert_eq!(data.total_segments(), 1);
    assert_eq!(data.segments().next().unwrap().end, [10.0, 0.0, 0.0]);
}

#[test]
fn multiple_modal_commands_in_one_block() {
    let data = parse_gcode("G91 G1 X10 E1\nG1 X10 E2").unwrap();
    assert_eq!(data.segments().last().unwrap().end, [20.0, 0.0, 0.0]);
    assert!(parse_gcode("G1 X10 G2 I5").is_err());
}

#[test]
fn color_modes_round_trip() {
    for mode in ColorMode::ALL {
        assert_eq!(mode.name().parse::<ColorMode>().unwrap(), mode);
        assert!(!mode.label().is_empty());
    }
    assert!("unknown".parse::<ColorMode>().is_err());
}
#[test]
fn plain_motion_and_numbered_blocks_agree() {
    let motion = [
        "G1 X-.25 Y+3 F600",
        "G1 X.123456789012345 Y1. E.5",
        "G1 X2X3 E.2",
        "G1 X1e-3 E.31",
        "G1 Z.2",
        "G2 X1.001 Y1 I.5 J0 E.05",
    ];
    // Line numbers force the general lexer. Both forms have identical modal state.
    let input = |numbered: bool| {
        let mut text = String::from("M83\n");
        for (i, line) in motion.iter().enumerate() {
            if numbered {
                text.push_str(&format!("N{i} "));
            }
            text.push_str(line);
            text.push('\n');
        }
        text
    };
    assert_eq!(
        parse_gcode(&input(false)).unwrap(),
        parse_gcode(&input(true)).unwrap()
    );
}
