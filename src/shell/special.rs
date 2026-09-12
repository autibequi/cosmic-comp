// SPDX-License-Identifier: GPL-3.0-only

//! Special workspaces: a second visibility domain on top of the regular
//! workspaces (the Hyprland `special` workspace / scratchpad concept).
//!
//! Windows in a special workspace stay mapped and alive at all times. While
//! the special workspace is hidden they are detached from every layout — the
//! same mechanism minimized windows use — so nothing renders or focuses them.
//! While shown they are attached to the floating layer of the active
//! workspace, which stacks them above tiled windows without touching the
//! renderer. Attaching/detaching goes through the same primitives as
//! [`Shell::move_element`], so the workspace protocol, focus stacks and
//! reactive popups keep working.
//!
//! Specials are numbered (`special 1..N`, bound to logo+F1..F12 by the
//! internal defaults) and independent: showing one does not hide the others.

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

/// Pure bookkeeping of one special workspace, generic over the window type so
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

    fn take<U>(&mut self, key: &U) -> Option<T>
    where
        T: PartialEq<U>,
    {
        let idx = self.entries.iter().position(|e| e == key)?;
        Some(self.entries.remove(idx))
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

    fn len(&self) -> usize {
        self.entries.len()
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
    /// Numbered special workspaces, grown on demand so `special 7` can exist
    /// without materializing 1..6 first.
    specials: Vec<SpecialStore<SpecialWindow>>,
}

impl std::fmt::Debug for SpecialWorkspaceManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpecialWorkspaceManager")
            .field(
                "specials",
                &self
                    .specials
                    .iter()
                    .map(|s| (s.shown(), s.len()))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl SpecialWorkspaceManager {
    fn slot_mut(&mut self, idx: usize) -> &mut SpecialStore<SpecialWindow> {
        while self.specials.len() <= idx {
            self.specials.push(SpecialStore::default());
        }
        &mut self.specials[idx]
    }

    /// Which special holds `window`, and whether it is currently shown.
    fn holder(&self, mapped: &CosmicMapped) -> Option<(usize, bool)> {
        self.specials
            .iter()
            .position(|s| s.contains(mapped))
            .map(|idx| (idx, self.specials[idx].shown()))
    }

    fn drop_dead(&mut self) {
        for slot in &mut self.specials {
            slot.retain(|w| w.mapped.alive());
        }
    }

    fn shown_any(&self) -> bool {
        self.specials.iter().any(|s| s.shown())
    }

    fn index_of(&self, mapped: &CosmicMapped) -> Option<usize> {
        self.specials.iter().position(|s| s.contains(mapped))
    }
}

impl Shell {
    /// Show special workspace `idx` if it is hidden, hide it if it is shown.
    ///
    /// Returns a focus candidate for the remaining windows, so the caller can
    /// keep the keyboard on the workspace instead of the detached windows.
    pub fn special_toggle(
        &mut self,
        seat: &Seat<State>,
        idx: usize,
    ) -> Option<KeyboardFocusTarget> {
        self.special.drop_dead();
        let output = seat.active_output();
        if self.special.slot_mut(idx).shown() {
            debug!(special = idx + 1, "hide special workspace");
            self.special.slot_mut(idx).set_shown(false);
            let attached: Vec<CosmicMapped> = self
                .special
                .slot_mut(idx)
                .entries()
                .filter(|w| w.attached_to.is_some())
                .map(|w| w.mapped.clone())
                .collect();
            for window in attached {
                self.special_detach_shown(&window, &output);
            }
            self.special_focus_candidate(&output)
        } else {
            self.special.slot_mut(idx).set_shown(true);
            if self.special.slot_mut(idx).is_empty() {
                debug!(special = idx + 1, "show special workspace (empty)");
                return None;
            }
            debug!(special = idx + 1, "show special workspace");
            let mut focus = None;
            let detached: Vec<CosmicMapped> = self
                .special
                .slot_mut(idx)
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

    /// Send the currently focused window to special workspace `idx`.
    ///
    /// While that special is shown the window becomes visible immediately;
    /// while hidden it is parked detached and invisible. Sending a window that
    /// already belongs to another special moves it between specials.
    pub fn special_send_current(
        &mut self,
        seat: &Seat<State>,
        idx: usize,
    ) -> Option<KeyboardFocusTarget> {
        self.special.drop_dead();
        let KeyboardFocusTarget::Element(window) = seat.get_keyboard()?.current_focus()? else {
            return None;
        };
        debug!(
            special = idx + 1,
            "send focused window to special workspace"
        );
        let output = seat.active_output();

        let entry = if let Some((from, was_shown)) = self.special.holder(&window) {
            if was_shown {
                self.special_detach_shown(&window, &output);
            }
            self.special.slot_mut(from).take(&window)
        } else {
            let restore = self.special_detach_window(&window)?;
            Some(SpecialWindow {
                mapped: window.clone(),
                restore: Some(restore),
                attached_to: None,
            })
        };
        self.special.slot_mut(idx).send(entry?);

        if self.special.slot_mut(idx).shown() {
            return self.special_attach_to_active(&window, &output);
        }
        self.special_focus_candidate(&output)
    }

    /// Re-attach shown special windows to the newly activated workspace: a
    /// workspace switch would otherwise leave them floating on the old one.
    pub fn special_follow(&mut self, output: &Output) {
        if !self.special.shown_any() {
            return;
        }
        let Some(current) = self.active_space(output).map(|ws| ws.handle) else {
            return;
        };
        for idx in 0..self.special.specials.len() {
            if !self.special.slot_mut(idx).shown() {
                continue;
            }
            let drifting: Vec<CosmicMapped> = self
                .special
                .slot_mut(idx)
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
    }

    /// Detach a window from its workspace layout, mirroring the detach half
    /// of `Shell::move_element`. Returns the captured restore state.
    fn special_detach_window(&mut self, window: &CosmicMapped) -> Option<WorkspaceRestoreData> {
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
        Some(restore)
    }

    /// Detach a shown window from the workspace it is floating on. Windows
    /// stay in their special store; only the visible attachment is removed.
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
        let idx = self.special.index_of(window)?;
        if let Some(entry) = self.special.slot_mut(idx).entry_mut(window) {
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
        let idx = self.special.index_of(window)?;
        if let Some(entry) = self.special.slot_mut(idx).entry_mut(window) {
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

    #[test]
    fn take_removes_window_from_slot() {
        let mut store = SpecialStore::<char>::default();
        store.send('a');
        assert_eq!(store.take(&'a'), Some('a'));
        assert!(!store.contains(&'a'));
    }

    #[test]
    fn slots_are_independent() {
        // Two stores side by side never share visibility or entries — this is
        // the invariant the numbered specials rely on.
        let mut first = SpecialStore::<char>::default();
        let mut second = SpecialStore::<char>::default();
        first.send('a');
        first.set_shown(true);
        assert!(first.shown());
        assert!(!second.shown());
        assert!(!second.contains(&'a'));
    }
}
