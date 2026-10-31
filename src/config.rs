// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic::cosmic_config::{
    self, Config, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry,
};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
pub const APP_ID: &str = "io.github.motixo.AdaptiveDockIcons";

const fn default_logo_scale() -> u16 {
    100
}

#[derive(Debug, Clone, Deserialize, Serialize, Default, PartialEq, Eq)]
pub enum ToplevelFilter {
    #[default]
    ActiveWorkspace,
    ConfiguredOutput,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, CosmicConfigEntry)]
#[version = 1]
pub struct AppListConfig {
    pub filter_top_levels: Option<ToplevelFilter>,
    pub favorites: Vec<String>,
    pub enable_drag_source: bool,
    /// Icon edge in logical pixels; zero follows the dock's suggested size.
    #[serde(default)]
    pub icon_size: u16,
    /// Logo scale inside added frames, as a percentage of its default size.
    #[serde(default = "default_logo_scale")]
    pub logo_scale: u16,
}

impl Default for AppListConfig {
    fn default() -> Self {
        Self {
            filter_top_levels: None,
            favorites: Vec::new(),
            enable_drag_source: true,
            icon_size: 0,
            logo_scale: default_logo_scale(),
        }
    }
}

impl AppListConfig {
    pub fn add_pinned(&mut self, id: String, config: &Config) {
        if !self.favorites.contains(&id) {
            self.favorites.push(id);
            let _ = self.write_entry(config);
        }
    }

    pub fn remove_pinned(&mut self, id: &str, config: &Config) {
        if let Some(pos) = self.favorites.iter().position(|e| e == id) {
            self.favorites.remove(pos);
            let _ = self.write_entry(config);
        }
    }

    pub fn update_pinned(&mut self, favorites: Vec<String>, config: &Config) {
        self.favorites = favorites;
        let _ = self.write_entry(config);
    }
}

pub const ORIGINAL_APP_ID: &str = "com.system76.CosmicAppList";

/// Seed a fresh profile once, then keep pin changes isolated from the stock applet.
pub fn load(config: &Config, original: Option<&Config>) -> AppListConfig {
    use cosmic::cosmic_config::ConfigGet;
    let fresh = ["favorites", "filter_top_levels", "enable_drag_source"]
        .iter()
        .all(|key| {
            matches!(
                config.get_local::<serde::de::IgnoredAny>(key),
                Err(cosmic_config::Error::NotFound)
            )
        });
    if fresh && let Some(original) = original {
        let mut seed = AppListConfig::get_entry(original).unwrap_or_else(|(_, value)| value);
        // Appearance configured before the first launch belongs to this applet.
        seed.icon_size = config.get("icon_size").unwrap_or_default();
        seed.logo_scale = config
            .get("logo_scale")
            .unwrap_or_else(|_| default_logo_scale());
        if let Err(error) = seed.write_entry(config) {
            tracing::error!(%error, "Could not save the initial Adaptive Dock Icons profile");
        }
        return seed;
    }
    AppListConfig::get_entry(config).unwrap_or_else(|(errors, value)| {
        for error in errors.into_iter().filter(cosmic_config::Error::is_err) {
            tracing::error!(%error, "Could not read Adaptive Dock Icons configuration");
        }
        value
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_pins_are_copied_once_without_modifying_the_original() {
        let directory = tempfile::tempdir().unwrap();
        let original =
            Config::with_custom_path(ORIGINAL_APP_ID, 1, directory.path().to_owned()).unwrap();
        let own = Config::with_custom_path(APP_ID, 1, directory.path().to_owned()).unwrap();
        let stock = AppListConfig {
            favorites: vec!["firefox".into(), "com.system76.CosmicFiles".into()],
            ..Default::default()
        };
        stock.write_entry(&original).unwrap();
        let mut migrated = load(&own, Some(&original));
        assert_eq!(migrated, stock);
        migrated.favorites.clear();
        migrated.write_entry(&own).unwrap();
        assert!(load(&own, Some(&original)).favorites.is_empty());
        assert_eq!(AppListConfig::get_entry(&original).unwrap(), stock);
    }

    #[test]
    fn invalid_profile_is_preserved_instead_of_being_reseeded() {
        use cosmic::cosmic_config::ConfigGet;
        let directory = tempfile::tempdir().unwrap();
        let own = Config::with_custom_path(APP_ID, 1, directory.path().to_owned()).unwrap();
        let invalid = directory
            .path()
            .join("cosmic")
            .join(APP_ID)
            .join("v1/favorites");
        std::fs::write(&invalid, "malformed").unwrap();
        let _ = load(&own, None);
        assert!(own.get::<Vec<String>>("favorites").is_err());
        assert_eq!(std::fs::read_to_string(invalid).unwrap(), "malformed");
    }

    #[test]
    fn first_launch_preserves_preconfigured_appearance_while_seeding_pins() {
        use cosmic::cosmic_config::ConfigSet;

        let directory = tempfile::tempdir().unwrap();
        let original =
            Config::with_custom_path(ORIGINAL_APP_ID, 1, directory.path().to_owned()).unwrap();
        let own = Config::with_custom_path(APP_ID, 1, directory.path().to_owned()).unwrap();
        let stock = AppListConfig {
            favorites: vec!["firefox".into()],
            ..Default::default()
        };
        stock.write_entry(&original).unwrap();
        own.set("icon_size", 24_u16).unwrap();
        own.set("logo_scale", 125_u16).unwrap();
        let migrated = load(&own, Some(&original));
        assert_eq!(migrated.icon_size, 24);
        assert_eq!(migrated.logo_scale, 125);
        assert_eq!(migrated.favorites, stock.favorites);
        assert_eq!(AppListConfig::get_entry(&own).unwrap(), migrated);
    }

    #[test]
    fn existing_profiles_default_safely_and_read_icon_size_changes() {
        use cosmic::cosmic_config::ConfigSet;

        let directory = tempfile::tempdir().unwrap();
        let config = Config::with_custom_path(APP_ID, 1, directory.path().to_owned()).unwrap();
        config.set("favorites", vec!["firefox"]).unwrap();
        assert_eq!(load(&config, None).icon_size, 0);
        assert_eq!(load(&config, None).logo_scale, 100);
        for size in [24_u16, 40, 0] {
            config.set("icon_size", size).unwrap();
            assert_eq!(load(&config, None).icon_size, size);
        }
        std::fs::write(
            directory
                .path()
                .join("cosmic")
                .join(APP_ID)
                .join("v1/icon_size"),
            "invalid",
        )
        .unwrap();
        let fallback = load(&config, None);
        assert_eq!(fallback.icon_size, 0);
        assert_eq!(fallback.favorites, vec!["firefox"]);
    }

    #[test]
    fn logo_scale_loads_changes_and_falls_back_without_losing_pins() {
        use cosmic::cosmic_config::ConfigSet;

        let directory = tempfile::tempdir().unwrap();
        let config = Config::with_custom_path(APP_ID, 1, directory.path().to_owned()).unwrap();
        config.set("favorites", vec!["firefox"]).unwrap();
        for scale in [80_u16, 100, 125, u16::MAX] {
            config.set("logo_scale", scale).unwrap();
            assert_eq!(load(&config, None).logo_scale, scale);
        }
        let path = directory
            .path()
            .join("cosmic")
            .join(APP_ID)
            .join("v1/logo_scale");
        for invalid in ["invalid", "-1", "65536"] {
            std::fs::write(&path, invalid).unwrap();
            let fallback = load(&config, None);
            assert_eq!(fallback.logo_scale, 100);
            assert_eq!(fallback.favorites, vec!["firefox"]);
        }
    }
}
