//! Unified color definitions for CLI and TUI rendering.

/// RGB color that can be converted to both colored crate and ratatui formats.
#[derive(Clone, Copy, Debug)]
pub struct ThemeColor(pub u8, pub u8, pub u8);

impl ThemeColor {
    /// Apply a dimming factor to the color.
    pub fn apply_dim(&self, factor: f32) -> Self {
        ThemeColor(
            (self.0 as f32 * factor) as u8,
            (self.1 as f32 * factor) as u8,
            (self.2 as f32 * factor) as u8,
        )
    }

    /// Get RGB tuple for use with colored crate's truecolor method.
    pub fn rgb(&self) -> (u8, u8, u8) {
        (self.0, self.1, self.2)
    }
}

// Color constants for consistent theming
pub mod theme {
    use super::ThemeColor;

    pub const GREEN: ThemeColor = ThemeColor(142, 192, 124);
    pub const RED: ThemeColor = ThemeColor(204, 36, 29);
    pub const GRAY: ThemeColor = ThemeColor(128, 128, 128);
    pub const GOLD: ThemeColor = ThemeColor(215, 153, 33);
    pub const TREE: ThemeColor = ThemeColor(55, 55, 50);
    pub const YELLOW: ThemeColor = ThemeColor(250, 189, 47);
    pub const PURPLE: ThemeColor = ThemeColor(180, 142, 173);
    pub const MUTED: ThemeColor = ThemeColor(90, 90, 90);
    pub const PR_NUMBER: ThemeColor = ThemeColor(90, 78, 98);
    pub const PR_ARROW: ThemeColor = ThemeColor(100, 105, 105);
    pub const UPSTREAM: ThemeColor = ThemeColor(88, 88, 88);
    pub const STACKED_ON: ThemeColor = ThemeColor(90, 120, 87);
    pub const BLUE: ThemeColor = ThemeColor(131, 165, 152);
}

/// Compute a deterministic RGB color from a string using its hash.
/// Uses MD5 to hash the string, derives a hue from the first two bytes,
/// and converts HSV to RGB with fixed saturation and value for readability.
pub fn string_to_color(s: &str) -> ThemeColor {
    let hash = md5::compute(s);
    // Use first two bytes to get a hue value (0-360)
    let hue = (u16::from(hash[0]) | (u16::from(hash[1]) << 8)) % 360;
    // Fixed saturation and value for good terminal readability
    let saturation = 0.35;
    let value = 0.75;
    let (r, g, b) = hsv_to_rgb(hue as f32, saturation, value);
    ThemeColor(r, g, b)
}

/// Convert HSV color to RGB.
/// h: hue (0-360), s: saturation (0-1), v: value (0-1)
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r, g, b) = match h as u32 {
        0..60 => (c, x, 0.0),
        60..120 => (x, c, 0.0),
        120..180 => (0.0, c, x),
        180..240 => (0.0, x, c),
        240..300 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}
