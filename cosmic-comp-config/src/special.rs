// SPDX-License-Identifier: GPL-3.0-only

//! Configuration for special workspaces (the scratchpad-style second
//! visibility domain).
//!
//! Two kinds coexist and are independent:
//!
//! - **Numbered** specials (`special 1..12`, bound to logo+F1..F12 by the
//!   internal defaults). Their numbering is fixed and cannot be renamed or
//!   reordered; only a shared default geometry can be configured.
//! - **Named** specials. Each entry in `special.named` declares its own
//!   geometry and `on_demand` flag. Names get their own slot IDs (offset
//!   past the numbered range) so toggling a named special never disturbs a
//!   numbered one.
//!
//! The whole section is optional: an absent `[special]` table deserializes
//! to [`SpecialConfig::default`], which reproduces the built-in behavior
//! exactly (no geometry overrides, no named specials).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Number of fixed numbered special slots (F1..F12). Named specials are
/// assigned slot IDs at or after this offset.
pub const SPECIAL_NUMBERED_SLOTS: usize = 12;

/// Where a special workspace is placed on the output when shown.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecialAnchor {
    #[default]
    Center,
    Top,
    Bottom,
    Left,
    Right,
}

/// Geometry shared by the numbered specials. `None` sizes keep the
/// compositor's automatic floating placement.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NumberedSpecialsConfig {
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub anchor: SpecialAnchor,
}

/// One named special workspace.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamedSpecialConfig {
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub anchor: SpecialAnchor,
    /// Reserved for a future quake-style toggle: when true, showing an
    /// empty named special may spawn an associated command. Not acted on
    /// by the compositor yet.
    #[serde(default)]
    pub on_demand: bool,
}

/// The `[special]` section. Every field defaults, so a config without this
/// section behaves exactly like a compositor built without it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecialConfig {
    /// Default geometry for the numbered specials (`special 1..12`).
    #[serde(default)]
    pub numbered: NumberedSpecialsConfig,
    /// Named specials, keyed by name. Sorted map so serialization is
    /// deterministic.
    #[serde(default)]
    pub named: BTreeMap<String, NamedSpecialConfig>,
}

impl SpecialConfig {
    /// True when the config changes nothing relative to the built-in
    /// behavior (no geometry overrides, no named specials).
    pub fn is_noop(&self) -> bool {
        *self == Self::default()
    }

    /// Slot ID for `special N` (1-based, as bound to F1..F12).
    pub fn numbered_slot(n: u8) -> usize {
        (n as usize)
            .saturating_sub(1)
            .min(SPECIAL_NUMBERED_SLOTS - 1)
    }

    /// The slot ID a named special resolves to: offset past the numbered
    /// range, in sorted-name order so the assignment is stable across
    /// reloads regardless of map iteration order.
    pub fn slot_for_name(&self, name: &str) -> Option<usize> {
        self.named
            .keys()
            .position(|n| n == name)
            .map(|idx| SPECIAL_NUMBERED_SLOTS + idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    /// The section as it appears in a full config document: nested under
    /// the root `special` key.
    #[derive(Deserialize)]
    struct Wrapper {
        #[serde(default)]
        special: SpecialConfig,
    }

    fn parse(doc: &str) -> SpecialConfig {
        toml::from_str::<Wrapper>(doc).unwrap().special
    }

    #[test]
    fn missing_section_parses_to_default() {
        let config = parse("");
        assert_eq!(config, SpecialConfig::default());
        assert!(config.is_noop());
    }

    #[test]
    fn empty_table_parses_to_default() {
        let config = parse("[special]");
        assert_eq!(config, SpecialConfig::default());
    }

    #[test]
    fn default_matches_builtin_behavior() {
        let config = SpecialConfig::default();
        assert_eq!(config.numbered, NumberedSpecialsConfig::default());
        assert_eq!(config.numbered.anchor, SpecialAnchor::Center);
        assert_eq!(config.numbered.width, None);
        assert!(config.named.is_empty());
    }

    #[test]
    fn full_section_parses() {
        let config = parse(
            r#"
            [special.numbered]
            width = 800
            height = 600
            anchor = "top"

            [special.named.terminal]
            width = 1000
            height = 400
            anchor = "top"
            on_demand = true

            [special.named.notes]
            anchor = "right"
            on_demand = false
            "#,
        );
        assert_eq!(config.numbered.width, Some(800));
        assert_eq!(config.numbered.height, Some(600));
        assert_eq!(config.numbered.anchor, SpecialAnchor::Top);

        let terminal = &config.named["terminal"];
        assert_eq!(terminal.width, Some(1000));
        assert_eq!(terminal.height, Some(400));
        assert_eq!(terminal.anchor, SpecialAnchor::Top);
        assert!(terminal.on_demand);

        let notes = &config.named["notes"];
        // Unset optional fields fall back to defaults.
        assert_eq!(notes.width, None);
        assert_eq!(notes.height, None);
        assert_eq!(notes.anchor, SpecialAnchor::Right);
        assert!(!notes.on_demand);
    }

    #[test]
    fn serialize_roundtrip() {
        let mut named = BTreeMap::new();
        named.insert(
            "terminal".to_string(),
            NamedSpecialConfig {
                width: Some(1000),
                height: Some(400),
                anchor: SpecialAnchor::Top,
                on_demand: true,
            },
        );
        let config = SpecialConfig {
            numbered: NumberedSpecialsConfig {
                width: Some(800),
                height: None,
                anchor: SpecialAnchor::Center,
            },
            named,
        };
        let serialized = toml::to_string(&config).unwrap();
        let parsed: SpecialConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(parsed, config);
    }

    #[test]
    fn named_specials_get_slots_past_numbered_range() {
        let mut config = SpecialConfig::default();
        config.named.insert("zeta".to_string(), Default::default());
        config.named.insert("alpha".to_string(), Default::default());
        config.named.insert("mid".to_string(), Default::default());
        // Sorted order (alpha, mid, zeta) determines slot stability.
        assert_eq!(config.slot_for_name("alpha"), Some(SPECIAL_NUMBERED_SLOTS));
        assert_eq!(
            config.slot_for_name("mid"),
            Some(SPECIAL_NUMBERED_SLOTS + 1)
        );
        assert_eq!(
            config.slot_for_name("zeta"),
            Some(SPECIAL_NUMBERED_SLOTS + 2)
        );
        assert_eq!(config.slot_for_name("missing"), None);
    }

    #[test]
    fn numbered_slots_are_fixed() {
        assert_eq!(SpecialConfig::numbered_slot(1), 0);
        assert_eq!(SpecialConfig::numbered_slot(12), 11);
        // Out-of-range input clamps into the numbered range, never spills
        // into the named range.
        assert_eq!(SpecialConfig::numbered_slot(13), 11);
    }
}
