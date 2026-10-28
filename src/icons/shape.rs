// SPDX-License-Identifier: GPL-3.0-only
use cosmic::cosmic_theme::Roundness;
use image::{RgbaImage, imageops};

pub(super) const CANVAS: u32 = 256;
pub(super) const CONTENT_SCALE: f32 = 0.90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconShape {
    Circular,
    Rectangular,
    Freeform,
}

impl IconShape {
    pub fn needs_frame(self, theme: Roundness) -> bool {
        match self {
            Self::Circular => theme != Roundness::Round,
            Self::Rectangular => false,
            Self::Freeform => true,
        }
    }

    pub fn can_crop(self) -> bool {
        self == Self::Rectangular
    }
}

pub(super) fn corner_radius(roundness: Roundness) -> f32 {
    match roundness {
        Roundness::Round => 0.5,
        Roundness::SlightlyRound => 0.125,
        Roundness::Square => 0.0,
    }
}

pub(super) fn crop_corners(image: &RgbaImage, roundness: Roundness) -> RgbaImage {
    let center = CANVAS as f32 / 2.0;
    let edge = CANVAS as f32 * CONTENT_SCALE;
    let radius = edge * corner_radius(roundness);
    RgbaImage::from_fn(CANVAS, CANVAS, |x, y| {
        let dx = (x as f32 + 0.5 - center).abs() - (edge / 2.0 - radius);
        let dy = (y as f32 + 0.5 - center).abs() - (edge / 2.0 - radius);
        let distance = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) - radius;
        let coverage = (0.5 - distance).clamp(0.0, 1.0);
        let mut pixel = *image.get_pixel(x, y);
        pixel[3] = (f32::from(pixel[3]) * coverage).round() as u8;
        pixel
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub bounds: (u32, u32, u32, u32),
    pub kind: IconShape,
}

fn solid_rounded_rectangle(
    image: &RgbaImage,
    (left, top, width, height): (u32, u32, u32, u32),
    threshold: u8,
) -> bool {
    if width.min(height) * 100 < width.max(height) * 80 {
        return false;
    }
    let area = (top..top + height)
        .flat_map(|y| (left..left + width).map(move |x| (x, y)))
        .filter(|&(x, y)| image.get_pixel(x, y)[3] >= threshold)
        .count();
    // Rounded corners remove (4 - pi) * radius² of the normalized rectangle.
    let radius =
        ((1.0 - area as f32 / (width * height) as f32) / (4.0 - std::f32::consts::PI)).sqrt();
    if radius > 0.35 {
        return false;
    }
    let mut errors = [0_u32; 4];
    let mut areas = [0_u32; 4];
    for y in 0..height {
        for x in 0..width {
            let dx = ((x as f32 + 0.5) / width as f32 - 0.5).abs() - (0.5 - radius);
            let dy = ((y as f32 + 0.5) / height as f32 - 0.5).abs() - (0.5 - radius);
            let expected = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) <= radius;
            let actual = image.get_pixel(left + x, top + y)[3] >= threshold;
            let quadrant = usize::from(y >= height / 2) * 2 + usize::from(x >= width / 2);
            errors[quadrant] += u32::from(actual != expected);
            areas[quadrant] += 1;
        }
    }
    // Check each corner separately so tabs, cutouts and stacked folders cannot hide in the average.
    errors
        .into_iter()
        .zip(areas)
        .all(|(error, area)| error * 100 <= area)
}

/// Ignore soft shadows and compare the silhouette against circles and rounded squares.
/// Only regular, filled rectangles can be cropped; other artwork keeps its original outline.
pub fn analyze(image: &RgbaImage) -> Option<Shape> {
    let peak = image.pixels().map(|p| p[3]).max()?;
    if peak < 16 {
        return None;
    }
    let threshold = (peak / 2).max(16);
    let (mut left, mut top, mut right, mut bottom) = (image.width(), image.height(), 0, 0);
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel[3] >= threshold {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x + 1);
            bottom = bottom.max(y + 1);
        }
    }
    let width = right.checked_sub(left)?;
    let height = bottom.checked_sub(top)?;
    if width < 4 || height < 4 {
        return None;
    }
    let bounds = (left, top, width, height);
    let mut spans = Vec::with_capacity(height as usize);
    for y in top..bottom {
        let row_left = (left..right).find(|x| image.get_pixel(*x, y)[3] >= threshold);
        let row_right = (left..right)
            .rev()
            .find(|x| image.get_pixel(*x, y)[3] >= threshold);
        spans.push(row_left.zip(row_right));
    }
    let mut best = (0.0_f32, Roundness::Round);
    for (radius, roundness) in [
        (0.0_f32, Roundness::Square),
        (0.125, Roundness::SlightlyRound),
        (0.5, Roundness::Round),
    ] {
        let (mut intersection, mut union) = (0, 0);
        for y in 0..height {
            for x in 0..width {
                let px = (x as f32 + 0.5) / width as f32;
                let py = (y as f32 + 0.5) / height as f32;
                let dx = (px - 0.5).abs() - (0.5 - radius);
                let dy = (py - 0.5).abs() - (0.5 - radius);
                let expected = dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) <= radius;
                let actual = spans[y as usize].is_some_and(|(a, b)| (a..=b).contains(&(left + x)));
                intersection += u32::from(actual && expected);
                union += u32::from(actual || expected);
            }
        }
        let similarity = intersection as f32 / union.max(1) as f32;
        if similarity > best.0 {
            best = (similarity, roundness);
        }
    }
    let circular = best.1 == Roundness::Round && width.min(height) * 10 >= width.max(height) * 9;
    let kind = if circular && best.0 >= 0.97 {
        IconShape::Circular
    } else if solid_rounded_rectangle(image, bounds, threshold) {
        IconShape::Rectangular
    } else if circular && best.0 >= 0.90 {
        IconShape::Circular
    } else {
        IconShape::Freeform
    };
    Some(Shape { bounds, kind })
}

pub fn normalize(image: &RgbaImage, shape: Shape) -> RgbaImage {
    let (x, y, width, height) = shape.bounds;
    let scale = CANVAS as f32 * CONTENT_SCALE / width.max(height) as f32;
    // Keep a little of the soft edge around the measured silhouette.
    let margin = (width.max(height) / 24).max(1);
    let left = x.saturating_sub(margin);
    let top = y.saturating_sub(margin);
    let right = (x + width + margin).min(image.width());
    let bottom = (y + height + margin).min(image.height());
    let crop = imageops::crop_imm(image, left, top, right - left, bottom - top);
    let target = imageops::resize(
        &crop.to_image(),
        (((right - left) as f32 * scale).round() as u32).max(1),
        (((bottom - top) as f32 * scale).round() as u32).max(1),
        imageops::FilterType::Lanczos3,
    );
    let mut canvas = RgbaImage::new(CANVAS, CANVAS);
    let center_x = (x as f32 + width as f32 / 2.0 - left as f32) * scale;
    let center_y = (y as f32 + height as f32 / 2.0 - top as f32) * scale;
    imageops::overlay(
        &mut canvas,
        &target,
        (CANVAS as f32 / 2.0 - center_x).round() as i64,
        (CANVAS as f32 / 2.0 - center_y).round() as i64,
    );
    canvas
}

pub(super) fn crop_to_theme(image: &RgbaImage, shape: Shape, roundness: Roundness) -> RgbaImage {
    let (x, y, width, height) = shape.bounds;
    let mut side = width.min(height);
    if roundness != Roundness::Round {
        let threshold = (image.pixels().map(|p| p[3]).max().unwrap_or(0) / 2).max(16);
        let center_x = x as f32 + (width - 1) as f32 / 2.0;
        let center_y = y as f32 + (height - 1) as f32 / 2.0;
        let radius = corner_radius(roundness);
        let fits = |side: u32| {
            (0..=8).all(|step| {
                let angle = step as f32 * std::f32::consts::FRAC_PI_2 / 8.0;
                let dx = (0.5 - radius + radius * angle.cos()) * (side - 1) as f32;
                let dy = (0.5 - radius + radius * angle.sin()) * (side - 1) as f32;
                [-1.0, 1.0].into_iter().all(|sx| {
                    [-1.0, 1.0].into_iter().all(|sy| {
                        let px = (center_x + sx * dx).round() as u32;
                        let py = (center_y + sy * dy).round() as u32;
                        image.get_pixel(px, py)[3] >= threshold
                    })
                })
            })
        };
        // Keep the target's corners inside the source's rounded body.
        while side > 2 && !fits(side) {
            side -= 1;
        }
        if side < width.min(height) {
            // Leave a source pixel around curved edges for image resampling.
            side = side.saturating_sub(2).max(2);
        }
    }
    let centered = Shape {
        bounds: (x + (width - side) / 2, y + (height - side) / 2, side, side),
        ..shape
    };
    crop_corners(&normalize(image, centered), roundness)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    const THEMES: [Roundness; 3] = [
        Roundness::Round,
        Roundness::SlightlyRound,
        Roundness::Square,
    ];

    fn silhouette(test: impl Fn(f32, f32) -> bool) -> RgbaImage {
        RgbaImage::from_fn(128, 128, |x, y| {
            let visible = test((x as f32 - 64.0) / 48.0, (y as f32 - 64.0) / 48.0);
            Rgba([40, 120, 220, if visible { 255 } else { 0 }])
        })
    }

    #[test]
    fn circular_logos_keep_their_pixels_and_follow_the_theme() {
        let circle = silhouette(|x, y| x.hypot(y) <= 1.0);
        let ring = silhouette(|x, y| (0.75..=1.0).contains(&x.hypot(y)));
        let irregular = silhouette(|x, y| x.hypot(y) <= 1.0 && !(x < -0.15 && y < -0.65));
        let mut translucent = circle.clone();
        for pixel in translucent.pixels_mut() {
            pixel[3] = if pixel[3] == 0 { 12 } else { 100 };
        }
        for image in [circle, ring, irregular, translucent] {
            let shape = analyze(&image).unwrap().kind;
            assert_eq!(shape, IconShape::Circular);
            assert!(!shape.can_crop());
            for theme in THEMES {
                assert_eq!(shape.needs_frame(theme), theme != Roundness::Round);
            }
        }
    }

    #[test]
    fn freeform_and_hollow_artwork_never_qualifies_for_cropping() {
        for image in [
            silhouette(|x, y| (-1.0..=1.0).contains(&y) && x.abs() <= (y + 1.0) / 2.0),
            silhouette(|x, y| x.abs() < 0.25 || y.abs() < 0.25),
            silhouette(|x, y| x.abs() <= 1.0 && y.abs() <= 0.6),
            silhouette(|x, y| x.abs() <= 1.0 && y.abs() <= 1.0 && !(x > 0.6 && y < -0.6)),
            silhouette(|x, y| x.abs() <= 1.0 && y.abs() <= 1.0 && x.hypot(y) > 0.25),
        ] {
            let shape = analyze(&image).unwrap().kind;
            assert_eq!(shape, IconShape::Freeform);
            assert!(!shape.can_crop());
            for theme in THEMES {
                assert!(shape.needs_frame(theme));
            }
        }
    }

    #[test]
    fn square_and_rounded_artwork_fills_each_target_outline() {
        for radius in [0.0_f32, 0.1, 0.25, 0.5, 0.6] {
            let image = silhouette(|x, y| {
                let dx = x.abs() - (1.0 - radius);
                let dy = y.abs() - (1.0 - radius);
                dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0) <= radius
            });
            let shape = analyze(&image).unwrap();
            assert_eq!(shape.kind, IconShape::Rectangular);
            for theme in THEMES {
                assert!(!shape.kind.needs_frame(theme));
                let cropped = crop_to_theme(&image, shape, theme);
                assert_eq!(*cropped.get_pixel(128, 128), Rgba([40, 120, 220, 255]));
                assert!(cropped.get_pixel(128, 20)[3] >= 250);
                assert_eq!(
                    cropped.get_pixel(15, 15)[3] >= 250,
                    theme == Roundness::Square
                );
                assert_eq!(
                    cropped.get_pixel(30, 30)[3] >= 250,
                    theme != Roundness::Round
                );
                assert!(cropped.pixels().any(|p| (1..255).contains(&p[3])));
            }
        }
    }

    #[test]
    fn transparent_or_tiny_shapes_are_left_alone() {
        assert!(analyze(&RgbaImage::new(128, 128)).is_none());
        assert!(analyze(&RgbaImage::from_pixel(2, 2, Rgba([255; 4]))).is_none());
    }

    #[test]
    fn normalization_centers_padded_icons_and_preserves_extent() {
        let image = RgbaImage::from_fn(160, 100, |x, y| {
            Rgba([
                255,
                100,
                50,
                if (60..=119).contains(&x) && (10..=69).contains(&y) {
                    255
                } else {
                    0
                },
            ])
        });
        let normalized = normalize(&image, analyze(&image).unwrap());
        let (x, y, width, height) = analyze(&normalized).unwrap().bounds;
        assert_eq!(normalized.dimensions(), (256, 256));
        assert_eq!(width, height);
        assert!((x as i32 * 2 + width as i32 - 256).abs() <= 2);
        assert!((y as i32 * 2 + height as i32 - 256).abs() <= 2);
        assert!((228..=232).contains(&width));
    }

    #[test]
    fn rectangular_bodies_fill_each_theme_without_distorting_content() {
        for (width, height) in [(112, 96), (112, 100), (96, 112), (100, 112)] {
            let image = RgbaImage::from_fn(128, 128, |x, y| {
                if !((128 - width) / 2..(128 + width) / 2).contains(&x)
                    || !((128 - height) / 2..(128 + height) / 2).contains(&y)
                {
                    Rgba([0; 4])
                } else if (x as f32 + 0.5 - 64.0).hypot(y as f32 + 0.5 - 64.0) < 12.0 {
                    Rgba([220, 40, 40, 255])
                } else {
                    Rgba([40, 120, 220, 255])
                }
            });
            let shape = analyze(&image).unwrap();
            for theme in THEMES {
                let cropped = crop_to_theme(&image, shape, theme);
                assert_eq!(cropped.get_pixel(128, 128), image.get_pixel(64, 64));
                for (x, y) in [(128, 20), (20, 128)] {
                    assert_eq!(*cropped.get_pixel(x, y), Rgba([40, 120, 220, 255]));
                }
                let marker = RgbaImage::from_fn(CANVAS, CANVAS, |x, y| {
                    let p = cropped.get_pixel(x, y);
                    Rgba([
                        255,
                        255,
                        255,
                        if p[0] > 180 && p[1] < 70 && p[3] > 128 {
                            255
                        } else {
                            0
                        },
                    ])
                });
                let (_, _, marker_width, marker_height) = analyze(&marker).unwrap().bounds;
                assert!(marker_width.abs_diff(marker_height) <= 1);
            }
        }
    }
}
