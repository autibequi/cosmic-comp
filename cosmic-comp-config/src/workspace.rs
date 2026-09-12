// SPDX-License-Identifier: GPL-3.0-only

use serde::{Deserialize, Serialize};

use crate::EdidProduct;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    pub workspace_mode: WorkspaceMode,
    #[serde(default)]
    pub workspace_layout: WorkspaceLayout,
    #[serde(default)]
    pub action_on_typing: Action,
    #[serde(default = "default_wraparound")]
    pub workspace_wraparound: bool,
}

fn default_wraparound() -> bool {
    true
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            workspace_mode: WorkspaceMode::default(),
            workspace_layout: WorkspaceLayout::default(),
            action_on_typing: Action::default(),
            workspace_wraparound: default_wraparound(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceMode {
    #[default]
    OutputBound,
    Global,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceLayout {
    #[default]
    Vertical,
    Horizontal,
    /// Workspaces are arranged as an horizontal strip of columns; switching
    /// workspaces pans the viewport to the column at index * output width.
    Scrolling,
}

#[cfg(test)]
mod tests {
    use super::WorkspaceLayout;
    use crate::workspace::WorkspaceConfig;

    #[test]
    fn layout_deserializes_new_variant() {
        let layout: WorkspaceLayout = serde_json::from_str(r#""Scrolling""#).unwrap();
        assert_eq!(layout, WorkspaceLayout::Scrolling);
    }

    #[test]
    fn layout_deserializes_legacy_variants() {
        assert_eq!(
            serde_json::from_str::<WorkspaceLayout>(r#""Vertical""#).unwrap(),
            WorkspaceLayout::Vertical
        );
        assert_eq!(
            serde_json::from_str::<WorkspaceLayout>(r#""Horizontal""#).unwrap(),
            WorkspaceLayout::Horizontal
        );
    }

    #[test]
    fn config_without_layout_field_deserializes_with_default() {
        let json = r#"{
            "workspace_mode": "OutputBound",
            "action_on_typing": "None",
            "workspace_wraparound": true
        }"#;
        let config: WorkspaceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.workspace_layout, WorkspaceLayout::Vertical);
    }

    #[test]
    fn config_with_scrolling_deserializes() {
        let json = r#"{
            "workspace_mode": "Global",
            "workspace_layout": "Scrolling",
            "action_on_typing": "None",
            "workspace_wraparound": false
        }"#;
        let config: WorkspaceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.workspace_layout, WorkspaceLayout::Scrolling);
    }

    #[test]
    fn layout_serializes_back_to_scrolling() {
        let json = serde_json::to_string(&WorkspaceLayout::Scrolling).unwrap();
        assert_eq!(json, r#""Scrolling""#);
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Action {
    #[default]
    None,
    OpenLauncher,
    OpenApplications,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputMatch {
    pub name: String,
    pub edid: Option<EdidProduct>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinnedWorkspace {
    pub output: OutputMatch,
    pub tiling_enabled: bool,
    pub id: Option<String>,
    pub name: Option<String>,
}
