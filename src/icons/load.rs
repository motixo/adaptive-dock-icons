// SPDX-License-Identifier: GPL-3.0-only
use super::{
    PreparedIcon,
    shape::{CANVAS, analyze, crop_to_theme, normalize},
};
use cosmic::{desktop::IconSourceExt, widget::icon};
use image::RgbaImage;
use std::{
    io::{Cursor, Read},
    path::Path,
};

const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

fn file_bytes(path: &Path) -> Option<Vec<u8>> {
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= MAX_FILE_BYTES).then_some(bytes)
}

pub fn rasterize(bytes: &[u8]) -> Option<RgbaImage> {
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return None;
    }
    if let Ok(format) = image::guess_format(bytes) {
        let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(2048);
        limits.max_image_height = Some(2048);
        limits.max_alloc = Some(32 * 1024 * 1024);
        reader.limits(limits);
        return reader.decode().ok().map(|image| image.to_rgba8());
    }
    let mut options = resvg::usvg::Options::default();
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    let tree = resvg::usvg::Tree::from_data(bytes, &options).ok()?;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(CANVAS, CANVAS)?;
    let scale = CANVAS as f32 / tree.size().width().max(tree.size().height());
    let transform = resvg::tiny_skia::Transform::from_scale(scale, scale).post_translate(
        (CANVAS as f32 - tree.size().width() * scale) / 2.0,
        (CANVAS as f32 - tree.size().height() * scale) / 2.0,
    );
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    let mut rgba = pixmap.take();
    for pixel in rgba.as_chunks_mut::<4>().0 {
        if pixel[3] > 0 {
            for channel in 0..3 {
                pixel[channel] = ((u32::from(pixel[channel]) * 255 + u32::from(pixel[3]) / 2)
                    / u32::from(pixel[3]))
                .min(255) as u8;
            }
        }
    }
    RgbaImage::from_raw(CANVAS, CANVAS, rgba)
}

pub fn prepare(name: &str) -> Option<PreparedIcon> {
    let handle = cosmic::desktop::fde::IconSource::from_unknown(name).as_cosmic_icon();
    if handle.symbolic {
        return None;
    }
    let bytes = match &handle.data {
        icon::Data::Svg(handle) => match handle.data() {
            cosmic::iced::core::svg::Data::Path(path) => file_bytes(path)?,
            cosmic::iced::core::svg::Data::Bytes(bytes) => bytes.to_vec(),
        },
        icon::Data::Image(cosmic::iced::core::image::Handle::Path(_, path)) => file_bytes(path)?,
        icon::Data::Image(cosmic::iced::core::image::Handle::Bytes(_, bytes)) => bytes.to_vec(),
        icon::Data::Image(_) => return None,
    };
    let source = rasterize(&bytes)?;
    let shape = analyze(&source)?;
    let image = normalize(&source, shape);
    let cropped = shape.kind.can_crop().then(|| {
        [
            cosmic::cosmic_theme::Roundness::Round,
            cosmic::cosmic_theme::Roundness::SlightlyRound,
            cosmic::cosmic_theme::Roundness::Square,
        ]
        .map(|roundness| {
            icon::from_raster_pixels(
                CANVAS,
                CANVAS,
                crop_to_theme(&source, shape, roundness).into_raw(),
            )
        })
    });
    Some(PreparedIcon {
        handle: icon::from_raster_pixels(CANVAS, CANVAS, image.into_raw()),
        shape: shape.kind,
        cropped,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn svg_alpha_is_unpremultiplied_before_display() {
        let bytes = br##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect width="64" height="64" fill="#ff8040" opacity="0.5"/></svg>"##;
        let image = rasterize(bytes).unwrap();
        let pixel = image.get_pixel(128, 128);
        assert_eq!(pixel[0], 255);
        assert!((126..=130).contains(&pixel[1]));
        assert!((62..=66).contains(&pixel[2]));
        assert!((126..=130).contains(&pixel[3]));
    }

    #[test]
    fn png_and_svg_follow_the_same_shape_rules() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><circle cx="32" cy="32" r="28" fill="red"/></svg>"#;
        let first = rasterize(svg).unwrap();
        let mut png = Cursor::new(Vec::new());
        first.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let second = rasterize(png.get_ref()).unwrap();
        assert_eq!(analyze(&first), analyze(&second));
        assert!(
            !analyze(&second)
                .unwrap()
                .kind
                .needs_frame(cosmic::cosmic_theme::Roundness::Round)
        );
    }

    #[test]
    fn stacked_folder_artwork_is_framed_in_every_theme() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64">
            <rect x="13" y="3" width="38" height="34" rx="2.125" fill="#3691bb"/>
            <rect x="10" y="9" width="44" height="34" rx="2.125" fill="#35a96d"/>
            <rect x="7" y="15" width="50" height="34" rx="2.125" fill="#ea9e00"/>
            <rect x="4" y="21" width="56" height="39" rx="2.8" fill="#8e8e8e"/>
        </svg>"##;
        let image = rasterize(svg).unwrap();
        let shape = analyze(&image).unwrap().kind;
        assert_eq!(shape, super::super::IconShape::Freeform);
        assert!(!shape.can_crop());
        for theme in [
            cosmic::cosmic_theme::Roundness::Round,
            cosmic::cosmic_theme::Roundness::SlightlyRound,
            cosmic::cosmic_theme::Roundness::Square,
        ] {
            assert!(shape.needs_frame(theme));
        }
    }

    #[test]
    fn rectangles_render_each_theme_without_added_frames() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rounded.svg");
        for rect in [
            r#"x="3" y="3" width="58" height="58" rx="17.4""#,
            r#"x="4" y="8" width="56" height="48" rx="4.6""#,
            r#"x="4" y="7" width="56" height="50" rx="2.8""#,
        ] {
            std::fs::write(
                &path,
                format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64"><rect {rect} fill="gray"/></svg>"#),
            )
            .unwrap();
            let icon = prepare(path.to_str().unwrap()).unwrap();
            assert_eq!(icon.shape, super::super::IconShape::Rectangular);
            for theme in [
                cosmic::cosmic_theme::Roundness::Round,
                cosmic::cosmic_theme::Roundness::SlightlyRound,
                cosmic::cosmic_theme::Roundness::Square,
            ] {
                assert!(!icon.shape.needs_frame(theme));
                let icon::Data::Image(cosmic::iced::core::image::Handle::Rgba {
                    width,
                    height,
                    pixels,
                    ..
                }) = icon.handle(theme).data
                else {
                    panic!("Expected a prepared raster icon");
                };
                let image = RgbaImage::from_raw(width, height, pixels.as_ref().to_vec()).unwrap();
                assert_eq!((width, height), (CANVAS, CANVAS));
                assert_eq!(
                    image.get_pixel(20, 20)[3] >= 250,
                    theme == cosmic::cosmic_theme::Roundness::Square,
                    "{rect}, {theme:?}: {:?}",
                    image.get_pixel(20, 20)
                );
                assert_eq!(
                    image.get_pixel(30, 30)[3] >= 250,
                    theme != cosmic::cosmic_theme::Roundness::Round
                );
                assert!(image.get_pixel(128, 20)[3] >= 250);
            }
        }
    }

    #[test]
    fn malformed_and_oversized_inputs_fall_back_to_the_original_icon() {
        assert!(rasterize(b"not an icon").is_none());
        assert!(rasterize(&vec![0; MAX_FILE_BYTES as usize + 1]).is_none());
        let image = image::RgbaImage::new(2049, 1);
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        assert!(rasterize(png.get_ref()).is_none());
    }
}
