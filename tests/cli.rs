//! End-to-end tests launch the real executable and validate PNG contents.
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};
static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gcv-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bgcode/reference.gcode")
}
fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_gcv"))
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap()
}

#[test]
fn export_real_png_without_display_and_scaled_dimensions() {
    let temp = TempDir::new();
    let output = temp.join("scaled.png");
    let result = run(&[
        fixture().to_str().unwrap(),
        "-e",
        output.to_str().unwrap(),
        "-r",
        "240x160",
        "-s",
        "1.5",
        "--no-bed",
        "-c",
        "top",
    ]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let png = image::open(output).unwrap().to_rgba8();
    assert_eq!(png.dimensions(), (360, 240));
    let distinct: std::collections::HashSet<_> = png.pixels().map(|p| p.0).collect();
    assert!(
        distinct.len() > 10,
        "image should contain antialiased geometry and text"
    );
}

#[test]
fn all_presets_and_color_modes_export_valid_images() {
    let temp = TempDir::new();
    for camera in ["isometric", "top", "front", "side"] {
        for mode in ["feature", "height", "speed", "fan", "temperature", "flow"] {
            let output = temp.join(&format!("{camera}-{mode}.png"));
            let result = run(&[
                fixture().to_str().unwrap(),
                "-e",
                output.to_str().unwrap(),
                "-r",
                "160x100",
                "-c",
                camera,
                "--color-mode",
                mode,
                "--show-travel",
            ]);
            assert!(
                result.status.success(),
                "{camera}/{mode}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            assert_eq!(image::open(output).unwrap().width(), 160);
        }
    }
}

#[test]
fn stats_are_json_and_binary_magic_beats_extension() {
    let temp = TempDir::new();
    let disguised = temp.join("binary.txt");
    std::fs::write(
        &disguised,
        include_bytes!("fixtures/bgcode/heatshrink12-meatpack-v1.bgcode"),
    )
    .unwrap();
    let result = run(&[disguised.to_str().unwrap(), "--stats"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stats: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(stats["schema_version"], 1);
    assert!(stats["total_segments"].as_u64().unwrap() > 0);
    assert!(stats["diagnostics"].is_array());
}

#[test]
fn invalid_options_files_and_layer_ranges_fail_cleanly() {
    let temp = TempDir::new();
    let output = temp.join("must-not-exist.png");
    let input = fixture();
    let input = input.to_str().unwrap();
    let dest = output.to_str().unwrap();
    for args in [
        vec![input, "-e", dest, "--layer", "999"],
        vec![input, "-e", dest, "--min-layer", "2", "--layer", "1"],
        vec![input, "-e", dest, "-r", "0x10"],
        vec![input, "-e", dest, "--scale", "NaN"],
        vec!["/file/does/not/exist.gcode", "-e", dest],
    ] {
        let result = run(&args);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("error"));
        assert!(!output.exists());
    }
}

#[test]
fn corrupt_binary_and_strict_diagnostics_are_errors() {
    let temp = TempDir::new();
    let corrupt = temp.join("corrupt.bgcode");
    std::fs::write(&corrupt, b"GCDE\x01\0").unwrap();
    assert!(
        !run(&[corrupt.to_str().unwrap(), "--stats"])
            .status
            .success()
    );
    let warned = temp.join("warned.gcode");
    std::fs::write(&warned, "G28\nG1 X10 Y10 E1\n").unwrap();
    let result = run(&[warned.to_str().unwrap(), "--strict", "--stats"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("strict parsing"));
}

#[test]
fn utf8_bom_and_empty_files_have_defined_behavior() {
    let with_bom = gcv::parse_bytes(b"\xef\xbb\xbfG1 X10 E1\n").unwrap();
    assert_eq!(with_bom.total_segments(), 1);
    assert!(gcv::parse_bytes(&[0xff, 0xfe]).is_err());
    let temp = TempDir::new();
    let empty = temp.join("empty.gcode");
    let output = temp.join("empty.png");
    std::fs::write(&empty, []).unwrap();
    assert!(
        run(&[
            empty.to_str().unwrap(),
            "-e",
            output.to_str().unwrap(),
            "-r",
            "80x80"
        ])
        .status
        .success()
    );
}

#[cfg(not(feature = "gui"))]
#[test]
fn headless_build_explains_how_to_get_gui() {
    let result = run(&[fixture().to_str().unwrap()]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("no GUI"));
}
