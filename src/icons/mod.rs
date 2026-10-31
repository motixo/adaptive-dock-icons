// SPDX-License-Identifier: GPL-3.0-only
//! Cached icon preparation and native, theme-aware presentation.

mod load;
mod shape;

use cosmic::{
    Element, Theme,
    cosmic_theme::Roundness,
    iced::Length,
    widget::{self, container, icon},
};
use load::prepare;
pub use shape::IconShape;

#[derive(Debug, Clone)]
pub struct PreparedIcon {
    handle: icon::Handle,
    cropped: Option<[icon::Handle; 3]>,
    pub shape: IconShape,
}

impl PreparedIcon {
    pub fn handle(&self, roundness: Roundness) -> icon::Handle {
        match (&self.cropped, roundness) {
            (Some(handles), Roundness::Round) => handles[0].clone(),
            (Some(handles), Roundness::SlightlyRound) => handles[1].clone(),
            (Some(handles), Roundness::Square) => handles[2].clone(),
            _ => self.handle.clone(),
        }
    }
}

pub fn view<Message: 'static>(
    icon: icon::Icon,
    shape: Option<IconShape>,
    size: u16,
    logo_scale: u16,
    theme: &Theme,
) -> Element<'static, Message> {
    let size = f32::from(size);
    let roundness = theme.cosmic().corner_radii.into();
    let framed = shape.is_some_and(|shape| shape.needs_frame(roundness));
    if !framed {
        return icon.width(size.into()).height(size.into()).into();
    }
    let edge = size * shape::CONTENT_SCALE;
    let default_logo_size = edge
        * match roundness {
            Roundness::Round => std::f32::consts::FRAC_1_SQRT_2,
            _ => 0.80,
        };
    // Keep the complete logo canvas inside the frame's rounded corners.
    let maximum_logo_size =
        edge * (1.0 - (2.0 - std::f32::consts::SQRT_2) * shape::corner_radius(roundness));
    let logo_size =
        (default_logo_size * f32::from(logo_scale.max(1)) / 100.0).min(maximum_logo_size);
    let style = frame_style(theme, edge);
    let frame = widget::container(icon.width(logo_size.into()).height(logo_size.into()))
        .align_x(cosmic::iced::Alignment::Center)
        .align_y(cosmic::iced::Alignment::Center)
        .width(edge)
        .height(edge)
        .class(cosmic::theme::Container::custom(move |_| style));
    widget::container(frame).center(Length::Fixed(size)).into()
}

fn frame_style(theme: &Theme, edge: f32) -> container::Style {
    let radius = edge * shape::corner_radius(theme.cosmic().corner_radii.into());
    container::Style {
        background: Some(theme.cosmic().bg_color().into()),
        border: cosmic::iced::Border {
            radius: radius.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub type LoadedIcons = (String, Vec<(String, Option<PreparedIcon>)>);

pub fn subscription(theme: String, names: Vec<String>) -> cosmic::iced::Subscription<LoadedIcons> {
    if names.is_empty() {
        return cosmic::iced::Subscription::none();
    }
    cosmic::iced::Subscription::run_with((theme, names), |request| {
        let (theme, names) = request.clone();
        futures::stream::once(async move {
            let loaded = tokio::task::spawn_blocking(move || {
                names
                    .into_iter()
                    .map(|name| {
                        let icon = prepare(&name);
                        (name, icon)
                    })
                    .collect()
            })
            .await
            .unwrap_or_else(|error| {
                tracing::error!(%error, "Icon preparation failed");
                Vec::new()
            });
            (theme, loaded)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::{Background, Color};
    use std::sync::Arc;

    fn icon_layout(
        shape: Option<IconShape>,
        scale: u16,
        theme: &Theme,
    ) -> cosmic::iced::advanced::layout::Node {
        use cosmic::iced::{Font, Limits, Pixels, advanced::widget::Tree};

        let renderer = cosmic::Renderer::new(Font::default(), Pixels(16.0));
        let icon = widget::icon(icon::from_raster_pixels(2, 2, vec![255; 16]));
        let mut element: Element<'_, ()> = view(icon, shape, 40, scale, theme);
        let mut tree = Tree::new(element.as_widget());
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &Limits::NONE)
    }

    #[test]
    fn logo_scale_changes_padding_without_resizing_or_moving_the_frame() {
        for roundness in [
            Roundness::Square,
            Roundness::SlightlyRound,
            Roundness::Round,
        ] {
            let mut theme = cosmic::cosmic_theme::Theme::dark_default();
            theme.corner_radii = roundness.into();
            let theme = Theme::system(Arc::new(theme));
            let original = icon_layout(Some(IconShape::Freeform), 100, &theme);
            let frame = &original.children()[0];
            let original_logo = frame.children()[0].bounds();
            for scale in [0, 80, 100, 115, 125, u16::MAX] {
                let layout = icon_layout(Some(IconShape::Freeform), scale, &theme);
                assert_eq!(layout.bounds(), original.bounds());
                let scaled_frame = &layout.children()[0];
                assert_eq!(scaled_frame.bounds(), frame.bounds());
                let logo = scaled_frame.children()[0].bounds();
                let edge = frame.size().width;
                assert!((logo.center_x() - edge / 2.0).abs() < 0.001);
                assert!((logo.center_y() - edge / 2.0).abs() < 0.001);
                assert!(logo.width <= edge && logo.height <= edge);
                if scale < 100 {
                    assert!(logo.width < original_logo.width);
                } else if scale == 100 {
                    assert_eq!(logo, original_logo);
                } else if roundness != Roundness::Round {
                    assert!(logo.width > original_logo.width);
                }
            }
            let largest = icon_layout(Some(IconShape::Freeform), u16::MAX, &theme);
            let logo = largest.children()[0].children()[0].bounds();
            let radius = frame.size().width * shape::corner_radius(roundness);
            let distance = (logo.x - radius).min(0.0).hypot((logo.y - radius).min(0.0));
            assert!(
                distance <= radius + 0.001,
                "Logo extends beyond {roundness:?} corners"
            );
        }
    }

    #[test]
    fn icons_without_added_frames_keep_their_size() {
        let mut theme = cosmic::cosmic_theme::Theme::dark_default();
        theme.corner_radii = Roundness::Square.into();
        let theme = Theme::system(Arc::new(theme));
        for shape in [None, Some(IconShape::Rectangular)] {
            let original = icon_layout(shape, 100, &theme);
            for scale in [0, 80, 125, u16::MAX] {
                assert_eq!(
                    icon_layout(shape, scale, &theme).bounds(),
                    original.bounds()
                );
            }
        }
    }

    #[test]
    fn frame_geometry_and_color_follow_the_system_theme() {
        for mut theme in [
            cosmic::cosmic_theme::Theme::dark_default(),
            cosmic::cosmic_theme::Theme::light_default(),
        ] {
            theme.accent.base = cosmic::cosmic_theme::palette::Srgba::new(0.8, 0.7, 0.6, 1.0);
            for (roundness, radius) in [
                (Roundness::Round, 20.0),
                (Roundness::SlightlyRound, 5.0),
                (Roundness::Square, 0.0),
            ] {
                theme.corner_radii = roundness.into();
                let theme = Theme::system(Arc::new(theme.clone()));
                let style = frame_style(&theme, 40.0);
                assert_eq!(style.border.radius, radius.into());
                assert_eq!(
                    style.background,
                    Some(Background::Color(Color::from(theme.cosmic().bg_color())))
                );
            }
        }
    }
}
