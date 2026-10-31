// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use crate::config::{APP_ID, AppListConfig};
use crate::{
    fl,
    icons::{self, PreparedIcon},
    wayland_subscription::{
        OutputUpdate, ToplevelCapabilities, ToplevelRequest, ToplevelUpdate, WaylandImage,
        WaylandRequest, WaylandUpdate, wayland_subscription,
    },
};
use cctk::{
    sctk::{output::OutputInfo, reexports::calloop::channel::Sender},
    toplevel_info::ToplevelInfo,
    wayland_client::protocol::{
        wl_data_device_manager::DndAction, wl_output::WlOutput, wl_seat::WlSeat,
    },
    wayland_protocols::ext::{
        foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
        workspace::v1::client::ext_workspace_handle_v1::ExtWorkspaceHandleV1,
    },
};
use cosmic::{
    Apply, Element, Task, app,
    applet::{
        Context, Size,
        cosmic_panel_config::{PanelAnchor, PanelSize},
    },
    cosmic_config::{Config, CosmicConfigEntry},
    desktop::IconSourceExt,
    iced::{
        self, Alignment, Background, Border, Length, Limits, Padding, Subscription,
        advanced::text::{Ellipsize, EllipsizeHeightLimit},
        clipboard::mime::{AllowedMimeTypes, AsMimeTypes},
        event::listen_with,
        platform_specific::shell::commands::popup::destroy_popup,
        runtime::{core::event, dnd::peek_dnd},
        widget::{
            Column, Row, column, mouse_area, row,
            rule::vertical as vertical_rule,
            space::{horizontal as horizontal_space, vertical as vertical_space},
            stack,
        },
        window,
    },
    surface::{self, action::LiveSettings},
    theme::{self, Button, Container},
    widget::{
        DndDestination, Image, button, container, divider, dnd_source,
        icon::{self, from_name},
        image::Handle,
        menu,
        rectangle_tracker::{RectangleTracker, RectangleUpdate, rectangle_tracker_subscription},
        svg, text,
    },
};
use cosmic::{
    desktop::fde::{self, DesktopEntry, get_languages_from_env, unicase::Ascii},
    widget::DndSource,
};
use cosmic_protocols::toplevel_info::v1::client::zcosmic_toplevel_handle_v1::State;
use futures::future::pending;
use rustc_hash::FxHashMap;
use std::{borrow::Cow, path::PathBuf, rc::Rc, time::Duration};
use switcheroo_control::Gpu;
use tokio::time::sleep;
use url::Url;

static MIME_TYPE: &str = "text/uri-list";

pub fn run() -> cosmic::iced::Result {
    cosmic::icon_theme::set_default(cosmic::config::icon_theme());
    cosmic::applet::run::<CosmicAppList>(())
}

#[derive(Debug, Clone)]
struct AppletIconData {
    icon_size: u16,
    icon_spacing: f32,
    dot_radius: f32,
    bar_size: f32,
    padding: Padding,
}

static DND_FAVORITES: u64 = u64::MAX;

impl AppletIconData {
    fn new(applet: &Context) -> Self {
        let icon_size = applet.suggested_size(false).0;
        let (major_padding, cross_padding) = applet.suggested_padding(false);
        let (h_padding, v_padding) = if applet.is_horizontal() {
            (major_padding as f32, cross_padding as f32)
        } else {
            (cross_padding as f32, major_padding as f32)
        };
        let icon_spacing = applet.spacing as f32;

        let (dot_radius, bar_size) = match applet.size {
            Size::Hardcoded(_) => (2.0, 8.0),
            Size::PanelSize(ref s) => {
                let size = s.get_applet_icon_size_with_padding(false);
                let small_size_threshold = PanelSize::S.get_applet_icon_size_with_padding(false);
                let medium_size_threshold = PanelSize::M.get_applet_icon_size_with_padding(false);
                if size <= small_size_threshold {
                    (1.0, 8.0)
                } else if size <= medium_size_threshold {
                    (2.0, 8.0)
                } else {
                    (2.0, 12.0)
                }
            }
        };
        let padding = match applet.anchor {
            PanelAnchor::Top => [
                v_padding - (dot_radius * 2. + 1.),
                h_padding,
                v_padding,
                h_padding,
            ],
            PanelAnchor::Bottom => [
                v_padding,
                h_padding,
                v_padding - (dot_radius * 2. + 1.),
                h_padding,
            ],
            PanelAnchor::Left => [
                v_padding,
                h_padding,
                v_padding,
                h_padding - (dot_radius * 2. + 1.),
            ],
            PanelAnchor::Right => [
                v_padding,
                h_padding - (dot_radius * 2. + 1.),
                v_padding,
                h_padding,
            ],
        };
        AppletIconData {
            icon_size,
            icon_spacing,
            dot_radius,
            bar_size,
            padding: padding.into(),
        }
    }

    fn with_icon_size(mut self, requested: u16) -> Self {
        if requested == 0 {
            return self;
        }

        // Use the existing button padding as space for enlargement. Compensate
        // on every edge so the button, dock, overflow and drop targets keep
        // their original dimensions, including the running-window indicator.
        let spare = self
            .padding
            .top
            .min(self.padding.right)
            .min(self.padding.bottom)
            .min(self.padding.left)
            .max(0.0);
        let maximum = (f32::from(self.icon_size) + 2.0 * spare) as u16;
        let minimum = self.bar_size.min(f32::from(self.icon_size)) as u16;
        let size = requested.clamp(minimum, maximum);
        let inset = (f32::from(self.icon_size) - f32::from(size)) / 2.0;
        self.padding.top += inset;
        self.padding.right += inset;
        self.padding.bottom += inset;
        self.padding.left += inset;
        self.icon_size = size;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DockItemId {
    Item(u32),
    ActiveOverflow,
    FavoritesOverflow,
}

impl From<u32> for DockItemId {
    fn from(id: u32) -> Self {
        DockItemId::Item(id)
    }
}

impl From<usize> for DockItemId {
    fn from(id: usize) -> Self {
        DockItemId::Item(id as u32)
    }
}

#[derive(Debug, Clone)]
struct DockItem {
    id: u32,
    toplevels: Vec<(ToplevelInfo, Option<WaylandImage>)>,
    desktop_info: DesktopEntry,
    // DesktopEntry IDs are heuristic; unpinning must use the original config key.
    original_app_id: String,
}

#[derive(Debug, Clone, Default)]
struct DndOffer {
    dock_item: Option<DockItem>,
    preview_index: usize,
}

#[derive(Debug, Clone)]
pub struct Popup {
    parent: window::Id,
    id: window::Id,
    dock_item: DockItem,
    popup_type: PopupType,
}

#[derive(Clone, Default)]
struct CosmicAppList {
    core: cosmic::app::Core,
    popup: Option<Popup>,
    subscription_ctr: u32,
    item_ctr: u32,
    desktop_entries: Vec<DesktopEntry>,
    icons: FxHashMap<String, Option<PreparedIcon>>,
    icon_theme: String,
    active_list: Vec<DockItem>,
    pinned_list: Vec<DockItem>,
    dnd_source: Option<(window::Id, DockItem, DndAction, Option<usize>)>,
    config: AppListConfig,
    wayland_sender: Option<Sender<WaylandRequest>>,
    capabilities: ToplevelCapabilities,
    seat: Option<WlSeat>,
    rectangle_tracker: Option<RectangleTracker<DockItemId>>,
    rectangles: FxHashMap<DockItemId, iced::Rectangle>,
    dnd_offer: Option<DndOffer>,
    is_listening_for_dnd: bool,
    gpus: Option<Vec<Gpu>>,
    active_workspaces: Vec<ExtWorkspaceHandleV1>,
    output_list: FxHashMap<WlOutput, OutputInfo>,
    locales: Vec<String>,
    hovered_toplevel: Option<ExtForeignToplevelHandleV1>,
    overflow_favorites_popup: Option<window::Id>,
    overflow_active_popup: Option<window::Id>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PopupType {
    RightClickMenu,
    ToplevelList,
}

#[derive(Debug, Clone)]
enum Message {
    Wayland(Box<WaylandUpdate>),
    PinApp(u32),
    UnpinApp(u32),
    Popup(u32, window::Id),
    Pressed(window::Id),
    ToplevelListPopup(u32, window::Id),
    ToplevelHoverChanged(ExtForeignToplevelHandleV1, bool),
    GpuRequest(Option<Vec<Gpu>>),
    CloseRequested(window::Id),
    ClosePopup,
    Activate(ExtForeignToplevelHandleV1),
    Toggle(ExtForeignToplevelHandleV1),
    Exec(String, Option<usize>, bool),
    CloseToplevel(ExtForeignToplevelHandleV1),
    Quit(String),
    NewSeat(WlSeat),
    RemovedSeat,
    Rectangle(RectangleUpdate<DockItemId>),
    StartDrag(u32),
    DragFinished,
    DndEnter(f64, f64),
    DndLeave,
    DndMotion(f64, f64),
    DndDropFinished,
    DndData(Option<DndPathBuf>),
    IncrementSubscriptionCtr,
    ConfigUpdated(AppListConfig),
    IconsReady(icons::LoadedIcons),
    OpenFavorites,
    OpenActive,
    Surface(surface::Action<Message>),
}

fn index_in_list(
    mut list_len: usize,
    item_size: f32,
    divider_size: f32,
    existing_preview: Option<usize>,
    pos_in_list: f32,
) -> usize {
    if existing_preview.is_some() {
        list_len += 1;
    }

    let index = if (list_len == 0) || (pos_in_list < item_size / 2.0) {
        0
    } else {
        let mut i = 1;
        let mut pos = item_size / 2.0;
        while i < list_len {
            let next_pos = pos + item_size + divider_size;
            if pos < pos_in_list && pos_in_list < next_pos {
                break;
            }
            pos = next_pos;
            i += 1;
        }
        i
    };

    if let Some(existing_preview) = existing_preview {
        if index >= existing_preview {
            index.saturating_sub(1)
        } else {
            index
        }
    } else {
        index
    }
}

async fn try_get_gpus() -> Option<Vec<Gpu>> {
    let connection = zbus::Connection::system().await.ok()?;
    let proxy = switcheroo_control::SwitcherooControlProxy::new(&connection)
        .await
        .ok()?;

    if !proxy.has_dual_gpu().await.ok()? {
        return None;
    }

    let gpus = proxy.get_gpus().await.ok()?;
    if gpus.is_empty() {
        return None;
    }

    Some(gpus)
}

const TOPLEVEL_BUTTON_WIDTH: f32 = 192.0;
const TOPLEVEL_BUTTON_HEIGHT: f32 = 156.0;

fn toplevel_button<'a>(
    img: Option<WaylandImage>,
    title: String,
    handle: ExtForeignToplevelHandleV1,
    is_focused: bool,
    is_hovered: bool,
    capabilities: ToplevelCapabilities,
) -> Element<'a, Message> {
    let border = 1.0;
    let preview = column![
        container(if let Some(img) = img {
            Element::from(Image::new(Handle::from_rgba(
                img.width,
                img.height,
                img.img.clone(),
            )))
        } else {
            Image::new(Handle::from_rgba(1, 1, [0u8, 0u8, 0u8, 255u8].as_slice())).into()
        })
        .class(Container::custom(move |theme| container::Style {
            border: Border {
                color: theme.cosmic().bg_divider().into(),
                width: border,
                radius: 1.0.into(),
            },
            ..Default::default()
        }))
        .padding(border as u16)
        .apply(container)
        .center(Length::Fill),
        text::body(title)
            .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1)))
            .width(Length::Fill)
            .center()
    ]
    .spacing(4)
    .padding([4, 4, 0, 4]);
    let close_button_overlay = if is_hovered && capabilities.close {
        row![
            horizontal_space(),
            button::custom(icon::from_name("window-close-symbolic").size(16))
                .class(Button::Destructive)
                .on_press(Message::CloseToplevel(handle.clone()))
                .padding(4)
        ]
    } else {
        row![]
    }
    .width(Length::Fill)
    .height(Length::Fill);

    let preview = stack![preview, close_button_overlay]
        .apply(button::custom)
        .on_press_maybe(
            capabilities
                .can_toggle(is_focused)
                .then(|| Message::Toggle(handle.clone())),
        )
        .class(window_menu_style(is_focused))
        .width(Length::Fixed(TOPLEVEL_BUTTON_WIDTH))
        .height(Length::Fixed(TOPLEVEL_BUTTON_HEIGHT))
        .padding(4)
        .selected(is_focused)
        .apply(mouse_area)
        .on_enter(Message::ToplevelHoverChanged(handle.clone(), true))
        .on_exit(Message::ToplevelHoverChanged(handle.clone(), false));
    if capabilities.close {
        preview
            .on_middle_press(Message::CloseToplevel(handle))
            .into()
    } else {
        preview.into()
    }
}

fn window_menu_style(selected: bool) -> cosmic::theme::Button {
    let radius = theme::active()
        .cosmic()
        .radius_m()
        .map(|x| if x < 8.0 { x } else { x - 4.0 });

    Button::Custom {
        active: Box::new(move |focused, theme| {
            let a = button::Catalog::active(theme, focused, selected, &Button::AppletMenu);
            button::Style {
                background: if selected {
                    Some(Background::Color(
                        theme.cosmic().icon_button.selected_state_color().into(),
                    ))
                } else {
                    a.background
                },
                border_radius: radius.into(),
                outline_width: 0.0,
                ..a
            }
        }),
        hovered: Box::new(move |focused, theme| {
            let focused = selected || focused;
            let text = button::Catalog::hovered(theme, focused, focused, &Button::AppletMenu);
            button::Style {
                border_radius: radius.into(),
                outline_width: 0.0,
                ..text
            }
        }),
        disabled: Box::new(move |theme| {
            let text = button::Catalog::disabled(theme, &Button::AppletMenu);
            button::Style {
                border_radius: radius.into(),
                outline_width: 0.0,
                ..text
            }
        }),
        pressed: Box::new(move |focused, theme| {
            let focused = selected || focused;
            let text = button::Catalog::pressed(theme, focused, focused, &Button::AppletMenu);
            button::Style {
                border_radius: radius.into(),
                outline_width: 0.0,
                ..text
            }
        }),
    }
}

fn app_list_icon_style(selected: bool) -> cosmic::theme::Button {
    Button::Custom {
        active: Box::new(move |focused, theme| {
            let a = button::Catalog::active(theme, focused, selected, &Button::AppletIcon);
            button::Style {
                background: if selected {
                    Some(Background::Color(
                        theme.cosmic().icon_button.selected_state_color().into(),
                    ))
                } else {
                    a.background
                },
                ..a
            }
        }),
        hovered: Box::new(move |focused, theme| {
            button::Catalog::hovered(theme, focused, selected, &Button::AppletIcon)
        }),
        disabled: Box::new(|theme| button::Catalog::disabled(theme, &Button::AppletIcon)),
        pressed: Box::new(move |focused, theme| {
            button::Catalog::pressed(theme, focused, selected, &Button::AppletIcon)
        }),
    }
}

#[inline]
pub fn menu_control_padding() -> Padding {
    let spacing = theme::spacing();
    [spacing.space_xxs, spacing.space_s].into()
}

fn find_desktop_entries<'a>(
    desktop_entries: &'a [fde::DesktopEntry],
    app_ids: &'a [String],
) -> impl Iterator<Item = fde::DesktopEntry> + 'a {
    app_ids.iter().map(|fav| {
        let unicase_fav = fde::unicase::Ascii::new(fav.as_str());
        fde::find_app_by_id(desktop_entries, unicase_fav).map_or_else(
            || fde::DesktopEntry::from_appid(fav.clone()),
            ToOwned::to_owned,
        )
    })
}

impl CosmicAppList {
    fn item_icon<'a>(
        &'a self,
        item: &'a DockItem,
        window_id: window::Id,
        interaction_enabled: bool,
        track_rectangle: bool,
    ) -> Element<'a, Message> {
        let applet = &self.core.applet;
        let prepared = self.prepared_icon(item);
        let rectangle_tracker = track_rectangle
            .then_some(self.rectangle_tracker.as_ref())
            .flatten();
        let dnd_source_enabled = self.config.enable_drag_source;
        let gpus = self.gpus.as_deref();
        let is_focused = item
            .toplevels
            .iter()
            .filter(|(info, _)| self.is_on_current_monitor_and_workspace(info))
            .any(|(info, _)| self.is_focused(&info.foreign_toplevel));
        let dot_border_radius = self.core.system_theme().cosmic().radius_xs();
        let DockItem {
            toplevels,
            desktop_info,
            id,
            ..
        } = item;

        let filtered_toplevels: Vec<_> = toplevels
            .iter()
            .filter(|(info, _)| self.is_on_current_monitor_and_workspace(info))
            .collect();
        let toplevel_count = filtered_toplevels.len();

        let app_icon = AppletIconData::new(applet).with_icon_size(self.config.icon_size);
        let roundness = self.core.system_theme().cosmic().corner_radii.into();

        let cosmic_icon =
            cosmic::widget::icon(prepared.map(|icon| icon.handle(roundness)).unwrap_or_else(
                || {
                    fde::IconSource::from_unknown(desktop_info.icon().unwrap_or_default())
                        .as_cosmic_icon()
                },
            ))
            .size(128)
            .width(app_icon.icon_size.into())
            .height(app_icon.icon_size.into());

        let shape = prepared.map(|icon| icon.shape);
        let icon_size = app_icon.icon_size;
        let logo_scale = self.config.logo_scale;
        let system_theme = self.core.system_theme().clone();
        let decorate_icon = move || {
            icons::view(
                cosmic_icon.clone(),
                shape,
                icon_size,
                logo_scale,
                &system_theme,
            )
        };
        let indicator = {
            let container = if toplevel_count <= 1 {
                vertical_space().height(Length::Fixed(0.0))
            } else {
                match applet.anchor {
                    PanelAnchor::Left | PanelAnchor::Right => {
                        vertical_space().height(app_icon.bar_size)
                    }
                    PanelAnchor::Top | PanelAnchor::Bottom => {
                        horizontal_space().width(app_icon.bar_size)
                    }
                }
            }
            .apply(container)
            .padding(app_icon.dot_radius);

            if toplevel_count == 0 {
                container
            } else {
                container.class(theme::Container::custom(move |theme| container::Style {
                    background: if is_focused {
                        Some(Background::Color(theme.cosmic().accent_color().into()))
                    } else {
                        Some(Background::Color(theme.cosmic().on_bg_color().into()))
                    },
                    border: Border {
                        radius: dot_border_radius.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }))
            }
        };

        let icon_wrapper: Element<_> = match applet.anchor {
            PanelAnchor::Left => row([
                indicator.into(),
                horizontal_space().width(Length::Fixed(1.0)).into(),
                decorate_icon(),
            ])
            .align_y(Alignment::Center)
            .into(),
            PanelAnchor::Right => row([
                decorate_icon(),
                horizontal_space().width(Length::Fixed(1.0)).into(),
                indicator.into(),
            ])
            .align_y(Alignment::Center)
            .into(),
            PanelAnchor::Top => column([
                indicator.into(),
                vertical_space().height(Length::Fixed(1.0)).into(),
                decorate_icon(),
            ])
            .align_x(Alignment::Center)
            .into(),
            PanelAnchor::Bottom => column([
                decorate_icon(),
                vertical_space().height(Length::Fixed(1.0)).into(),
                indicator.into(),
            ])
            .align_x(Alignment::Center)
            .into(),
        };

        let icon_button = button::custom(icon_wrapper)
            .padding(app_icon.padding)
            .selected(is_focused)
            .class(app_list_icon_style(is_focused));

        let icon_button: Element<_> = if interaction_enabled {
            mouse_area(
                icon_button
                    .on_press_maybe(if toplevel_count == 0 {
                        launch_on_preferred_gpu(desktop_info, gpus)
                    } else if toplevel_count == 1 {
                        filtered_toplevels
                            .first()
                            .filter(|_| self.capabilities.can_toggle(is_focused))
                            .map(|t| Message::Toggle(t.0.foreign_toplevel.clone()))
                    } else {
                        Some(Message::ToplevelListPopup(*id, window_id))
                    })
                    .width(Length::Shrink)
                    .height(Length::Shrink),
            )
            .on_right_release(Message::Popup(*id, window_id))
            .on_middle_release({
                launch_on_preferred_gpu(desktop_info, gpus)
                    .unwrap_or(Message::Popup(*id, window_id))
            })
            .into()
        } else {
            icon_button.into()
        };

        let path = desktop_info.path.clone();
        let icon_button = if dnd_source_enabled && interaction_enabled {
            DndSource::with_id(
                icon_button,
                cosmic::widget::Id::new(format!("adaptive-dock-icons-item-{id}")),
            )
            .window(window_id)
            .drag_icon(move |_| {
                (
                    decorate_icon().map(|_| ()),
                    iced::core::widget::tree::State::None,
                    iced::Vector::ZERO,
                )
            })
            .drag_threshold(16.)
            .drag_content(move || DndPathBuf(path.clone()))
            .on_start(Some(Message::StartDrag(*id)))
            .on_cancel(Some(Message::DragFinished))
            .on_finish(Some(Message::DragFinished))
        } else {
            dnd_source(icon_button)
        };

        if let Some(tracker) = rectangle_tracker {
            tracker.container((*id).into(), icon_button).into()
        } else {
            icon_button.into()
        }
    }

    fn icon_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self
            .pinned_list
            .iter()
            .chain(&self.active_list)
            .chain(self.popup.iter().map(|popup| &popup.dock_item))
            .filter_map(|item| item.desktop_info.icon().map(str::to_owned))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn prepared_icon(&self, item: &DockItem) -> Option<&PreparedIcon> {
        if self.icon_theme != cosmic::icon_theme::default() {
            return None;
        }
        self.icons.get(item.desktop_info.icon()?)?.as_ref()
    }

    fn update_desktop_entries(&mut self) {
        self.desktop_entries = fde::Iter::new(fde::default_paths())
            .filter_map(|p| fde::DesktopEntry::from_path(p, Some(&self.locales)).ok())
            .collect::<Vec<_>>();
    }

    fn is_on_current_monitor_and_workspace(&self, toplevel_info: &ToplevelInfo) -> bool {
        use crate::config::ToplevelFilter;

        let on_active_workspace = self.active_workspaces.is_empty()
            || toplevel_info.workspace.is_empty()
            || self
                .active_workspaces
                .iter()
                .any(|workspace| toplevel_info.workspace.contains(workspace));

        match &self.config.filter_top_levels {
            None => true,
            Some(ToplevelFilter::ActiveWorkspace) => on_active_workspace,
            Some(ToplevelFilter::ConfiguredOutput) => {
                let on_active_output = self
                    .output_list
                    .iter()
                    .find(|(_, info)| info.name.as_ref() == Some(&self.core.applet.output_name))
                    .is_none_or(|(active_output, _)| {
                        toplevel_info
                            .output
                            .iter()
                            .any(|output| output == active_output)
                    });

                on_active_output && on_active_workspace
            }
        }
    }

    fn visible_active_items(&self) -> impl Iterator<Item = &DockItem> {
        self.active_list.iter().filter(|item| {
            item.toplevels
                .iter()
                .any(|(info, _)| self.is_on_current_monitor_and_workspace(info))
        })
    }

    fn update_pinned_list(&mut self) {
        self.pinned_list = find_desktop_entries(&self.desktop_entries, &self.config.favorites)
            .zip(&self.config.favorites)
            .enumerate()
            .map(|(pinned_ctr, (e, original_id))| DockItem {
                id: pinned_ctr as u32,
                toplevels: Vec::new(),
                desktop_info: e,
                original_app_id: original_id.clone(),
            })
            .collect();
    }

    fn close_popups(&mut self) -> Task<cosmic::Action<Message>> {
        let mut commands = Vec::new();
        if let Some(popup) = self.popup.take() {
            commands.push(destroy_popup(popup.id));
        }
        if let Some(popup) = self.overflow_active_popup.take() {
            commands.push(destroy_popup(popup));
        }
        if let Some(popup) = self.overflow_favorites_popup.take() {
            commands.push(destroy_popup(popup));
        }
        Task::batch(commands)
    }

    /// Keep running favorites visible before moving idle favorites into overflow.
    fn panel_overflow_lengths(&self) -> (Option<usize>, Option<usize>) {
        let mut favorite_index;
        let mut active_index = None;
        let Some(mut max_major_axis_len) =
            self.core
                .applet
                .suggested_bounds
                .as_ref()
                .map(|c| match self.core.applet.anchor {
                    PanelAnchor::Top | PanelAnchor::Bottom => c.width as u32,
                    PanelAnchor::Left | PanelAnchor::Right => c.height as u32,
                })
        else {
            return (None, active_index);
        };
        max_major_axis_len = max_major_axis_len.saturating_sub(1);
        let applet_icon = AppletIconData::new(&self.core.applet);

        let button_total_size = self.core.applet.suggested_size(true).0
            + self.core.applet.suggested_padding(true).0 * 2
            + applet_icon.icon_spacing as u16;

        let favorite_active_cnt = self
            .pinned_list
            .iter()
            .filter(|t| !t.toplevels.is_empty())
            .count();

        let active_count = self.visible_active_items().count();
        let btn_count = max_major_axis_len / u32::from(button_total_size.max(1));
        if btn_count >= self.pinned_list.len() as u32 + active_count as u32 {
            return (None, active_index);
        } else {
            favorite_index = (btn_count as usize).min(favorite_active_cnt).max(2);
        }

        let active_index_max = (btn_count as i32)
            - (self.pinned_list.len() as i32).saturating_sub(favorite_index as i32);
        if active_index_max >= active_count as i32 {
            active_index = Some(active_count);
        } else {
            active_index = Some((active_index_max.max(2) as usize).min(active_count));
        }

        if let Some(active_index) = active_index {
            let favorite_index_max = (btn_count as i32) - active_index as i32;
            favorite_index = favorite_index_max.max(2) as usize;
        } else {
            favorite_index = (btn_count as usize).min(self.pinned_list.len());
        }
        (Some(favorite_index), active_index)
    }

    fn currently_active_toplevel(&self) -> Vec<ExtForeignToplevelHandleV1> {
        if self.active_workspaces.is_empty() {
            return Vec::new();
        }
        let current_output = &self.core.applet.output_name;
        let mut focused_toplevels: Vec<ExtForeignToplevelHandleV1> = Vec::new();
        let active_workspaces = &self.active_workspaces;
        for toplevel_list in self.active_list.iter().chain(self.pinned_list.iter()) {
            for (t_info, _) in &toplevel_list.toplevels {
                if t_info.state.contains(&State::Activated)
                    && active_workspaces
                        .iter()
                        .any(|workspace| t_info.workspace.contains(workspace))
                    && t_info.output.iter().any(|x| {
                        self.output_list.get(x).is_some_and(|val| {
                            val.name.as_ref().is_some_and(|n| n == current_output)
                        })
                    })
                {
                    focused_toplevels.push(t_info.foreign_toplevel.clone());
                }
            }
        }
        focused_toplevels
    }

    fn find_desktop_entry_for_toplevel(
        &mut self,
        info: &ToplevelInfo,
        unicase_appid: Ascii<&str>,
    ) -> DesktopEntry {
        if let Some(appid) = fde::find_app_by_id(&self.desktop_entries, unicase_appid) {
            appid.clone()
        } else {
            self.update_desktop_entries();
            if let Some(appid) = fde::find_app_by_id(&self.desktop_entries, unicase_appid) {
                appid.clone()
            } else {
                tracing::error!(id = info.app_id, "could not find desktop entry for app");
                let mut fallback_entry = fde::DesktopEntry::from_appid(info.app_id.clone());
                // Non-Steam Proton and Wine windows may only match their desktop entry by title.
                let is_proton_game = info.app_id == "steam_app_default";
                if is_proton_game || info.app_id.ends_with(".exe") {
                    for entry in &self.desktop_entries {
                        let localised_name = entry.name(&self.locales).unwrap_or_default();
                        if localised_name == info.title {
                            if is_proton_game
                                && !entry.categories().unwrap_or_default().contains(&"Game")
                            {
                                continue;
                            }
                            fallback_entry = entry.clone();
                            break;
                        }
                    }
                }
                fallback_entry
            }
        }
    }

    fn is_focused(&self, handle: &ExtForeignToplevelHandleV1) -> bool {
        self.currently_active_toplevel().contains(handle)
    }

    fn is_hovered(&self, handle: &ExtForeignToplevelHandleV1) -> bool {
        self.hovered_toplevel.as_ref() == Some(handle)
    }
}

impl cosmic::Application for CosmicAppList {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &'static str = APP_ID;

    fn init(core: cosmic::app::Core, _flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let config = Config::new(APP_ID, AppListConfig::VERSION)
            .map(|config| {
                crate::config::load(
                    &config,
                    Config::new(crate::config::ORIGINAL_APP_ID, AppListConfig::VERSION)
                        .ok()
                        .as_ref(),
                )
            })
            .unwrap_or_default();

        let mut app_list = Self {
            core,
            config,
            locales: get_languages_from_env(),
            ..Default::default()
        };

        app_list.update_desktop_entries();
        app_list.update_pinned_list();

        app_list.item_ctr = app_list.pinned_list.len() as u32;

        (
            app_list,
            Task::perform(try_get_gpus(), |gpus| {
                cosmic::Action::App(Message::GpuRequest(gpus))
            }),
        )
    }

    fn core(&self) -> &cosmic::app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::app::Core {
        &mut self.core
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::IconsReady((theme, loaded)) => {
                if theme == cosmic::icon_theme::default() {
                    if self.icon_theme != theme {
                        self.icons.clear();
                        self.icon_theme = theme;
                    }
                    let names = self.icon_names();
                    self.icons.retain(|name, _| names.contains(name));
                    self.icons.extend(loaded);
                }
            }
            Message::Popup(id, parent_window_id) => {
                if let Some(Popup {
                    parent,
                    id: popup_id,
                    ..
                }) = self.popup.take()
                {
                    if parent == parent_window_id {
                        return destroy_popup(popup_id);
                    } else {
                        self.overflow_active_popup = None;
                        self.overflow_favorites_popup = None;
                        return Task::batch([destroy_popup(popup_id), destroy_popup(parent)]);
                    }
                }
                if let Some(toplevel_group) = self
                    .active_list
                    .iter()
                    .chain(self.pinned_list.iter())
                    .find(|t| t.id == id)
                    .cloned()
                {
                    let Some(rectangle) = self.rectangles.get(&toplevel_group.id.into()).copied()
                    else {
                        tracing::error!("No rectangle found for toplevel group");
                        return Task::none();
                    };
                    let popup_task =
                        cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                            move |_| LiveSettings::default(),
                            move |app: &mut Self| {
                                let new_id = window::Id::unique();
                                app.popup = Some(Popup {
                                    parent: parent_window_id,
                                    id: new_id,
                                    dock_item: toplevel_group.clone(),
                                    popup_type: PopupType::RightClickMenu,
                                });

                                let mut popup_settings = app.core.applet.get_popup_settings(
                                    parent_window_id,
                                    new_id,
                                    None,
                                    None,
                                    None,
                                );
                                let iced::Rectangle {
                                    x,
                                    y,
                                    width,
                                    height,
                                } = rectangle;
                                popup_settings.positioner.anchor_rect = iced::Rectangle::<i32> {
                                    x: x as i32,
                                    y: y as i32,
                                    width: width as i32,
                                    height: height as i32,
                                };
                                popup_settings
                            },
                            None,
                        ));

                    let gpu_update = Task::perform(try_get_gpus(), |gpus| {
                        cosmic::Action::App(Message::GpuRequest(gpus))
                    });
                    return Task::batch([gpu_update, popup_task]);
                }
            }
            Message::ToplevelListPopup(id, parent_window_id) => {
                if let Some(Popup {
                    parent,
                    id: popup_id,
                    ..
                }) = self.popup.take()
                {
                    if parent == parent_window_id {
                        return destroy_popup(popup_id);
                    } else {
                        self.overflow_active_popup = None;
                        self.overflow_favorites_popup = None;
                        return Task::batch([destroy_popup(popup_id), destroy_popup(parent)]);
                    }
                }
                if let Some(toplevel_group) = self
                    .active_list
                    .iter()
                    .chain(self.pinned_list.iter())
                    .find(|t| t.id == id)
                {
                    for (info, _) in &toplevel_group.toplevels {
                        if let Some(tx) = self.wayland_sender.as_ref() {
                            let _ =
                                tx.send(WaylandRequest::Screencopy(info.foreign_toplevel.clone()));
                        }
                    }

                    let Some(rectangle) = self.rectangles.get(&toplevel_group.id.into()).copied()
                    else {
                        return Task::none();
                    };
                    let new_id = window::Id::unique();
                    self.popup = Some(Popup {
                        parent: parent_window_id,
                        id: new_id,
                        dock_item: toplevel_group.clone(),
                        popup_type: PopupType::ToplevelList,
                    });
                    let popup_task =
                        cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                            |_| Default::default(),
                            move |app: &mut Self| {
                                let mut popup_settings = app.core.applet.get_popup_settings(
                                    parent_window_id,
                                    new_id,
                                    None,
                                    None,
                                    None,
                                );
                                let iced::Rectangle {
                                    x,
                                    y,
                                    width,
                                    height,
                                } = rectangle;
                                popup_settings.positioner.anchor_rect = iced::Rectangle::<i32> {
                                    x: x as i32,
                                    y: y as i32,
                                    width: width as i32,
                                    height: height as i32,
                                };
                                let max_windows = 7.0;
                                let window_spacing = 8.0;
                                popup_settings.positioner.size_limits = match app.core.applet.anchor
                                {
                                    PanelAnchor::Right | PanelAnchor::Left => Limits::NONE
                                        .min_width(100.0)
                                        .min_height(30.0)
                                        .max_width(window_spacing * 2.0 + TOPLEVEL_BUTTON_WIDTH)
                                        .max_height(
                                            TOPLEVEL_BUTTON_HEIGHT * max_windows
                                                + window_spacing * (max_windows + 1.0),
                                        ),
                                    PanelAnchor::Bottom | PanelAnchor::Top => Limits::NONE
                                        .min_width(30.0)
                                        .min_height(100.0)
                                        .max_width(
                                            TOPLEVEL_BUTTON_WIDTH * max_windows
                                                + window_spacing * (max_windows + 1.0),
                                        )
                                        .max_height(window_spacing * 2.0 + TOPLEVEL_BUTTON_HEIGHT),
                                };
                                popup_settings
                            },
                            None,
                        ));

                    return popup_task;
                }
            }
            Message::ToplevelHoverChanged(handle, entering) => {
                match (entering, &self.hovered_toplevel) {
                    (true, _) => self.hovered_toplevel = Some(handle),
                    (false, Some(h)) if h == &handle => self.hovered_toplevel = None,
                    _ => {}
                }
            }
            Message::PinApp(id) => {
                if let Some(i) = self.active_list.iter().position(|t| t.id == id) {
                    let entry = self.active_list.remove(i);
                    self.config.add_pinned(
                        entry.original_app_id.clone(),
                        &Config::new(APP_ID, AppListConfig::VERSION).unwrap(),
                    );
                    self.pinned_list.push(entry);
                }
                if let Some(Popup { id: popup_id, .. }) = self.popup.take() {
                    return destroy_popup(popup_id);
                }
            }
            Message::UnpinApp(id) => {
                if let Some(i) = self.pinned_list.iter().position(|t| t.id == id) {
                    let entry = self.pinned_list.remove(i);

                    self.config.remove_pinned(
                        &entry.original_app_id,
                        &Config::new(APP_ID, AppListConfig::VERSION).unwrap(),
                    );

                    self.rectangles.remove(&entry.id.into());
                    if !entry.toplevels.is_empty() {
                        self.active_list.push(entry);
                    }
                }
                if let Some(Popup { id: popup_id, .. }) = self.popup.take() {
                    return destroy_popup(popup_id);
                }
            }
            Message::Activate(handle) => {
                if let Some(tx) = self.wayland_sender.as_ref() {
                    let _ = tx.send(WaylandRequest::Toplevel(ToplevelRequest::Activate(handle)));
                }
                if let Some(p) = self.popup.take() {
                    return destroy_popup(p.id);
                }
            }
            Message::Toggle(handle) => {
                if let Some(tx) = self.wayland_sender.as_ref() {
                    let _ = tx.send(WaylandRequest::Toplevel(if self.is_focused(&handle) {
                        ToplevelRequest::Minimize(handle)
                    } else {
                        ToplevelRequest::Activate(handle)
                    }));
                }
                if let Some(p) = self.popup.take() {
                    return destroy_popup(p.id);
                }
            }
            Message::CloseToplevel(handle) => {
                if let Some(tx) = self.wayland_sender.as_ref() {
                    let _ = tx.send(WaylandRequest::Toplevel(ToplevelRequest::Quit(handle)));
                }
            }
            Message::Quit(id) => {
                if let Some(toplevel_group) = self
                    .active_list
                    .iter()
                    .chain(self.pinned_list.iter())
                    .find(|t| t.desktop_info.id() == id)
                {
                    for (info, _) in &toplevel_group.toplevels {
                        if let Some(tx) = self.wayland_sender.as_ref() {
                            let _ = tx.send(WaylandRequest::Toplevel(ToplevelRequest::Quit(
                                info.foreign_toplevel.clone(),
                            )));
                        }
                    }
                }
                if let Some(Popup { id: popup_id, .. }) = self.popup.take() {
                    return destroy_popup(popup_id);
                }
            }
            Message::StartDrag(id) => {
                if let Some((_, toplevel_group, pos)) = self
                    .active_list
                    .iter()
                    .find_map(|t| {
                        if t.id == id {
                            Some((false, t.clone(), None))
                        } else {
                            None
                        }
                    })
                    .or_else(|| {
                        self.pinned_list
                            .iter()
                            .position(|t| t.id == id)
                            .map(|pos| (true, self.pinned_list[pos].clone(), Some(pos)))
                    })
                {
                    let icon_id = window::Id::unique();
                    self.dnd_source =
                        Some((icon_id, toplevel_group.clone(), DndAction::empty(), pos));
                }
            }
            Message::DragFinished => {
                if let Some((_, mut toplevel_group, _, _pinned_pos)) = self.dnd_source.take() {
                    if self.dnd_offer.take().is_some()
                        && let Some((_, toplevel_group, _, pinned_pos)) = self.dnd_source.as_ref()
                    {
                        let mut pos = 0;
                        self.pinned_list.retain_mut(|pinned| {
                            let matched_id =
                                pinned.desktop_info.id() == toplevel_group.desktop_info.id();
                            let pinned_match =
                                pinned_pos.is_some_and(|pinned_pos| pinned_pos == pos);
                            let ret = !matched_id || pinned_match;

                            pos += 1;
                            ret
                        });
                    }

                    if !self
                        .pinned_list
                        .iter()
                        .chain(self.active_list.iter())
                        .any(|t| t.desktop_info.id() == toplevel_group.desktop_info.id())
                        && !toplevel_group.toplevels.is_empty()
                    {
                        self.item_ctr += 1;
                        toplevel_group.id = self.item_ctr;
                        self.active_list.push(toplevel_group);
                    }
                }
            }
            Message::DndEnter(x, y) => {
                let item_size = self.core.applet.suggested_size(false).0
                    + 2 * self.core.applet.suggested_padding(false).0;
                let pos_in_list = match self.core.applet.anchor {
                    PanelAnchor::Top | PanelAnchor::Bottom => x as f32,
                    PanelAnchor::Left | PanelAnchor::Right => y as f32,
                };
                let num_pinned = self.pinned_list.len();
                let index = index_in_list(num_pinned, item_size as f32, 4.0, None, pos_in_list);
                self.dnd_offer = Some(DndOffer {
                    preview_index: index,
                    ..DndOffer::default()
                });
                if let Some(dnd_source) = self.dnd_source.as_ref() {
                    self.dnd_offer.as_mut().unwrap().dock_item = Some(dnd_source.1.clone());
                } else {
                    return peek_dnd::<DndPathBuf>()
                        .map(Message::DndData)
                        .map(cosmic::Action::App);
                }
            }
            Message::DndMotion(x, y) => {
                let item_size = self.core.applet.suggested_size(false).0
                    + 2 * self.core.applet.suggested_padding(false).0;
                let pos_in_list = match self.core.applet.anchor {
                    PanelAnchor::Top | PanelAnchor::Bottom => x as f32,
                    PanelAnchor::Left | PanelAnchor::Right => y as f32,
                };
                let num_pinned = self.pinned_list.len();
                let index = index_in_list(
                    num_pinned,
                    item_size as f32,
                    4.0,
                    self.dnd_offer.as_ref().map(|o| o.preview_index),
                    pos_in_list,
                );
                if let Some(o) = self.dnd_offer.as_mut() {
                    o.preview_index = index;
                }
            }
            Message::DndLeave => {
                if let Some((_, toplevel_group, _, pinned_pos)) = self.dnd_source.as_ref() {
                    let mut pos = 0;
                    self.pinned_list.retain_mut(|pinned| {
                        let matched_id =
                            pinned.desktop_info.id() == toplevel_group.desktop_info.id();
                        let pinned_match = pinned_pos.is_some_and(|pinned_pos| pinned_pos == pos);
                        let ret = !matched_id || pinned_match;

                        pos += 1;
                        ret
                    });
                }
                self.dnd_offer = None;
            }
            Message::DndData(file_path) => {
                let Some(file_path) = file_path else {
                    tracing::error!("Couldn't peek at hovered path.");
                    return Task::none();
                };
                if let Some(DndOffer { dock_item, .. }) = self.dnd_offer.as_mut()
                    && let Ok(de) = fde::DesktopEntry::from_path(file_path.0, Some(&self.locales))
                {
                    self.item_ctr += 1;
                    *dock_item = Some(DockItem {
                        id: self.item_ctr,
                        toplevels: Vec::new(),
                        original_app_id: de.id().to_string(),
                        desktop_info: de,
                    });
                }
            }
            Message::DndDropFinished => {
                if let Some((mut dock_item, index)) = self
                    .dnd_offer
                    .take()
                    .and_then(|o| o.dock_item.map(|i| (i, o.preview_index)))
                {
                    self.item_ctr += 1;

                    if let Some((pos, is_pinned)) = self
                        .active_list
                        .iter()
                        .position(|de| de.original_app_id == dock_item.original_app_id)
                        .map(|pos| (pos, false))
                        .or_else(|| {
                            self.pinned_list
                                .iter()
                                .position(|de| de.original_app_id == dock_item.original_app_id)
                                .map(|pos| (pos, true))
                        })
                    {
                        let t = if is_pinned {
                            let t = self.pinned_list.remove(pos);
                            self.config.remove_pinned(
                                &t.original_app_id,
                                &Config::new(APP_ID, AppListConfig::VERSION).unwrap(),
                            );
                            t
                        } else {
                            self.active_list.remove(pos)
                        };
                        dock_item.toplevels = t.toplevels;
                    }
                    dock_item.id = self.item_ctr;

                    if dock_item.desktop_info.exec().is_some() {
                        self.pinned_list
                            .insert(index.min(self.pinned_list.len()), dock_item);
                        self.config.update_pinned(
                            self.pinned_list
                                .iter()
                                .map(|dock_item| dock_item.original_app_id.clone())
                                .collect(),
                            &Config::new(APP_ID, AppListConfig::VERSION).unwrap(),
                        );
                    }
                }
            }
            Message::Wayland(event) => match *event {
                WaylandUpdate::Init(tx) => {
                    self.wayland_sender.replace(tx);
                }
                WaylandUpdate::Capabilities(capabilities) => {
                    self.capabilities = capabilities;
                }
                WaylandUpdate::Image(handle, img) => {
                    'img_update: for x in self
                        .active_list
                        .iter_mut()
                        .chain(self.pinned_list.iter_mut())
                    {
                        if let Some((_, handle_img)) = x
                            .toplevels
                            .iter_mut()
                            .find(|(info, _)| info.foreign_toplevel == handle)
                        {
                            *handle_img = Some(img);
                            break 'img_update;
                        }
                    }
                }
                WaylandUpdate::Finished => {
                    self.capabilities = ToplevelCapabilities::default();
                    for t in &mut self.pinned_list {
                        t.toplevels.clear();
                    }
                    self.active_list.clear();
                    let subscription_ctr = self.subscription_ctr;
                    let rand_d = fastrand::u64(0..100);
                    return iced::Task::perform(
                        async move {
                            if let Some(millis) = 2u64
                                .checked_pow(subscription_ctr)
                                .and_then(|d| d.checked_add(rand_d))
                            {
                                sleep(Duration::from_millis(millis)).await;
                            } else {
                                pending::<()>().await;
                            }
                        },
                        |()| Message::IncrementSubscriptionCtr,
                    )
                    .map(cosmic::action::app);
                }
                WaylandUpdate::Toplevel(event) => match event {
                    ToplevelUpdate::Add(mut info) => {
                        let unicase_appid = fde::unicase::Ascii::new(&*info.app_id);
                        let new_desktop_info =
                            self.find_desktop_entry_for_toplevel(&info, unicase_appid);

                        if let Some(t) = self
                            .active_list
                            .iter_mut()
                            .chain(self.pinned_list.iter_mut())
                            .find(|DockItem { desktop_info, .. }| {
                                desktop_info.id() == new_desktop_info.id()
                            })
                        {
                            t.toplevels.push((info, None));
                        } else {
                            if info.app_id.is_empty() {
                                info.app_id = format!("Unknown Application {}", self.item_ctr);
                            }
                            self.item_ctr += 1;

                            self.active_list.push(DockItem {
                                id: self.item_ctr,
                                original_app_id: info.app_id.clone(),
                                toplevels: vec![(info, None)],
                                desktop_info: new_desktop_info,
                            });
                        }
                    }
                    ToplevelUpdate::Remove(handle) => {
                        for t in self
                            .active_list
                            .iter_mut()
                            .chain(self.pinned_list.iter_mut())
                        {
                            t.toplevels
                                .retain(|(info, _)| info.foreign_toplevel != handle);
                        }
                        self.active_list.retain(|t| !t.toplevels.is_empty());

                        if let Some(popup) = &mut self.popup
                            && popup.popup_type == PopupType::ToplevelList
                        {
                            popup
                                .dock_item
                                .toplevels
                                .retain(|(info, _)| info.foreign_toplevel != handle);

                            if popup.dock_item.toplevels.is_empty() {
                                let id = popup.id;
                                self.popup = None;
                                return destroy_popup(id);
                            }
                        }
                    }
                    ToplevelUpdate::Update(info) => {
                        if info.app_id.is_empty() {
                            return Task::none();
                        }
                        let mut updated_appid = false;

                        'toplevel_loop: for toplevel_list in self
                            .active_list
                            .iter_mut()
                            .chain(self.pinned_list.iter_mut())
                        {
                            for (t_info, _) in &mut toplevel_list.toplevels {
                                if info.foreign_toplevel == t_info.foreign_toplevel {
                                    if info.app_id != t_info.app_id {
                                        updated_appid = true;
                                    }

                                    *t_info = info.clone();
                                    break 'toplevel_loop;
                                }
                            }
                        }

                        if updated_appid {
                            for t in self
                                .active_list
                                .iter_mut()
                                .chain(self.pinned_list.iter_mut())
                            {
                                t.toplevels.retain(|(t_info, _)| {
                                    t_info.foreign_toplevel != info.foreign_toplevel
                                });
                            }
                            self.active_list.retain(|t| !t.toplevels.is_empty());

                            let new_desktop_entry = self
                                .find_desktop_entry_for_toplevel(&info, Ascii::new(&info.app_id));

                            if let Some(t) = self
                                .active_list
                                .iter_mut()
                                .chain(self.pinned_list.iter_mut())
                                .find(|DockItem { desktop_info, .. }| {
                                    desktop_info.id() == new_desktop_entry.id()
                                })
                            {
                                t.toplevels.push((info, None));
                            } else {
                                self.item_ctr += 1;

                                self.active_list.push(DockItem {
                                    id: self.item_ctr,
                                    original_app_id: info.app_id.clone(),
                                    toplevels: vec![(info, None)],
                                    desktop_info: new_desktop_entry,
                                });
                            }
                        }
                    }
                },
                WaylandUpdate::Workspace(workspaces) => self.active_workspaces = workspaces,
                WaylandUpdate::Output(event) => match event {
                    OutputUpdate::Add(output, info) => {
                        self.output_list.insert(output, info);
                    }
                    OutputUpdate::Update(output, info) => {
                        self.output_list.insert(output, info);
                    }
                    OutputUpdate::Remove(output) => {
                        self.output_list.remove(&output);
                    }
                },
                WaylandUpdate::ActivationToken {
                    token,
                    app_id,
                    exec,
                    gpu_idx,
                    terminal,
                } => {
                    let mut envs = Vec::new();
                    if let Some(token) = token {
                        envs.push(("XDG_ACTIVATION_TOKEN".to_string(), token.clone()));
                        envs.push(("DESKTOP_STARTUP_ID".to_string(), token));
                    }
                    if let (Some(gpus), Some(idx)) = (self.gpus.as_ref(), gpu_idx) {
                        envs.extend(
                            gpus[idx]
                                .environment
                                .iter()
                                .map(|(k, v)| (k.clone(), v.clone())),
                        );
                    }
                    tokio::spawn(async move {
                        cosmic::desktop::spawn_desktop_exec(
                            exec,
                            envs,
                            app_id.as_deref(),
                            terminal,
                        )
                        .await;
                    });
                }
            },
            Message::NewSeat(s) => {
                self.seat.replace(s);
            }
            Message::RemovedSeat => {
                self.seat.take();
            }
            Message::Exec(exec, gpu_idx, terminal) => {
                if let Some(tx) = self.wayland_sender.as_ref() {
                    let _ = tx.send(WaylandRequest::TokenRequest {
                        app_id: Self::APP_ID.to_string(),
                        exec,
                        gpu_idx,
                        terminal,
                    });
                }
            }
            Message::Rectangle(u) => match u {
                RectangleUpdate::Rectangle(r) => {
                    self.rectangles.insert(r.0, r.1);
                }
                RectangleUpdate::Init(tracker) => {
                    self.rectangle_tracker.replace(tracker);
                }
            },
            Message::ClosePopup => {
                if let Some(p) = self.popup.take() {
                    return destroy_popup(p.id);
                }
            }

            Message::IncrementSubscriptionCtr => {
                self.subscription_ctr += 1;
            }
            Message::ConfigUpdated(config) => {
                let favorites_changed = self.config.favorites != config.favorites;
                self.config = config;
                if !favorites_changed {
                    return Task::none();
                }
                for item in self.pinned_list.drain(..) {
                    if !item.toplevels.is_empty() {
                        self.active_list.push(item);
                    }
                }

                self.pinned_list =
                    find_desktop_entries(&self.desktop_entries, &self.config.favorites)
                        .zip(&self.config.favorites)
                        .map(|(de, original_id)| {
                            if let Some(p) = self
                                .active_list
                                .iter()
                                .position(|dock_item| dock_item.desktop_info.id() == de.id())
                            {
                                let mut d = self.active_list.remove(p);
                                d.original_app_id.clone_from(original_id);
                                d
                            } else {
                                self.item_ctr += 1;
                                DockItem {
                                    id: self.item_ctr,
                                    toplevels: Vec::new(),
                                    desktop_info: de.clone(),
                                    original_app_id: original_id.clone(),
                                }
                            }
                        })
                        .collect();
            }
            Message::CloseRequested(id) => {
                if Some(id) == self.popup.as_ref().map(|p| p.id) {
                    self.popup = None;
                }
                if self.overflow_active_popup.is_some_and(|p| p == id) {
                    self.overflow_active_popup = None;
                }
                if self.overflow_favorites_popup.is_some_and(|p| p == id) {
                    self.overflow_favorites_popup = None;
                }
            }
            Message::GpuRequest(gpus) => {
                self.gpus = gpus;
            }
            Message::OpenActive => {
                let create_new = self.overflow_active_popup.is_none();
                let mut cmds = vec![self.close_popups()];

                if create_new {
                    let new_id = window::Id::unique();
                    self.overflow_active_popup = Some(new_id);
                    let Some(iced::Rectangle {
                        x,
                        y,
                        width,
                        height,
                    }) = self.rectangles.get(&DockItemId::ActiveOverflow).copied()
                    else {
                        return Task::none();
                    };

                    let popup_task =
                        cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                            |_| Default::default(),
                            move |app: &mut Self| {
                                let mut popup_settings = app.core.applet.get_popup_settings(
                                    app.core.main_window_id().unwrap(),
                                    new_id,
                                    None,
                                    None,
                                    None,
                                );

                                popup_settings.positioner.anchor_rect = iced::Rectangle::<i32> {
                                    x: x as i32,
                                    y: y as i32,
                                    width: width as i32,
                                    height: height as i32,
                                };

                                let applet_suggested_size = app.core.applet.suggested_size(false).0
                                    + 2 * app.core.applet.suggested_padding(false).0;
                                let (_favorite_popup_cutoff, active_popup_cutoff) =
                                    app.panel_overflow_lengths();
                                let popup_applet_count =
                                    app.visible_active_items().count().saturating_sub(
                                        (active_popup_cutoff.unwrap_or_default()).saturating_sub(1),
                                    ) as f32;
                                let popup_applet_size = applet_suggested_size as f32
                                    * popup_applet_count
                                    + 4.0 * (popup_applet_count - 1.);
                                let (max_width, max_height) = match app.core.applet.anchor {
                                    PanelAnchor::Top | PanelAnchor::Bottom => {
                                        (popup_applet_size, applet_suggested_size as f32)
                                    }
                                    PanelAnchor::Left | PanelAnchor::Right => {
                                        (applet_suggested_size as f32, popup_applet_size)
                                    }
                                };
                                popup_settings.positioner.size_limits = Limits::NONE
                                    .max_width(max_width)
                                    .min_width(1.)
                                    .max_height(max_height)
                                    .min_height(1.);
                                popup_settings
                            },
                            None,
                        ));
                    cmds.push(popup_task);
                }
                return Task::batch(cmds);
            }
            Message::OpenFavorites => {
                let create_new = self.overflow_favorites_popup.is_none();
                let mut cmds = vec![self.close_popups()];

                if create_new {
                    let new_id = window::Id::unique();
                    self.overflow_favorites_popup = Some(new_id);
                    let Some(iced::Rectangle {
                        x,
                        y,
                        width,
                        height,
                    }) = self.rectangles.get(&DockItemId::FavoritesOverflow).copied()
                    else {
                        return Task::none();
                    };

                    let popup_task =
                        cosmic::surface::surface_task(cosmic::surface::action::app_popup(
                            |_| Default::default(),
                            move |app: &mut Self| {
                                let mut popup_settings = app.core.applet.get_popup_settings(
                                    app.core.main_window_id().unwrap(),
                                    new_id,
                                    None,
                                    None,
                                    None,
                                );

                                popup_settings.positioner.anchor_rect = iced::Rectangle::<i32> {
                                    x: x as i32,
                                    y: y as i32,
                                    width: width as i32,
                                    height: height as i32,
                                };

                                let applet_suggested_size = app.core.applet.suggested_size(false).0
                                    + 2 * app.core.applet.suggested_padding(false).0;
                                let (favorite_popup_cutoff, _active_popup_cutoff) =
                                    app.panel_overflow_lengths();
                                let popup_applet_count = app.pinned_list.len().saturating_sub(
                                    favorite_popup_cutoff.unwrap_or_default().saturating_sub(1),
                                ) as f32;
                                let popup_applet_size = applet_suggested_size as f32
                                    * popup_applet_count
                                    + 4.0 * (popup_applet_count - 1.);
                                let (max_width, max_height) = match app.core.applet.anchor {
                                    PanelAnchor::Top | PanelAnchor::Bottom => {
                                        (popup_applet_size, applet_suggested_size as f32)
                                    }
                                    PanelAnchor::Left | PanelAnchor::Right => {
                                        (applet_suggested_size as f32, popup_applet_size)
                                    }
                                };
                                popup_settings.positioner.size_limits = Limits::NONE
                                    .max_width(max_width)
                                    .min_width(1.)
                                    .max_height(max_height)
                                    .min_height(1.);
                                popup_settings
                            },
                            None,
                        ));
                    cmds.push(popup_task);
                }
                return Task::batch(cmds);
            }
            Message::Pressed(id) => {
                if self.popup.is_some() && self.core.main_window_id() == Some(id) {
                    return self.close_popups();
                }
            }
            Message::Surface(a) => {
                return cosmic::task::message(cosmic::Action::Surface(a));
            }
        }

        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let app_icon = AppletIconData::new(&self.core.applet);
        let is_horizontal = match self.core.applet.anchor {
            PanelAnchor::Top | PanelAnchor::Bottom => true,
            PanelAnchor::Left | PanelAnchor::Right => false,
        };
        let divider_padding = match self.core.applet.size {
            Size::Hardcoded(_) => 4,
            Size::PanelSize(ref s) => {
                let size = s.get_applet_icon_size_with_padding(false);

                let small_size_threshold = PanelSize::S.get_applet_icon_size_with_padding(false);

                if size <= small_size_threshold { 4 } else { 8 }
            }
        };
        let (favorite_popup_cutoff, active_popup_cutoff) = self.panel_overflow_lengths();
        let mut favorite_to_remove = if let Some(cutoff) = favorite_popup_cutoff {
            if cutoff < self.pinned_list.len() {
                self.pinned_list.len() - cutoff + 1
            } else {
                0
            }
        } else {
            0
        };
        let favorites: Vec<_> = self
            .pinned_list
            .iter()
            .rev()
            .filter(|f| {
                if favorite_to_remove > 0 && f.toplevels.is_empty() {
                    favorite_to_remove -= 1;
                    false
                } else {
                    true
                }
            })
            .collect();
        let mut favorites: Vec<_> = favorites[favorite_to_remove..]
            .iter()
            .rev()
            .map(|dock_item| {
                self.core
                    .applet
                    .applet_tooltip::<Message>(
                        self.item_icon(
                            dock_item,
                            self.core.main_window_id().unwrap(),
                            self.popup.is_none(),
                            true,
                        ),
                        dock_item
                            .desktop_info
                            .full_name(&self.locales)
                            .unwrap_or_default()
                            .into_owned(),
                        self.popup.is_some(),
                        Message::Surface,
                        None,
                    )
                    .into()
            })
            .collect();

        if favorite_popup_cutoff.is_some() {
            let icon = match self.core.applet.anchor {
                PanelAnchor::Bottom => "go-up-symbolic",
                PanelAnchor::Left => "go-next-symbolic",
                PanelAnchor::Right => "go-previous-symbolic",
                PanelAnchor::Top => "go-down-symbolic",
            };
            let btn = self
                .core
                .applet
                .icon_button(icon)
                .on_press(Message::OpenFavorites);
            let btn: Element<_> = if let Some(rectangle_tracker) = self.rectangle_tracker.as_ref() {
                rectangle_tracker
                    .container(DockItemId::FavoritesOverflow, btn)
                    .into()
            } else {
                btn.into()
            };
            favorites.push(btn);
        }

        if let Some((item, index)) = self
            .dnd_offer
            .as_ref()
            .and_then(|o| o.dock_item.as_ref().map(|item| (item, o.preview_index)))
        {
            favorites.insert(
                index.min(favorites.len()),
                self.item_icon(item, self.core.main_window_id().unwrap(), false, false),
            );
        } else if self.is_listening_for_dnd && self.pinned_list.is_empty() {
            favorites.push(
                container(
                    icon::from_name("starred-symbolic.symbolic")
                        .size(self.core.applet.suggested_size(false).0),
                )
                .padding(self.core.applet.suggested_padding(false).1)
                .into(),
            );
        }

        let filtered_active_list: Vec<_> = self.visible_active_items().collect();
        let show_active = !filtered_active_list.is_empty();

        let mut active: Vec<_> =
            filtered_active_list[..active_popup_cutoff.map_or(filtered_active_list.len(), |n| {
                if n < filtered_active_list.len() {
                    n.saturating_sub(1)
                } else {
                    n
                }
            })]
                .iter()
                .map(|dock_item| {
                    self.core
                        .applet
                        .applet_tooltip(
                            self.item_icon(
                                dock_item,
                                self.core.main_window_id().unwrap(),
                                self.popup.is_none(),
                                true,
                            ),
                            dock_item
                                .desktop_info
                                .full_name(&self.locales)
                                .unwrap_or_default()
                                .into_owned(),
                            self.popup.is_some(),
                            Message::Surface,
                            None,
                        )
                        .into()
                })
                .collect();

        if active_popup_cutoff.is_some_and(|n| n < filtered_active_list.len()) {
            let icon = match self.core.applet.anchor {
                PanelAnchor::Bottom => "go-up-symbolic",
                PanelAnchor::Left => "go-next-symbolic",
                PanelAnchor::Right => "go-previous-symbolic",
                PanelAnchor::Top => "go-down-symbolic",
            };
            let btn = self
                .core
                .applet
                .icon_button(icon)
                .on_press(Message::OpenActive);
            let btn: Element<_> = if let Some(rectangle_tracker) = self.rectangle_tracker.as_ref() {
                rectangle_tracker
                    .container(DockItemId::ActiveOverflow, btn)
                    .into()
            } else {
                btn.into()
            };
            active.push(btn);
        }

        let window_size = self.core.applet.suggested_bounds.as_ref();
        let max_num = if self.core.applet.is_horizontal() {
            let suggested_width = self.core.applet.suggested_size(false).0
                + self.core.applet.suggested_padding(false).0 * 2;
            window_size
                .map(|w| w.width)
                .map_or(u32::MAX, |b| (b / suggested_width as f32) as u32) as usize
        } else {
            let suggested_height = self.core.applet.suggested_size(false).1
                + self.core.applet.suggested_padding(false).0 * 2;
            window_size
                .map(|w| w.height)
                .map_or(u32::MAX, |b| (b / suggested_height as f32) as u32) as usize
        }
        .max(4);
        if max_num < favorites.len() + active.len() {
            let active_leftover = max_num.saturating_sub(favorites.len());
            favorites.truncate(max_num - active_leftover);
            active.truncate(active_leftover);
        }
        let (w, h, favorites, active, divider) = if is_horizontal {
            (
                Length::Shrink,
                Length::Shrink,
                DndDestination::for_data::<DndPathBuf>(
                    row(favorites).spacing(app_icon.icon_spacing),
                    |_, _| Message::DndDropFinished,
                )
                .drag_id(DND_FAVORITES),
                row(active).spacing(app_icon.icon_spacing).into(),
                container(vertical_rule(1))
                    .height(Length::Fill)
                    .padding([divider_padding, 0])
                    .into(),
            )
        } else {
            (
                Length::Shrink,
                Length::Shrink,
                DndDestination::for_data(
                    column(favorites).spacing(app_icon.icon_spacing),
                    |_data: Option<DndPathBuf>, _| Message::DndDropFinished,
                )
                .drag_id(DND_FAVORITES),
                column(active).spacing(app_icon.icon_spacing).into(),
                container(divider::horizontal::default())
                    .width(Length::Fill)
                    .padding([0, divider_padding])
                    .into(),
            )
        };

        let favorites = favorites
            .on_enter(|x, y, _| Message::DndEnter(x, y))
            .on_motion(Message::DndMotion)
            .on_leave(|| Message::DndLeave);

        let show_pinned =
            !self.pinned_list.is_empty() || self.dnd_offer.is_some() || self.is_listening_for_dnd;
        let content_list: Vec<Element<_>> = if show_pinned && show_active {
            vec![favorites.into(), divider, active]
        } else if show_pinned {
            vec![favorites.into()]
        } else if show_active {
            vec![active]
        } else {
            vec![
                icon::from_name("io.github.motixo.AdaptiveDockIcons")
                    .size(self.core.applet.suggested_size(false).0)
                    .into(),
            ]
        };

        let mut content = match &self.core.applet.anchor {
            PanelAnchor::Left | PanelAnchor::Right => container(
                Column::with_children(content_list)
                    .spacing(4.0)
                    .align_x(Alignment::Center)
                    .height(h)
                    .width(w),
            ),
            PanelAnchor::Top | PanelAnchor::Bottom => container(
                Row::with_children(content_list)
                    .spacing(4.0)
                    .align_y(Alignment::Center)
                    .height(h)
                    .width(w),
            ),
        };
        if self.active_list.is_empty() && self.pinned_list.is_empty() {
            let suggested_size = self.core.applet.suggested_size(false);
            content = content.width(suggested_size.0).height(suggested_size.1);
        }

        let mut limits = Limits::NONE.min_width(1.).min_height(1.);

        if let Some(b) = self.core.applet.suggested_bounds {
            if b.width as i32 > 0 {
                limits = limits.max_width(b.width);
            }
            if b.height as i32 > 0 {
                limits = limits.max_height(b.height);
            }
        }

        self.core
            .applet
            .autosize_window(content)
            .limits(limits)
            .into()
    }

    fn view_window(&self, id: window::Id) -> Element<'_, Message> {
        if let Some((_, item, _, _)) = self.dnd_source.as_ref().filter(|s| s.0 == id) {
            cosmic::widget::icon(
                fde::IconSource::from_unknown(item.desktop_info.icon().unwrap_or_default())
                    .as_cosmic_icon(),
            )
            .size(self.core.applet.suggested_size(false).0)
            .into()
        } else if let Some(Popup {
            dock_item: DockItem { id, .. },
            popup_type,
            ..
        }) = self.popup.as_ref().filter(|p| id == p.id)
        {
            let (dock_item, is_pinned) = match self.pinned_list.iter().find(|i| i.id == *id) {
                Some(e) => (e, true),
                None => match self.active_list.iter().find(|i| i.id == *id) {
                    Some(e) => (e, false),
                    None => return text::body("").into(),
                },
            };

            let filtered_toplevels: Vec<_> = dock_item
                .toplevels
                .iter()
                .filter(|(toplevel_info, _)| {
                    self.is_on_current_monitor_and_workspace(toplevel_info)
                })
                .collect();

            let toplevels = &filtered_toplevels;
            let desktop_info = &dock_item.desktop_info;

            match popup_type {
                PopupType::RightClickMenu => {
                    fn menu_button<'a, Message: Clone + 'a>(
                        content: impl Into<Element<'a, Message>>,
                    ) -> cosmic::widget::Button<'a, Message> {
                        button::custom(content)
                            .height(20 + 2 * theme::spacing().space_xxs)
                            .class(Button::MenuItem)
                            .padding(menu_control_padding())
                            .width(Length::Fill)
                    }

                    let mut content =
                        menu::menu_column::MenuColumn::with_capacity(4).align_x(Alignment::Center);

                    if let Some(exec) = desktop_info.exec() {
                        if !toplevels.is_empty() {
                            content =
                                content.push(menu_button(text::body(fl!("new-window"))).on_press(
                                    Message::Exec(exec.to_string(), None, desktop_info.terminal()),
                                ));
                        } else if let Some(gpus) = self.gpus.as_ref() {
                            let default_idx = preferred_gpu_idx(desktop_info, gpus.iter());
                            for (i, gpu) in gpus.iter().enumerate() {
                                content = content.push(
                                    menu_button(text::body(format!(
                                        "{} {}",
                                        fl!("run-on", gpu = gpu.name.clone()),
                                        if i == default_idx {
                                            fl!("run-on-default")
                                        } else {
                                            String::new()
                                        }
                                    )))
                                    .on_press(Message::Exec(
                                        exec.to_string(),
                                        Some(i),
                                        desktop_info.terminal(),
                                    )),
                                );
                            }
                        } else {
                            content = content.push(menu_button(text::body(fl!("run"))).on_press(
                                Message::Exec(exec.to_string(), None, desktop_info.terminal()),
                            ));
                        }
                        for action in desktop_info.actions().into_iter().flatten() {
                            if action == "new-window" {
                                continue;
                            }

                            let Some(exec) = desktop_info.action_entry(action, "Exec") else {
                                continue;
                            };
                            let Some(name) =
                                desktop_info.action_entry_localized(action, "Name", &self.locales)
                            else {
                                continue;
                            };
                            content = content.push(menu_button(text::body(name)).on_press(
                                Message::Exec(exec.into(), None, desktop_info.terminal()),
                            ));
                        }
                        content = content.push(divider::horizontal::light());
                    }

                    if !toplevels.is_empty() {
                        let mut list_col = column![];
                        for (info, _) in toplevels {
                            list_col = list_col.push(
                                menu_button(
                                    text::body(&info.title)
                                        .ellipsize(Ellipsize::End(EllipsizeHeightLimit::Lines(1))),
                                )
                                .on_press_maybe(
                                    self.capabilities
                                        .activate
                                        .then(|| Message::Activate(info.foreign_toplevel.clone())),
                                ),
                            );
                        }
                        content = content.push(list_col);
                        content = content.push(divider::horizontal::light());
                    }

                    let svg_accent = Rc::new(|theme: &cosmic::Theme| {
                        let color = theme.cosmic().accent_color().into();
                        svg::Style { color: Some(color) }
                    });
                    content = content.push(
                        menu_button(
                            if is_pinned {
                                row![
                                    icon::icon(from_name("checkbox-checked-symbolic").into())
                                        .size(16)
                                        .class(cosmic::theme::Svg::Custom(svg_accent.clone())),
                                    text::body(fl!("pin"))
                                ]
                            } else {
                                row![text::body(fl!("pin"))]
                            }
                            .spacing(8),
                        )
                        .on_press(if is_pinned {
                            Message::UnpinApp(*id)
                        } else {
                            Message::PinApp(*id)
                        }),
                    );

                    if !toplevels.is_empty() && self.capabilities.close {
                        content = content.push(divider::horizontal::light());
                        content = match toplevels.len() {
                            1 => content.push(
                                menu_button(text::body(fl!("quit")))
                                    .on_press(Message::Quit(desktop_info.id().to_string())),
                            ),
                            _ => content.push(
                                menu_button(text::body(fl!("quit-all")))
                                    .on_press(Message::Quit(desktop_info.id().to_string())),
                            ),
                        };
                    }
                    self.core
                        .applet
                        .popup_container(
                            container(content)
                                .padding(1)
                                .height(Length::Shrink)
                                .width(Length::Fill),
                        )
                        .limits(
                            Limits::NONE
                                .min_width(1.)
                                .min_height(1.)
                                .max_width(300.)
                                .max_height(1000.),
                        )
                        .into()
                }
                PopupType::ToplevelList => match self.core.applet.anchor {
                    PanelAnchor::Left | PanelAnchor::Right => {
                        let mut content =
                            column![].padding(8).align_x(Alignment::Center).spacing(8);
                        for (info, img) in toplevels {
                            content = content.push(toplevel_button(
                                img.clone(),
                                info.title.clone(),
                                info.foreign_toplevel.clone(),
                                self.is_focused(&info.foreign_toplevel),
                                self.is_hovered(&info.foreign_toplevel),
                                self.capabilities,
                            ));
                        }
                        self.core
                            .applet
                            .popup_container(content)
                            .limits(Limits::NONE.min_width(1.).min_height(1.).max_height(1000.))
                            .into()
                    }
                    PanelAnchor::Bottom | PanelAnchor::Top => {
                        let mut content = row![].padding(8).align_y(Alignment::Center).spacing(8);
                        for (info, img) in toplevels {
                            content = content.push(toplevel_button(
                                img.clone(),
                                info.title.clone(),
                                info.foreign_toplevel.clone(),
                                self.is_focused(&info.foreign_toplevel),
                                self.is_hovered(&info.foreign_toplevel),
                                self.capabilities,
                            ));
                        }
                        self.core
                            .applet
                            .popup_container(content)
                            .limits(Limits::NONE.min_width(1.).min_height(1.).max_height(1000.))
                            .into()
                    }
                },
            }
        } else if self
            .overflow_active_popup
            .as_ref()
            .is_some_and(|overflow_id| overflow_id == &id)
        {
            let (_favorite_popup_cutoff, active_popup_cutoff) = self.panel_overflow_lengths();

            let filtered_active_list: Vec<_> = self.visible_active_items().collect();

            let active: Vec<_> = filtered_active_list
                .iter()
                .rev()
                .take(active_popup_cutoff.map_or(filtered_active_list.len(), |n| {
                    if n < filtered_active_list.len() {
                        filtered_active_list.len() - n + 1
                    } else {
                        0
                    }
                }))
                .map(|dock_item| {
                    self.core
                        .applet
                        .applet_tooltip(
                            self.item_icon(dock_item, id, self.popup.is_none(), true),
                            dock_item
                                .desktop_info
                                .full_name(&self.locales)
                                .unwrap_or_default()
                                .into_owned(),
                            self.popup.is_some(),
                            Message::Surface,
                            Some(id),
                        )
                        .into()
                })
                .collect();
            let content = match &self.core.applet.anchor {
                PanelAnchor::Left | PanelAnchor::Right => container(
                    Column::with_children(active)
                        .spacing(4.0)
                        .align_x(Alignment::Center)
                        .width(Length::Shrink)
                        .height(Length::Shrink),
                ),
                PanelAnchor::Top | PanelAnchor::Bottom => container(
                    Row::with_children(active)
                        .spacing(4.0)
                        .align_y(Alignment::Center)
                        .width(Length::Shrink)
                        .height(Length::Shrink),
                ),
            };
            let content: Element<_> = if self.popup.is_some() {
                mouse_area(content)
                    .on_release(Message::ClosePopup)
                    .on_right_release(Message::ClosePopup)
                    .into()
            } else {
                content.into()
            };
            self.core
                .applet
                .popup_container(content)
                .limits(
                    Limits::NONE
                        .min_width(1.)
                        .min_height(1.)
                        .max_width(1920.)
                        .max_height(1000.),
                )
                .into()
        } else if self
            .overflow_favorites_popup
            .as_ref()
            .is_some_and(|popup_id| popup_id == &id)
        {
            let (favorite_popup_cutoff, _active_popup_cutoff) = self.panel_overflow_lengths();

            let mut favorite_to_remove = if let Some(cutoff) = favorite_popup_cutoff {
                if cutoff < self.pinned_list.len() {
                    self.pinned_list.len() - cutoff + 1
                } else {
                    0
                }
            } else {
                0
            };
            let mut favorites_extra = Vec::with_capacity(favorite_to_remove);
            let mut favorites: Vec<_> = self
                .pinned_list
                .iter()
                .rev()
                .filter(|f| {
                    if favorite_to_remove > 0 && f.toplevels.is_empty() {
                        favorite_to_remove -= 1;
                        true
                    } else {
                        favorites_extra.push(*f);
                        false
                    }
                })
                .collect();
            favorites.extend(favorites_extra[..favorite_to_remove].iter().copied());
            let favorites: Vec<_> = favorites
                .iter()
                .rev()
                .map(|dock_item| {
                    self.core
                        .applet
                        .applet_tooltip(
                            self.item_icon(dock_item, id, self.popup.is_none(), true),
                            dock_item
                                .desktop_info
                                .full_name(&self.locales)
                                .unwrap_or_default()
                                .to_string(),
                            self.popup.is_some(),
                            Message::Surface,
                            Some(id),
                        )
                        .into()
                })
                .collect();
            let content = match &self.core.applet.anchor {
                PanelAnchor::Left | PanelAnchor::Right => container(
                    Column::with_children(favorites)
                        .spacing(4.0)
                        .align_x(Alignment::Center)
                        .width(Length::Shrink)
                        .height(Length::Shrink),
                ),
                PanelAnchor::Top | PanelAnchor::Bottom => container(
                    Row::with_children(favorites)
                        .spacing(4.0)
                        .align_y(Alignment::Center)
                        .width(Length::Shrink)
                        .height(Length::Shrink),
                ),
            };
            let content: Element<_> = if self.popup.is_some() {
                mouse_area(content)
                    .on_right_release(Message::ClosePopup)
                    .on_press(Message::ClosePopup)
                    .into()
            } else {
                content.into()
            };
            self.core
                .applet
                .popup_container(content)
                .limits(
                    Limits::NONE
                        .min_width(1.)
                        .min_height(1.)
                        .max_width(1920.)
                        .max_height(1000.),
                )
                .into()
        } else {
            let suggested = self.core.applet.suggested_size(false);
            iced::widget::row!()
                .width(Length::Fixed(suggested.0 as f32))
                .height(Length::Fixed(suggested.1 as f32))
                .into()
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let theme = cosmic::icon_theme::default();
        let missing = self
            .icon_names()
            .into_iter()
            .filter(|name| self.icon_theme != theme || !self.icons.contains_key(name))
            .collect();
        Subscription::batch([
            icons::subscription(theme, missing).map(Message::IconsReady),
            wayland_subscription().map(|event| Message::Wayland(Box::new(event))),
            listen_with(|e, _, id| match e {
                cosmic::iced::core::Event::PlatformSpecific(event::PlatformSpecific::Wayland(
                    event::wayland::Event::Seat(e, seat),
                )) => match e {
                    event::wayland::SeatEvent::Enter => Some(Message::NewSeat(seat)),
                    event::wayland::SeatEvent::Leave => Some(Message::RemovedSeat),
                },
                cosmic::iced::core::Event::Mouse(
                    cosmic::iced::core::mouse::Event::ButtonPressed(_),
                ) => Some(Message::Pressed(id)),
                _ => None,
            }),
            rectangle_tracker_subscription(0).map(|update| Message::Rectangle(update.1)),
            self.core.watch_config(APP_ID).map(|u| {
                for why in u.errors {
                    tracing::error!(why = why.to_string(), "Error watching config");
                }
                Message::ConfigUpdated(u.config)
            }),
        ])
    }

    fn style(&self) -> Option<iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }
}

fn launch_on_preferred_gpu(desktop_info: &DesktopEntry, gpus: Option<&[Gpu]>) -> Option<Message> {
    let exec = desktop_info.exec()?;

    let gpu_idx = gpus.map(|gpus| preferred_gpu_idx(desktop_info, gpus.iter()));

    Some(Message::Exec(
        exec.to_string(),
        gpu_idx,
        desktop_info.terminal(),
    ))
}

fn preferred_gpu_idx<'a, I>(desktop_info: &DesktopEntry, mut gpus: I) -> usize
where
    I: Iterator<Item = &'a Gpu>,
{
    gpus.position(|gpu| gpu.default ^ desktop_info.prefers_non_default_gpu())
        .unwrap_or(0)
}

#[derive(Debug, Default, Clone)]
pub struct DndPathBuf(PathBuf);

impl AllowedMimeTypes for DndPathBuf {
    fn allowed() -> std::borrow::Cow<'static, [String]> {
        std::borrow::Cow::Owned(vec![MIME_TYPE.to_string()])
    }
}

impl TryFrom<(Vec<u8>, String)> for DndPathBuf {
    type Error = anyhow::Error;

    fn try_from((data, mime_type): (Vec<u8>, String)) -> Result<Self, Self::Error> {
        anyhow::ensure!(mime_type == MIME_TYPE, "Invalid MIME type");
        let data = std::str::from_utf8(&data)?;
        let mut uris = data
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'));
        let uri = uris
            .next()
            .ok_or_else(|| anyhow::anyhow!("Empty URI list"))?;
        anyhow::ensure!(uris.next().is_none(), "Expected one desktop entry");
        let path = Url::parse(uri)?
            .to_file_path()
            .map_err(|_| anyhow::anyhow!("Expected a local file URI"))?;
        Ok(DndPathBuf(path))
    }
}

impl AsMimeTypes for DndPathBuf {
    fn available(&self) -> std::borrow::Cow<'static, [String]> {
        std::borrow::Cow::Owned(vec![MIME_TYPE.to_string()])
    }

    fn as_bytes(&self, mime_type: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
        if mime_type != MIME_TYPE {
            return None;
        }
        Some(Cow::Owned(
            format!("{}\r\n", Url::from_file_path(&self.0).ok()?).into_bytes(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cctk::wayland_client::{Connection, Dispatch, QueueHandle, protocol::wl_registry};
    use cosmic::Application;
    use std::os::unix::net::UnixStream;

    struct TestClient;

    impl Dispatch<wl_registry::WlRegistry, ()> for TestClient {
        fn event(
            _: &mut Self,
            _: &wl_registry::WlRegistry,
            _: wl_registry::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtForeignToplevelHandleV1, ()> for TestClient {
        fn event(
            _: &mut Self,
            _: &ExtForeignToplevelHandleV1,
            _: cctk::wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ExtWorkspaceHandleV1, ()> for TestClient {
        fn event(
            _: &mut Self,
            _: &ExtWorkspaceHandleV1,
            _: cctk::wayland_protocols::ext::workspace::v1::client::ext_workspace_handle_v1::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    fn info(handle: ExtForeignToplevelHandleV1, app_id: &str) -> ToplevelInfo {
        ToplevelInfo {
            title: app_id.into(),
            app_id: app_id.into(),
            identifier: String::new(),
            state: Default::default(),
            output: Default::default(),
            geometry: Default::default(),
            workspace: Default::default(),
            cosmic_toplevel: None,
            foreign_toplevel: handle,
        }
    }

    fn item(id: u32, info: ToplevelInfo) -> DockItem {
        DockItem {
            id,
            desktop_info: DesktopEntry::from_appid(info.app_id.clone()),
            original_app_id: info.app_id.clone(),
            toplevels: vec![(info, None)],
        }
    }

    #[test]
    fn resizing_icons_keeps_actual_button_layout_in_every_dock_orientation() {
        use iced::advanced::widget::Tree;

        let renderer = cosmic::Renderer::new(iced::Font::default(), iced::Pixels(16.0));
        let window = window::Id::unique();
        let item = DockItem {
            id: 1,
            desktop_info: DesktopEntry::from_appid("test-icon".into()),
            original_app_id: "test-icon".into(),
            toplevels: Vec::new(),
        };
        for anchor in [
            PanelAnchor::Top,
            PanelAnchor::Bottom,
            PanelAnchor::Left,
            PanelAnchor::Right,
        ] {
            for size in [
                Size::PanelSize(PanelSize::XS),
                Size::PanelSize(PanelSize::S),
                Size::PanelSize(PanelSize::M),
                Size::PanelSize(PanelSize::L),
                Size::PanelSize(PanelSize::XL),
                Size::PanelSize(PanelSize::Custom(16)),
                Size::PanelSize(PanelSize::Custom(80)),
                Size::Hardcoded((40, 40)),
            ] {
                let mut app = CosmicAppList::default();
                app.core.applet.anchor = anchor;
                app.core.applet.size = size;
                let bounds = |app: &CosmicAppList| {
                    let mut element = app.item_icon(&item, window, false, false);
                    let mut tree = Tree::new(element.as_widget());
                    element
                        .as_widget_mut()
                        .layout(&mut tree, &renderer, &Limits::NONE)
                        .bounds()
                };
                let original = bounds(&app);
                let default_size = AppletIconData::new(&app.core.applet).icon_size;
                for requested in [1, 16, 24, 32, 40, 64, u16::MAX, 0] {
                    app.config.icon_size = requested;
                    assert_eq!(bounds(&app), original, "{anchor:?}, {requested}");
                    let icon = AppletIconData::new(&app.core.applet).with_icon_size(requested);
                    if requested == 16 {
                        assert!(icon.icon_size <= 16);
                    } else if requested == u16::MAX {
                        assert!(icon.icon_size >= default_size);
                    } else if requested == 0 {
                        assert_eq!(icon.icon_size, default_size);
                    }
                }
            }
        }
    }

    #[test]
    fn changing_appearance_live_preserves_pinned_item_identity() {
        use cosmic::Application;

        let mut app = CosmicAppList::default();
        app.config.favorites.push("test-icon".into());
        app.pinned_list.push(DockItem {
            id: 42,
            desktop_info: DesktopEntry::from_appid("test-icon".into()),
            original_app_id: "test-icon".into(),
            toplevels: Vec::new(),
        });
        let mut config = app.config.clone();
        config.icon_size = 24;
        config.logo_scale = 125;
        let _ = app.update(Message::ConfigUpdated(config));
        assert_eq!(app.config.icon_size, 24);
        assert_eq!(app.config.logo_scale, 125);
        assert_eq!(app.pinned_list[0].id, 42);
    }

    #[test]
    fn overflow_uses_visible_windows_and_renders_safely() {
        let (client, _server) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let queue = conn.new_event_queue::<TestClient>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let visible: ExtWorkspaceHandleV1 = registry.bind(1, 1, &qh, ());
        let hidden: ExtWorkspaceHandleV1 = registry.bind(2, 1, &qh, ());
        for visible_count in [0, 1, 8] {
            let mut app = CosmicAppList::default();
            app.core.set_main_window_id(Some(window::Id::unique()));
            app.core.applet.suggested_bounds = Some(iced::Size::new(128.0, 64.0));
            app.config.filter_top_levels = Some(crate::config::ToplevelFilter::ActiveWorkspace);
            app.active_workspaces.push(visible.clone());
            for n in 0..8 + visible_count {
                let handle: ExtForeignToplevelHandleV1 = registry.bind(10 + n, 1, &qh, ());
                let mut info = info(handle, &format!("window-{n}"));
                info.workspace.insert(if n < visible_count {
                    visible.clone()
                } else {
                    hidden.clone()
                });
                app.active_list.push(item(n, info));
            }
            assert_eq!(app.visible_active_items().count(), visible_count as usize);
            let cutoff = app.panel_overflow_lengths().1;
            assert!(cutoff.is_none_or(|n| n <= visible_count as usize));
            if visible_count <= 1 {
                assert!(cutoff.is_none());
            } else {
                assert!(cutoff.is_some());
            }
            let _ = app.view();
        }
    }

    #[test]
    fn changing_one_window_app_id_preserves_its_peers() {
        let (client, _server) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let queue = conn.new_event_queue::<TestClient>();
        let qh = queue.handle();
        let registry = conn.display().get_registry(&qh, ());
        let first: ExtForeignToplevelHandleV1 = registry.bind(1, 1, &qh, ());
        let other: ExtForeignToplevelHandleV1 = registry.bind(2, 1, &qh, ());
        let mut app = CosmicAppList {
            desktop_entries: vec![DesktopEntry::from_appid("new".into())],
            active_list: vec![
                item(1, info(first.clone(), "old")),
                item(2, info(other.clone(), "new")),
            ],
            item_ctr: 2,
            ..Default::default()
        };
        let _ = app.update(Message::Wayland(Box::new(WaylandUpdate::Toplevel(
            ToplevelUpdate::Update(info(first.clone(), "new")),
        ))));
        assert_eq!(app.active_list.len(), 1);
        let handles: Vec<_> = app.active_list[0]
            .toplevels
            .iter()
            .map(|(info, _)| &info.foreign_toplevel)
            .collect();
        assert_eq!(handles.len(), 2);
        assert!(handles.contains(&&first));
        assert!(handles.contains(&&other));
    }

    #[test]
    fn drag_uris_round_trip_and_reject_invalid_payloads() {
        let original = DndPathBuf(PathBuf::from("/tmp/برنامه #1.desktop"));
        let payload = original.as_bytes(MIME_TYPE).unwrap().into_owned();
        assert!(payload.starts_with(b"file:///"));
        assert_eq!(
            DndPathBuf::try_from((payload, MIME_TYPE.into())).unwrap().0,
            original.0
        );
        let commented = b"# desktop entry\r\n\r\nfile:///tmp/Test%20App.desktop\r\n".to_vec();
        assert_eq!(
            DndPathBuf::try_from((commented, MIME_TYPE.into()))
                .unwrap()
                .0,
            PathBuf::from("/tmp/Test App.desktop")
        );
        for payload in [
            "",
            "/tmp/app.desktop",
            "https://example.com/app.desktop",
            "file:///tmp/a.desktop\r\nfile:///tmp/b.desktop",
        ] {
            assert!(DndPathBuf::try_from((payload.as_bytes().to_vec(), MIME_TYPE.into())).is_err());
        }
        assert!(original.as_bytes("text/plain").is_none());
        assert!(
            DndPathBuf::try_from((b"file:///tmp/app.desktop".to_vec(), "text/plain".into()))
                .is_err()
        );
    }

    #[test]
    fn compositor_capabilities_gate_window_actions() {
        use cctk::wayland_client::WEnum;
        use cosmic_protocols::toplevel_management::v1::client::zcosmic_toplevel_manager_v1::ZcosmicToplelevelManagementCapabilitiesV1 as Capability;

        let (client, _server) = UnixStream::pair().unwrap();
        let conn = Connection::from_socket(client).unwrap();
        let queue = conn.new_event_queue::<TestClient>();
        let registry = conn.display().get_registry(&queue.handle(), ());
        let handle: ExtForeignToplevelHandleV1 = registry.bind(1, 1, &queue.handle(), ());
        let limited = ToplevelCapabilities::from(vec![
            WEnum::Value(Capability::Activate),
            WEnum::Unknown(u32::MAX),
        ]);
        assert!(limited.supports(&ToplevelRequest::Activate(handle.clone())));
        assert!(!limited.supports(&ToplevelRequest::Minimize(handle.clone())));
        assert!(!limited.supports(&ToplevelRequest::Quit(handle.clone())));
        assert!(limited.can_toggle(false));
        assert!(!limited.can_toggle(true));
        let mut app = CosmicAppList::default();
        let _ = app.update(Message::Wayland(Box::new(WaylandUpdate::Capabilities(
            limited,
        ))));
        assert!(app.capabilities.activate);
        assert!(!app.capabilities.close);
        let all = ToplevelCapabilities::from(vec![
            WEnum::Value(Capability::Activate),
            WEnum::Value(Capability::Minimize),
            WEnum::Value(Capability::Close),
        ]);
        assert!(all.supports(&ToplevelRequest::Minimize(handle.clone())));
        assert!(all.supports(&ToplevelRequest::Quit(handle)));
        assert!(all.can_toggle(true));
    }
}
