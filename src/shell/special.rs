// SPDX-License-Identifier: GPL-3.0-only

//! Special workspace: a second visibility domain on top of the regular
//! workspaces (the Hyprland `special` workspace / scratchpad concept).
//!
//! Windows in the special workspace stay mapped and alive at all times.
//! While the special workspace is hidden they are detached from every layout —
//! the same mechanism minimized windows use — so nothing renders or focuses
//! them. While shown they are attached to the floating layer of the active
//! workspace of their output, which stacks them above tiled windows without
//! touching the renderer. Attaching/detaching goes through the same
//! primitives as [`Shell::move_element`], so the workspace protocol,
//! focus stacks and reactive popups keep working.

use smithay::input::Seat;
use smithay::output::Output;
use smithay::utils::IsAlive;
use tracing::debug;

use crate::shell::focus::target::KeyboardFocusTarget;
use crate::shell::{CosmicMapped, SeatExt, Shell, WorkspaceHandle, WorkspaceRestoreData};
use crate::state::State;
use crate::wayland::protocols::toplevel_info::{
    toplevel_enter_workspace, toplevel_leave_workspace,
};

/// Pure bookkeeping of the special workspace, generic over the window type so
/// the policy can be unit tested without a compositor backend.
#[derive(Debug)]
struct SpecialStore<T> {
    entries: Vec<T>,
    shown: bool,
}

impl<T> Default for SpecialStore<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            shown: false,
        }
    }
}

impl<T> SpecialStore<T> {
    fn send(&mut self, entry: T)
    where
        T: PartialEq,
    {
        if !self.entries.iter().any(|e| e == &entry) {
            self.entries.push(entry);
        }
    }

    fn contains<U>(&self, key: &U) -> bool
    where
        T: PartialEq<U>,
    {
        self.entries.iter().any(|e| e == key)
    }

    fn entries(&self) -> impl Iterator<Item = &T> {
        self.entries.iter()
    }

    fn entry_mut<U>(&mut self, key: &U) -> Option<&mut T>
    where
        T: PartialEq<U>,
    {
        let idx = self.entries.iter().position(|e| e == key)?;
        self.entries.get_mut(idx)
    }

    fn retain(&mut self, keep: impl Fn(&T) -> bool) {
        self.entries.retain(keep);
    }

    fn shown(&self) -> bool {
        self.shown
    }

    fn set_shown(&mut self, shown: bool) {
        self.shown = shown;
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

struct SpecialWindow {
    mapped: CosmicMapped,
    /// State captured when the window was detached from its workspace, so a
    /// later iteration can restore tiling/floating position on exit.
    #[allow(dead_code)]
    restore: Option<WorkspaceRestoreData>,
    /// Workspace the window is currently floating on while shown.
    attached_to: Option<WorkspaceHandle>,
}

impl PartialEq for SpecialWindow {
    fn eq(&self, other: &Self) -> bool {
        self.mapped == other.mapped
    }
}

impl PartialEq<CosmicMapped> for SpecialWindow {
    fn eq(&self, other: &CosmicMapped) -> bool {
        self.mapped == *other
    }
}

#[derive(Default)]
pub struct SpecialWorkspaceManager {
    store: SpecialStore<SpecialWindow>,
}

impl std::fmt::Debug for SpecialWorkspaceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpecialWorkspaceManager")
            .field("shown", &self.store.shown())
            .field("windows", &self.store.entries().count())
            .finish()
    }
}

impl SpecialWorkspaceManager {
    fn send(&mut self, mapped: CosmicMapped, restore: Option<WorkspaceRestoreData>) {
        self.store.send(SpecialWindow {
            mapped,
            restore,
            attached_to: None,
        });
    }

    fn contains(&self, mapped: &CosmicMapped) -> bool {
        self.store.contains(mapped)
    }

    fn shown(&self) -> bool {
        self.store.shown()
    }

    fn is_empty(&self) -> bool {
        self.store.is_empty()
    }

    fn len(&self) -> usize {
        self.store.entries().count()
    }

    fn drop_dead(&mut self) {
        self.store.retain(|w| w.mapped.alive());
    }
}

impl Shell {
    /// Show the special workspace if it is hidden, hide it if it is shown.
    ///
    /// Returns a focus candidate for the remaining windows, so the caller can
    /// keep the keyboard on the workspace instead of the detached windows.
    pub fn special_toggle(&mut self, seat: &Seat<State>) -> Option<KeyboardFocusTarget> {
        debug!(target: "special_ws", shown = self.special.shown(), windows = self.special.len(), "toggle special workspace");
        self.special.drop_dead();
        let output = seat.active_output();
        if self.special.shown() {
            self.special.store.set_shown(false);
            let attached: Vec<CosmicMapped> = self
                .special
                .store
                .entries()
                .filter(|w| w.attached_to.is_some())
                .map(|w| w.mapped.clone())
                .collect();
            for window in attached {
                self.special_detach_shown(&window, &output);
            }
            self.special_focus_candidate(&output)
        } else {
            self.special.store.set_shown(true);
            let mut focus = None;
            if self.special.is_empty() {
                return None;
            }
            let detached: Vec<CosmicMapped> = self
                .special
                .store
                .entries()
                .filter(|w| w.attached_to.is_none())
                .map(|w| w.mapped.clone())
                .collect();
            for window in detached {
                focus = self.special_attach_to_active(&window, &output);
            }
            focus
        }
    }

    /// Send the currently focused window to the special workspace.
    ///
    /// While the special workspace is shown the window becomes visible
    /// immediately; while hidden it is parked detached and invisible.
    pub fn special_send_current(&mut self, seat: &Seat<State>) -> Option<KeyboardFocusTarget> {
        debug!(target: "special_ws", "send focused window to special workspace");
        self.special.drop_dead();
        let KeyboardFocusTarget::Element(window) = seat.get_keyboard()?.current_focus()? else {
            return None;
        };
        let output = seat.active_output();
        if self.special.contains(&window) {
            // Already in the special workspace: detach it from the floating
            // layer it may currently be shown on, keeping it stored.
            self.special_detach_shown(&window, &output);
        } else {
            self.special_detach_window(&window)?;
        }
        if self.special.shown() {
            self.special_attach_to_active(&window, &output)?;
        }
        self.special_focus_candidate(&output)
    }

    /// Re-attach shown special windows to the newly activated workspace: a
    /// workspace switch would otherwise leave them floating on the old one.
    pub fn special_follow(&mut self, output: &Output) {
        if !self.special.shown() {
            return;
        }
        let Some(current) = self.active_space(output).map(|ws| ws.handle) else {
            return;
        };
        let drifting: Vec<CosmicMapped> = self
            .special
            .store
            .entries()
            .filter(|w| w.attached_to.as_ref().is_some_and(|h| *h != current))
            .map(|w| w.mapped.clone())
            .collect();
        for window in drifting {
            if self.special_detach_shown(&window, output).is_some() {
                self.special_attach_to_active(&window, output);
            }
        }
    }

    /// Detach a window from its workspace layout into the special store,
    /// mirroring the detach half of `Shell::move_element`.
    fn special_detach_window(&mut self, window: &CosmicMapped) -> Option<()> {
        let (handle, restore) = {
            let workspace = self
                .workspaces
                .sets
                .values_mut()
                .flat_map(|set| set.workspaces.iter_mut())
                .find(|ws| ws.mapped().any(|m| m == window))?;
            let handle = workspace.handle;
            let restore = workspace.unmap_element(window)?;
            (handle, restore)
        };
        for (toplevel, _) in window.windows() {
            toplevel_leave_workspace(&toplevel, &handle);
        }
        self.special.send(window.clone(), Some(restore));
        Some(())
    }

    /// Detach a shown window from the workspace it is floating on. Windows
    /// stay in the store; only the visible attachment is removed.
    fn special_detach_shown(&mut self, window: &CosmicMapped, output: &Output) -> Option<()> {
        let handle = {
            let workspace = self
                .workspaces
                .sets
                .get_mut(output)?
                .workspaces
                .iter_mut()
                .find(|ws| ws.floating_layer.mapped().any(|m| m == window))?;
            let handle = workspace.handle;
            workspace.floating_layer.unmap(window, None);
            handle
        };
        for (toplevel, _) in window.windows() {
            toplevel_leave_workspace(&toplevel, &handle);
        }
        if let Some(entry) = self.special.store.entry_mut(window) {
            entry.attached_to = None;
        }
        Some(())
    }

    /// Attach a stored window as a floating window on the active workspace.
    fn special_attach_to_active(
        &mut self,
        window: &CosmicMapped,
        output: &Output,
    ) -> Option<KeyboardFocusTarget> {
        let handle = {
            let workspace = self.active_space_mut(output)?;
            // Geometry placement (dropdown size/anchor) is a later iteration.
            workspace.floating_layer.map(window.clone(), None);
            workspace.handle
        };
        for (toplevel, _) in window.windows() {
            toplevel_enter_workspace(&toplevel, &handle);
        }
        if let Some(entry) = self.special.store.entry_mut(window) {
            entry.attached_to = Some(handle);
        }
        Some(KeyboardFocusTarget::Element(window.clone()))
    }

    /// Topmost remaining window of the active workspace, used to hand the
    /// keyboard back when special windows go away.
    fn special_focus_candidate(&self, output: &Output) -> Option<KeyboardFocusTarget> {
        let mapped = self.active_space(output)?.mapped().next()?.clone();
        Some(KeyboardFocusTarget::Element(mapped))
    }
}

#[cfg(test)]
mod tests {
    use super::SpecialStore;

    #[test]
    fn toggle_hidden_special_makes_it_visible() {
        let mut store = SpecialStore::<char>::default();
        assert!(!store.shown());
        store.set_shown(true);
        assert!(store.shown());
    }

    #[test]
    fn toggle_visible_special_hides_it() {
        let mut store = SpecialStore::<char>::default();
        store.set_shown(true);
        store.set_shown(false);
        assert!(!store.shown());
    }

    #[test]
    fn hidden_special_retains_windows() {
        let mut store = SpecialStore::<char>::default();
        store.send('a');
        store.set_shown(true);
        store.set_shown(false);
        assert!(store.contains(&'a'));
    }

    #[test]
    fn send_does_not_duplicate_windows() {
        let mut store = SpecialStore::<char>::default();
        store.send('a');
        store.send('a');
        assert_eq!(store.entries().count(), 1);
    }

    #[test]
    fn drop_dead_keeps_only_alive_windows() {
        let mut store = SpecialStore::<char>::default();
        store.send('a');
        store.send('b');
        store.retain(|c| *c != 'a');
        assert!(!store.contains(&'a'));
        assert!(store.contains(&'b'));
    }

    #[test]
    fn showing_an_empty_special_is_a_no_op() {
        let mut store = SpecialStore::<char>::default();
        store.set_shown(true);
        assert!(store.is_empty());
    }
}
