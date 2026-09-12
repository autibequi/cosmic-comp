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
//!
//! Special state lives in RAM only: a compositor restart forgets which
//! windows were parked and unconditionally re-maps them on their original
//! workspaces. That is a documented limitation, not a crash path.

use calloop::LoopHandle;
use smithay::input::Seat;
use smithay::output::Output;
use smithay::utils::IsAlive;
use tracing::debug;

use crate::shell::focus::FocusTarget;
use crate::shell::focus::target::KeyboardFocusTarget;
use crate::shell::{
    CosmicMapped, CosmicSurface, FloatingRestoreData, SeatExt, Shell, WorkspaceHandle,
    WorkspaceRestoreData,
};
use crate::state::State;
use crate::utils::prelude::*;
use crate::wayland::protocols::toplevel_info::{
    toplevel_enter_workspace, toplevel_leave_workspace,
};

/// Pure bookkeeping of one special workspace, generic over the window type so
/// the policy can be unit tested without a compositor backend.
#[derive(Debug)]
struct SpecialStore<T> {
    entries: Vec<T>,
    shown: bool,
    /// Window keyboard focus should return to when the special is shown
    /// again; stale values fall back to the oldest entry.
    last_focused: Option<T>,
}

impl<T> Default for SpecialStore<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            shown: false,
            last_focused: None,
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
        if self.last_focused.as_ref().is_some_and(|l| l == key) {
            self.last_focused = None;
        }
        Some(self.entries.remove(idx))
    }

    /// Remember `entry` as the window focus returns to when the special is
    /// shown again.
    fn set_last_focused(&mut self, entry: &T)
    where
        T: Clone,
    {
        self.last_focused = Some(entry.clone());
    }

    /// The window keyboard focus should land on when the special is shown:
    /// the last focused window while it is still a member, else the oldest
    /// member. Pure policy, unit tested without a compositor.
    fn focus_to_restore(&self) -> Option<&T>
    where
        T: PartialEq,
    {
        if self.last_focused.as_ref().is_some_and(|l| self.contains(l)) {
            self.last_focused.as_ref()
        } else {
            self.entries.first()
        }
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

#[derive(Clone)]
struct SpecialWindow {
    mapped: CosmicMapped,
    /// State captured when the window was detached from its workspace, so a
    /// later iteration can restore tiling/floating position on exit.
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

    /// Whether `mapped` is currently attached (visible) to a workspace.
    fn is_attached(&self, mapped: &CosmicMapped) -> bool {
        self.specials
            .iter()
            .any(|s| s.entries().any(|w| w == mapped && w.attached_to.is_some()))
    }

    /// Consume the `was_maximized` flag captured at send time, so the state
    /// is restored exactly once on show.
    fn take_was_maximized(&mut self, mapped: &CosmicMapped) -> bool {
        let Some(idx) = self.specials.iter().position(|s| s.contains(mapped)) else {
            return false;
        };
        let Some(entry) = self.specials[idx].entry_mut(mapped) else {
            return false;
        };
        let was = match &mut entry.restore {
            Some(WorkspaceRestoreData::Floating(f)) => f.was_maximized,
            Some(WorkspaceRestoreData::Tiling(t)) => t.was_maximized,
            _ => false,
        };
        if was {
            match entry.restore.as_mut().unwrap() {
                WorkspaceRestoreData::Floating(f) => f.was_maximized = false,
                WorkspaceRestoreData::Tiling(t) => t.was_maximized = false,
                _ => {}
            }
        }
        was
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
        loop_handle: &LoopHandle<'static, State>,
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
            self.special_focus_candidate(seat, &output)
        } else {
            self.special.slot_mut(idx).set_shown(true);
            if self.special.slot_mut(idx).is_empty() {
                debug!(special = idx + 1, "show special workspace (empty)");
                return None;
            }
            debug!(special = idx + 1, "show special workspace");
            // Focus returns to the window that had it when the special was
            // last hidden, not to an arbitrary member.
            let restore_focus = self
                .special
                .slot_mut(idx)
                .focus_to_restore()
                .map(|w| w.mapped.clone());
            let mut focus = None;
            let detached: Vec<CosmicMapped> = self
                .special
                .slot_mut(idx)
                .entries()
                .filter(|w| w.attached_to.is_none())
                .map(|w| w.mapped.clone())
                .collect();
            for window in &detached {
                focus = self.special_attach_to_active(window, &output);
            }
            for window in &detached {
                if self.special.take_was_maximized(window) {
                    self.maximize_request(window, seat, false, loop_handle);
                }
            }
            match restore_focus {
                Some(window) if self.special.is_attached(&window) => {
                    Some(KeyboardFocusTarget::Element(window))
                }
                _ => focus,
            }
        }
    }

    /// Send the currently focused window to special workspace `idx`.
    ///
    /// While that special is shown the window becomes visible immediately;
    /// while hidden it is parked detached and invisible. Sending a window that
    /// already belongs to another special moves it between specials.
    ///
    /// Fullscreen windows are unfullscreened first: they are held by the
    /// workspace's fullscreen layer, which the detach path cannot reach.
    pub fn special_send_current(
        &mut self,
        seat: &Seat<State>,
        idx: usize,
        loop_handle: &LoopHandle<'static, State>,
    ) -> Option<KeyboardFocusTarget> {
        self.special.drop_dead();
        let mut window = match seat.get_keyboard()?.current_focus()? {
            KeyboardFocusTarget::Element(window) => window,
            KeyboardFocusTarget::Fullscreen(surface) => {
                debug!(
                    special = idx + 1,
                    "unfullscreening focused window before send"
                );
                match self.unfullscreen_request(&surface, loop_handle) {
                    Some(KeyboardFocusTarget::Element(window)) => window,
                    _ => {
                        debug!(
                            special = idx + 1,
                            "could not unfullscreen focused window; refusing to send"
                        );
                        return None;
                    }
                }
            }
            _ => return None,
        };
        // Keyboard focus may still point at the Element while the window is
        // mapped fullscreen (Element(X) -> Fullscreen(X) focus transitions
        // are suppressed), so the window itself has to be checked too.
        if let Some(surface) = self.fullscreen_surface_of(&window)
            && let Some(KeyboardFocusTarget::Element(remapped)) =
                self.unfullscreen_request(&surface, loop_handle)
        {
            window = remapped;
        }
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
            let focus = self.special_attach_to_active(&window, &output);
            if self.special.take_was_maximized(&window) {
                self.maximize_request(&window, seat, false, loop_handle);
            }
            return focus;
        }
        self.special_focus_candidate(seat, &output)
    }

    /// Re-attach shown special windows to the newly activated workspace: a
    /// workspace switch would otherwise leave them floating on the old one.
    pub fn special_follow(&mut self, output: &Output) {
        if !self.special.shown_any() {
            return;
        }
        self.special_revalidate_attachments();
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

    /// Forget visible attachments whose workspace no longer exists (an
    /// output was removed while a special was shown): the windows stay
    /// parked in their slot and can be re-shown later. Must not crash on
    /// removed outputs — the lookups are all fallible on purpose.
    fn special_revalidate_attachments(&mut self) {
        let mut stranded = Vec::new();
        for slot in &self.special.specials {
            for entry in slot.entries() {
                if let Some(handle) = entry.attached_to
                    && self.workspaces.space_for_handle(&handle).is_none()
                {
                    stranded.push(entry.mapped.clone());
                }
            }
        }
        for window in stranded {
            if let Some(idx) = self.special.index_of(&window)
                && let Some(entry) = self.special.slot_mut(idx).entry_mut(&window)
            {
                entry.attached_to = None;
            }
        }
    }

    /// Detach a window from its workspace layout, mirroring the detach half
    /// of `Shell::move_element`. Returns the captured restore state.
    fn special_detach_window(&mut self, window: &CosmicMapped) -> Option<WorkspaceRestoreData> {
        // Sticky windows live on the set's sticky layer, outside every
        // workspace layout; capture their geometry and detach from there.
        let is_sticky = self
            .workspaces
            .sets
            .values()
            .any(|set| set.sticky_layer.mapped().any(|m| m == window));
        if is_sticky {
            // A maximized sticky window is additionally mapped (maximized)
            // on a workspace's floating layer; unmaximize it first so it
            // lives in exactly one layer before detaching.
            if window.maximized_state.lock().unwrap().is_some() {
                self.unmaximize_request(window);
            }
            let (_, set) = self
                .workspaces
                .sets
                .iter_mut()
                .find(|(_, set)| set.sticky_layer.mapped().any(|m| m == window))?;
            let geometry = set.sticky_layer.element_geometry(window)?;
            let output_size = set.output.geometry().size.as_logical();
            set.sticky_layer.unmap(window, None)?;
            for ws in &mut set.workspaces {
                ws.focus_stack.remove_mapped(window);
            }
            return Some(WorkspaceRestoreData::Floating(FloatingRestoreData {
                geometry,
                output_size,
                was_maximized: false,
                was_snapped: None,
            }));
        }

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
            // Drop the window from every seat's focus stack so focus fixups
            // do not keep handing the keyboard back to a hidden window.
            workspace.focus_stack.remove_mapped(window);
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
            let entry = entry.clone();
            self.special.slot_mut(idx).set_last_focused(&entry);
        }
        Some(KeyboardFocusTarget::Element(window.clone()))
    }

    /// Hand the keyboard back to a regular window when special windows go
    /// away: the most recently focused window of the active workspace that is
    /// not parked in a special workspace, falling back to the base of the
    /// workspace's window stack.
    fn special_focus_candidate(
        &self,
        seat: &Seat<State>,
        output: &Output,
    ) -> Option<KeyboardFocusTarget> {
        let workspace = self.active_space(output)?;
        let stack = workspace.focus_stack.get(seat);
        let candidate = mru_focus_excluding(stack.iter(), |target| match target {
            FocusTarget::Window(mapped) => self.special.index_of(mapped).is_some(),
            // A fullscreen surface would cover the special being shown.
            FocusTarget::Fullscreen(_) => true,
        });
        match candidate {
            Some(FocusTarget::Window(mapped)) => Some(KeyboardFocusTarget::Element(mapped.clone())),
            Some(FocusTarget::Fullscreen(surface)) => {
                Some(KeyboardFocusTarget::Fullscreen(surface.clone()))
            }
            None => {
                let mapped = workspace.mapped().next()?.clone();
                Some(KeyboardFocusTarget::Element(mapped))
            }
        }
    }

    /// The surface `mapped` is currently mapped fullscreen on, if any.
    fn fullscreen_surface_of(&self, mapped: &CosmicMapped) -> Option<CosmicSurface> {
        let active = mapped.active_window();
        self.workspaces.iter().find_map(|(_, set)| {
            set.workspaces.iter().find_map(|ws| {
                ws.get_fullscreen_surfaces()
                    .find(|fs| fs.surface == active)
                    .map(|fs| fs.surface.clone())
            })
        })
    }
}

/// First candidate in MRU order that is not excluded — the pure policy
/// behind `special_focus_candidate`, unit tested without a compositor.
fn mru_focus_excluding<'a, T>(
    mru: impl Iterator<Item = &'a T>,
    mut excluded: impl FnMut(&T) -> bool,
) -> Option<&'a T> {
    mru.into_iter().find(|t| !excluded(t))
}

#[cfg(test)]
mod tests {
    use super::{SpecialStore, mru_focus_excluding};

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
        let second = SpecialStore::<char>::default();
        first.send('a');
        first.set_shown(true);
        assert!(first.shown());
        assert!(!second.shown());
        assert!(!second.contains(&'a'));
    }

    #[test]
    fn focus_restores_to_last_focused_window() {
        let mut store = SpecialStore::<char>::default();
        store.send('a');
        store.send('b');
        store.set_last_focused(&'b');
        assert_eq!(store.focus_to_restore(), Some(&'b'));
    }

    #[test]
    fn focus_restores_to_oldest_entry_without_last_focused() {
        let mut store = SpecialStore::<char>::default();
        store.send('a');
        store.send('b');
        assert_eq!(store.focus_to_restore(), Some(&'a'));
    }

    #[test]
    fn stale_last_focused_falls_back_to_oldest_entry() {
        let mut store = SpecialStore::<char>::default();
        store.send('a');
        store.send('b');
        store.set_last_focused(&'b');
        store.take(&'b');
        assert_eq!(store.focus_to_restore(), Some(&'a'));
    }

    #[test]
    fn take_clears_matching_last_focused() {
        let mut store = SpecialStore::<char>::default();
        store.send('a');
        store.set_last_focused(&'a');
        store.take(&'a');
        assert_eq!(store.focus_to_restore(), None);
    }

    #[test]
    fn mru_focus_skips_special_windows() {
        // MRU order: most recent first. 'b' is parked in a special.
        let mru = ['a', 'b', 'c'];
        let picked = mru_focus_excluding(mru.iter(), |t| *t == 'b');
        assert_eq!(picked, Some(&'a'));
    }

    #[test]
    fn mru_focus_returns_none_when_everything_is_special() {
        let mru = ['a', 'b'];
        let picked = mru_focus_excluding(mru.iter(), |_| true);
        assert_eq!(picked, None);
    }

    #[test]
    fn mru_focus_takes_first_free_window() {
        let mru = ['x', 'y'];
        let picked = mru_focus_excluding(mru.iter(), |_| false);
        assert_eq!(picked, Some(&'x'));
    }
}
