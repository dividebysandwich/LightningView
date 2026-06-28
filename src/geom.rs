// --- Minimal 2D geometry ---
//
// Small `Vec2` / `Rect` replacements for the egui types the canvas used to lean
// on (`egui::Vec2`, `egui::Pos2`, `egui::Rect`). Points and vectors are both
// represented as `Vec2`; rectangles are stored as a min corner + size, mirroring
// egui's `Rect::from_min_size` so the zoom/pan/tiling math ports over verbatim.

use std::ops::{Add, AddAssign, Div, Mul, Sub, SubAssign};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const ZERO: Vec2 = Vec2 { x: 0.0, y: 0.0 };

    #[inline]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    #[inline]
    pub fn length_sq(self) -> f32 {
        self.x * self.x + self.y * self.y
    }
}

impl Add for Vec2 {
    type Output = Vec2;
    #[inline]
    fn add(self, rhs: Vec2) -> Vec2 {
        Vec2::new(self.x + rhs.x, self.y + rhs.y)
    }
}
impl Sub for Vec2 {
    type Output = Vec2;
    #[inline]
    fn sub(self, rhs: Vec2) -> Vec2 {
        Vec2::new(self.x - rhs.x, self.y - rhs.y)
    }
}
impl Mul<f32> for Vec2 {
    type Output = Vec2;
    #[inline]
    fn mul(self, rhs: f32) -> Vec2 {
        Vec2::new(self.x * rhs, self.y * rhs)
    }
}
impl Div<f32> for Vec2 {
    type Output = Vec2;
    #[inline]
    fn div(self, rhs: f32) -> Vec2 {
        Vec2::new(self.x / rhs, self.y / rhs)
    }
}
impl AddAssign for Vec2 {
    #[inline]
    fn add_assign(&mut self, rhs: Vec2) {
        self.x += rhs.x;
        self.y += rhs.y;
    }
}
impl SubAssign for Vec2 {
    #[inline]
    fn sub_assign(&mut self, rhs: Vec2) {
        self.x -= rhs.x;
        self.y -= rhs.y;
    }
}

/// An axis-aligned rectangle stored as a minimum corner plus size, in pixel
/// space with a top-left origin (y grows downward), matching the old egui usage.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub min: Vec2,
    pub size: Vec2,
}

impl Rect {
    #[inline]
    pub fn from_min_size(min: Vec2, size: Vec2) -> Self {
        Self { min, size }
    }

    /// Build from a min and max corner. `max` must be >= `min`.
    #[inline]
    pub fn from_min_max(min: Vec2, max: Vec2) -> Self {
        Self { min, size: max - min }
    }

    /// Rect from explicit pixel coordinates.
    #[inline]
    pub fn xywh(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self::from_min_size(Vec2::new(x, y), Vec2::new(w, h))
    }

    #[inline]
    pub fn size(&self) -> Vec2 {
        self.size
    }
    #[inline]
    pub fn width(&self) -> f32 {
        self.size.x
    }
    #[inline]
    pub fn height(&self) -> f32 {
        self.size.y
    }
    #[inline]
    pub fn max(&self) -> Vec2 {
        self.min + self.size
    }
    #[inline]
    pub fn center(&self) -> Vec2 {
        self.min + self.size * 0.5
    }

    /// True if this rectangle overlaps `other` (touching edges count as overlap,
    /// matching egui's inclusive `intersects`).
    pub fn intersects(&self, other: Rect) -> bool {
        let a_max = self.max();
        let b_max = other.max();
        self.min.x <= b_max.x
            && other.min.x <= a_max.x
            && self.min.y <= b_max.y
            && other.min.y <= a_max.y
    }

    /// True if `p` lies within the rectangle (inclusive of the min/max edges).
    /// Used for immediate-mode widget hit-testing (dialogs, context menu).
    #[allow(dead_code)]
    pub fn contains(&self, p: Vec2) -> bool {
        let max = self.max();
        p.x >= self.min.x && p.x <= max.x && p.y >= self.min.y && p.y <= max.y
    }
}
