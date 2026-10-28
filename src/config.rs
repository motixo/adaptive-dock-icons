// Copyright 2023 System76 <info@system76.com>
// SPDX-License-Identifier: GPL-3.0-only

use cosmic::cosmic_config::{
    self, Config, CosmicConfigEntry, cosmic_config_derive::CosmicConfigEntry,
};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
pub const APP_ID: &str = "io.github.motixo.AdaptiveDockIcons";

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
}

impl Default for AppListConfig {
    fn default() -> Self {
        Self {
            filter_top_levels: None,
            favorites: Vec::new(),
            enable_drag_source: true,
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
        let seed = AppListConfig::get_entry(original).unwrap_or_else(|(_, value)| value);
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
}
