//! Orbit-camera parameters independent of any windowing or graphics API.
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum CameraPreset {
    #[default]
    Isometric,
    Top,
    Front,
    Side,
}

impl CameraPreset {
    pub const ALL: [Self; 4] = [Self::Isometric, Self::Top, Self::Front, Self::Side];

    pub fn name(self) -> &'static str {
        match self {
            Self::Isometric => "isometric",
            Self::Top => "top",
            Self::Front => "front",
            Self::Side => "side",
        }
    }
}

impl fmt::Display for CameraPreset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for CameraPreset {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|preset| preset.name().eq_ignore_ascii_case(value))
            .ok_or_else(|| {
                format!("unknown camera {value:?}; expected isometric, top, front or side")
            })
    }
}

/// Azimuth zero looks from -Y; azimuth 90 looks from +X. Elevation 90 is top.
/// Pan is measured in fractions of the viewport, with positive Y downwards.
/// A zoom greater than one magnifies the image without moving the near plane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Camera {
    pub azimuth_deg: f64,
    pub elevation_deg: f64,
    pub zoom: f64,
    pub pan: [f64; 2],
    pub perspective: bool,
}

impl Default for Camera {
    fn default() -> Self {
        Self::from_preset(CameraPreset::Isometric)
    }
}

impl Camera {
    pub fn from_preset(preset: CameraPreset) -> Self {
        let (azimuth_deg, elevation_deg) = match preset {
            CameraPreset::Isometric => (45.0, 35.264389682754654),
            CameraPreset::Top => (0.0, 90.0),
            CameraPreset::Front => (0.0, 0.0),
            CameraPreset::Side => (90.0, 0.0),
        };
        Self {
            azimuth_deg,
            elevation_deg,
            zoom: 1.0,
            pan: [0.0, 0.0],
            perspective: true,
        }
    }

    pub fn reset(&mut self, preset: CameraPreset) {
        *self = Self::from_preset(preset);
    }

    pub fn orbit(&mut self, dx_degrees: f64, dy_degrees: f64) {
        if dx_degrees.is_finite() && dy_degrees.is_finite() {
            self.azimuth_deg = (self.azimuth_deg + dx_degrees).rem_euclid(360.0);
            self.elevation_deg = (self.elevation_deg + dy_degrees).clamp(-90.0, 90.0);
        }
    }

    pub fn pan_by(&mut self, dx_pixels: f64, dy_pixels: f64, width: f64, height: f64) {
        if [dx_pixels, dy_pixels, width, height]
            .iter()
            .all(|v| v.is_finite())
            && width > 0.0
            && height > 0.0
        {
            self.pan[0] = (self.pan[0] + dx_pixels / width).clamp(-100.0, 100.0);
            self.pan[1] = (self.pan[1] + dy_pixels / height).clamp(-100.0, 100.0);
        }
    }

    pub fn zoom_by(&mut self, factor: f64) {
        if factor.is_finite() && factor > 0.0 {
            self.zoom = (self.zoom * factor).clamp(0.01, 1000.0);
        }
    }

    pub(crate) fn valid(&self) -> bool {
        [
            self.azimuth_deg,
            self.elevation_deg,
            self.zoom,
            self.pan[0],
            self.pan[1],
        ]
        .iter()
        .all(|v| v.is_finite())
            && self.zoom > 0.0
            && self.zoom <= 1000.0
            && self.elevation_deg.abs() <= 90.0
            && self.pan.iter().all(|value| value.abs() <= 100.0)
    }

    /// Right, up and backwards unit vectors. This formulation remains valid at
    /// the poles, where a cross product with the world-up vector would collapse.
    pub fn basis(&self) -> [[f64; 3]; 3] {
        let a = self.azimuth_deg.rem_euclid(360.0).to_radians();
        let e = self.elevation_deg.to_radians();
        [
            [a.cos(), a.sin(), 0.0],
            [-e.sin() * a.sin(), e.sin() * a.cos(), e.cos()],
            [e.cos() * a.sin(), -e.cos() * a.cos(), e.sin()],
        ]
    }
}
