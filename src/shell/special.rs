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
//! Named specials from the `[special]` config section get their own slot IDs
//! past the numbered range (see `cosmic_comp_config::special`), so toggling
//! a named special never disturbs a numbered one. A config without that
//! section produces the exact built-in behavior: twelve numbered slots, no
//! named slots, no geometry overrides.
//!
//! Special state lives in RAM only: a compositor restart forgets which
//! windows were parked and unconditionally re-maps them on their original
//! workspaces. That is a documented limitation, not a crash path.

use calloop::LoopHandle;
use cosmic_comp_config::special::{SPECIAL_NUMBERED_SLOTS, SpecialAnchor, SpecialConfig};
use smithay::desktop::layer_map_for_output;
use smithay::input::Seat;
use smithay::output::Output;
use smithay::utils::{IsAlive, Logical, Rectangle};
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

/// The four lifecycle states of one special workspace. The slide animation
/// itself lives in the floating layout (fire-and-forget, keyed per window),
/// so `Showing`/`Hiding` are the transient states while a toggle operation
/// is driving windows in or out; `settle` resolves them once the operation
/// is done. Retoggling mid-animation is safe in every interleaving: the
/// layout removes or replaces the per-window animation on each map/unmap,
/// so a canceled slide can never leave a phantom frame behind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum SpecialVisibility {
    #[default]
    Hidden,
    Showing,
    Visible,
    Hiding,
}

impl SpecialVisibility {
    /// The state a toggle enters from the current one. Toggling mid-slide
    /// reverses course (`Showing -> Hiding`, `Hiding -> Showing`) instead of
    /// getting stuck in a terminal state.
    fn toggle(self) -> Self {
        match self {
            Self::Hidden | Self::Hiding => Self::Showing,
            Self::Showing | Self::Visible => Self::Hiding,
        }
    }

    /// Resolve the transient states once the toggle operation finished
    /// driving its windows: the animation keeps running in the layout, but
    /// the store's bookkeeping is terminal again.
    fn settle(self) -> Self {
        match self {
            Self::Showing => Self::Visible,
            Self::Hiding => Self::Hidden,
            other => other,
        }
    }

    fn is_shown(self) -> bool {
        matches!(self, Self::Showing | Self::Visible)
    }
}

/// Pure bookkeeping of one special workspace, generic over the window type so
/// the policy can be unit tested without a compositor backend.
#[derive(Debug)]
struct SpecialStore<T> {
    entries: Vec<T>,
    visibility: SpecialVisibility,
    /// Window keyboard focus should return to when the special is shown
    /// again; stale values fall back to the oldest entry.
    last_focused: Option<T>,
}

impl<T> Default for SpecialStore<T> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            visibility: SpecialVisibility::default(),
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
        self.visibility.is_shown()
    }

    /// Apply a toggle transition and report whether the special is now
    /// being shown (as opposed to hidden).
    fn begin_toggle(&mut self) -> bool {
        self.visibility = self.visibility.toggle();
        self.visibility.is_shown()
    }

    /// Resolve the transient `Showing`/`Hiding` states after the toggle
    /// operation finished driving its windows.
    fn settle(&mut self) {
        self.visibility = self.visibility.settle();
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

/// Placement hint for a special slot, straight from the config. `None`
/// sizes fall back to the compositor's 80%x40% dropdown footprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpecialGeometry {
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub anchor: SpecialAnchor,
    /// Whether showing/hiding this special plays the slide animation.
    pub animate: bool,
}

/// Fractions of the output's non-exclusive zone used when the config gives
/// no explicit size: the Hyprland-style centered dropdown footprint.
const FALLBACK_WIDTH_FRACTION: i32 = 4;
const FALLBACK_WIDTH_DIVISOR: i32 = 5;
const FALLBACK_HEIGHT_FRACTION: i32 = 2;
const FALLBACK_HEIGHT_DIVISOR: i32 = 5;

/// Pure geometry policy: resolve the on-output rectangle for a special
/// window from its config hint and the output's non-exclusive zone.
/// Everything is in logical pixels, so fractional output scales are handled
/// by construction. Dimensions without a config entry fall back to the
/// 80%x40% dropdown footprint; sizes are clamped to the zone and positions
/// follow the configured anchor.
fn special_rectangle(
    geometry: SpecialGeometry,
    zone: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    let fallback_w = (zone.size.w * FALLBACK_WIDTH_FRACTION / FALLBACK_WIDTH_DIVISOR).max(1);
    let fallback_h = (zone.size.h * FALLBACK_HEIGHT_FRACTION / FALLBACK_HEIGHT_DIVISOR).max(1);
    let width = geometry
        .width
        .map(|w| w as i32)
        .unwrap_or(fallback_w)
        .clamp(1, zone.size.w.max(1));
    let height = geometry
        .height
        .map(|h| h as i32)
        .unwrap_or(fallback_h)
        .clamp(1, zone.size.h.max(1));

    let x = match geometry.anchor {
        SpecialAnchor::Left => zone.loc.x,
        SpecialAnchor::Right => zone.loc.x + zone.size.w - width,
        _ => zone.loc.x + (zone.size.w - width) / 2,
    };
    let y = match geometry.anchor {
        SpecialAnchor::Top => zone.loc.y,
        SpecialAnchor::Bottom => zone.loc.y + zone.size.h - height,
        _ => zone.loc.y + (zone.size.h - height) / 2,
    };
    Rectangle::new((x, y).into(), (width, height).into())
}

#[derive(Default)]
pub struct SpecialWorkspaceManager {
    /// Numbered special workspaces, grown on demand so `special 7` can exist
    /// without materializing 1..6 first. Slots `0..SPECIAL_NUMBERED_SLOTS`
    /// are the fixed numbered specials; slots at or after the offset belong
    /// to named specials, in the registry order of `named`.
    specials: Vec<SpecialStore<SpecialWindow>>,
    /// Named special registry; position in this vec plus the numbered
    /// offset is the slot ID (mirrors `SpecialConfig::slot_for_name`).
    named: Vec<String>,
    /// Placement hint per slot. Written from the config; consumed when a
    /// shown special is attached to the active workspace.
    geometry: std::collections::HashMap<usize, SpecialGeometry>,
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
    /// Build a manager pre-seeded from the `[special]` config section. An
    /// absent/default section yields exactly the built-in numbered behavior.
    pub fn from_config(config: &SpecialConfig) -> Self {
        let mut manager = Self::default();
        manager.apply_config(config);
        manager
    }

    /// Re-seed the named registry and geometry hints from the config.
    ///
    /// Already-parked windows are left where they are: slots are identity
    /// (numbered stay numbered, named keep their slot as long as the name
    /// survives a reload), so a config change never dumps windows onto the
    /// user's workspace. A name removed from the config orphans its slot —
    /// its windows stay parked until reassigned or closed.
    pub fn apply_config(&mut self, config: &SpecialConfig) {
        self.named = config.named.keys().cloned().collect();
        self.geometry.clear();
        for idx in 0..SPECIAL_NUMBERED_SLOTS {
            self.geometry.insert(
                idx,
                SpecialGeometry {
                    width: config.numbered.width,
                    height: config.numbered.height,
                    anchor: config.numbered.anchor,
                    animate: config.numbered.animate,
                },
            );
        }
        for (name, named) in &config.named {
            if let Some(idx) = config.slot_for_name(name) {
                self.geometry.insert(
                    idx,
                    SpecialGeometry {
                        width: named.width,
                        height: named.height,
                        anchor: named.anchor,
                        animate: named.animate,
                    },
                );
            }
        }
    }

    /// Slot ID for a named special, if the name is configured.
    pub fn slot_for_name(&self, name: &str) -> Option<usize> {
        self.named
            .iter()
            .position(|n| n == name)
            .map(|idx| SPECIAL_NUMBERED_SLOTS + idx)
    }

    /// The configured placement hint for a slot, if any.
    pub fn geometry_for(&self, slot: usize) -> Option<SpecialGeometry> {
        self.geometry.get(&slot).copied()
    }

    /// The configured named specials, in slot order.
    #[allow(dead_code)]
    pub fn named_specials(&self) -> &[String] {
        &self.named
    }

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
        let showing = self.special.slot_mut(idx).begin_toggle();
        if !showing {
            debug!(special = idx + 1, "hide special workspace");
            let geometry = self.special.geometry_for(idx);
            let attached: Vec<CosmicMapped> = self
                .special
                .slot_mut(idx)
                .entries()
                .filter(|w| w.attached_to.is_some())
                .map(|w| w.mapped.clone())
                .collect();
            for window in attached {
                self.special_detach_shown(&window, &output, geometry);
            }
            self.special.slot_mut(idx).settle();
            self.special_focus_candidate(seat, &output)
        } else {
            if self.special.slot_mut(idx).is_empty() {
                debug!(special = idx + 1, "show special workspace (empty)");
                self.special.slot_mut(idx).settle();
                return None;
            }
            debug!(special = idx + 1, "show special workspace");
            let geometry = self.special.geometry_for(idx);
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
                focus = self.special_attach_to_active(window, &output, geometry);
            }
            for window in &detached {
                if self.special.take_was_maximized(window) {
                    self.maximize_request(window, seat, false, loop_handle);
                }
            }
            self.special.slot_mut(idx).settle();
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
                let geometry = self.special.geometry_for(from);
                self.special_detach_shown(&window, &output, geometry);
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
            let geometry = self.special.geometry_for(idx);
            let focus = self.special_attach_to_active(&window, &output, geometry);
            if self.special.take_was_maximized(&window) {
                self.maximize_request(&window, seat, false, loop_handle);
            }
            return focus;
        }
        self.special_focus_candidate(seat, &output)
    }

    /// Named-special variants of the toggle/send primitives. They resolve
    /// the configured name to its slot (offset past the numbered range) and
    /// otherwise behave exactly like the numbered path — a named and a
    /// numbered special can be shown at the same time. Unknown names are
    /// ignored (logged), they have no slot.
    // No keybind path dispatches names yet; exposed for the follow-up wave.
    #[allow(dead_code)]
    pub fn special_toggle_named(
        &mut self,
        seat: &Seat<State>,
        name: &str,
        loop_handle: &LoopHandle<'static, State>,
    ) -> Option<KeyboardFocusTarget> {
        match self.special.slot_for_name(name) {
            Some(idx) => self.special_toggle(seat, idx, loop_handle),
            None => {
                debug!(special = name, "toggle for unconfigured named special");
                None
            }
        }
    }

    #[allow(dead_code)]
    pub fn special_send_current_named(
        &mut self,
        seat: &Seat<State>,
        name: &str,
        loop_handle: &LoopHandle<'static, State>,
    ) -> Option<KeyboardFocusTarget> {
        match self.special.slot_for_name(name) {
            Some(idx) => self.special_send_current(seat, idx, loop_handle),
            None => {
                debug!(special = name, "send to unconfigured named special");
                None
            }
        }
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
            let geometry = self.special.geometry_for(idx);
            let drifting: Vec<CosmicMapped> = self
                .special
                .slot_mut(idx)
                .entries()
                .filter(|w| w.attached_to.as_ref().is_some_and(|h| *h != current))
                .map(|w| w.mapped.clone())
                .collect();
            for window in drifting {
                if self
                    .special_detach_shown(&window, output, geometry)
                    .is_some()
                {
                    // Workspace switches re-attach without the slide: the
                    // window is already visible, moving it should not replay
                    // the dropdown.
                    self.special_attach_to_active(&window, output, None);
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
    /// With `Some(geometry)` the window slides back out through the top
    /// edge of the output (the hide half of the dropdown) instead of
    /// vanishing instantly.
    fn special_detach_shown(
        &mut self,
        window: &CosmicMapped,
        output: &Output,
        geometry: Option<SpecialGeometry>,
    ) -> Option<()> {
        let animate = geometry.is_some_and(|g| g.animate);
        let handle = {
            let workspace = self
                .workspaces
                .sets
                .get_mut(output)?
                .workspaces
                .iter_mut()
                .find(|ws| ws.floating_layer.mapped().any(|m| m == window))?;
            let handle = workspace.handle;
            // The slide target is the window's own footprint parked fully
            // above the top edge of the output's non-exclusive zone.
            let slide_to = animate.then(|| {
                let zone = layer_map_for_output(workspace.output())
                    .non_exclusive_zone()
                    .as_local();
                workspace
                    .floating_layer
                    .element_geometry(window)
                    .map(|mut geo| {
                        geo.loc.y = zone.loc.y - geo.size.h;
                        geo
                    })
            });
            workspace.floating_layer.unmap(window, slide_to.flatten());
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
    /// With `Some(geometry)` the window is placed (and sized) at the slot's
    /// configured footprint and slides in from the top edge like a
    /// dropdown; `None` keeps the automatic floating placement, unmoved.
    fn special_attach_to_active(
        &mut self,
        window: &CosmicMapped,
        output: &Output,
        geometry: Option<SpecialGeometry>,
    ) -> Option<KeyboardFocusTarget> {
        // The slot must be resolved before the workspace borrow: the
        // geometry hint comes out of the special store.
        let handle = {
            let workspace = self.active_space_mut(output)?;
            if let Some(geometry) = geometry {
                let zone = layer_map_for_output(workspace.output()).non_exclusive_zone();
                let rect = special_rectangle(geometry, zone);
                workspace
                    .floating_layer
                    .map_special(window.clone(), rect, geometry.animate);
            } else {
                workspace.floating_layer.map(window.clone(), None);
            }
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
    use super::{
        SpecialGeometry, SpecialStore, SpecialVisibility, SpecialWorkspaceManager,
        mru_focus_excluding, special_rectangle,
    };
    use cosmic_comp_config::special::{
        NumberedSpecialsConfig, SPECIAL_NUMBERED_SLOTS, SpecialAnchor, SpecialConfig,
    };
    use smithay::utils::{Logical, Rectangle, Size};
    use std::collections::BTreeMap;

    fn geometry(width: Option<u32>, height: Option<u32>, anchor: SpecialAnchor) -> SpecialGeometry {
        SpecialGeometry {
            width,
            height,
            anchor,
            animate: true,
        }
    }

    fn zone() -> Rectangle<i32, Logical> {
        Rectangle::new((0, 0).into(), (1920, 1080).into())
    }

    /// Manager built from an absent/default `[special]` section: no named
    /// slots, default geometry — the built-in numbered behavior, unchanged.
    #[test]
    fn default_config_changes_nothing() {
        let manager = SpecialWorkspaceManager::from_config(&SpecialConfig::default());
        assert!(manager.named_specials().is_empty());
        assert_eq!(manager.slot_for_name("anything"), None);
        assert_eq!(
            manager.geometry_for(0),
            Some(geometry(None, None, SpecialAnchor::Center))
        );
    }

    #[test]
    fn named_specials_resolve_to_slots_past_the_numbered_range() {
        let mut config = SpecialConfig::default();
        config
            .named
            .insert("terminal".to_string(), Default::default());
        config.named.insert("notes".to_string(), Default::default());
        let manager = SpecialWorkspaceManager::from_config(&config);
        assert_eq!(manager.slot_for_name("notes"), Some(SPECIAL_NUMBERED_SLOTS));
        assert_eq!(
            manager.slot_for_name("terminal"),
            Some(SPECIAL_NUMBERED_SLOTS + 1)
        );
        assert_eq!(manager.slot_for_name("unknown"), None);
    }

    #[test]
    fn named_geometry_is_independent_from_numbered_geometry() {
        let mut config = SpecialConfig {
            numbered: NumberedSpecialsConfig {
                width: Some(800),
                height: None,
                anchor: SpecialAnchor::Top,
                animate: false,
            },
            ..Default::default()
        };
        config.named.insert(
            "terminal".to_string(),
            cosmic_comp_config::special::NamedSpecialConfig {
                width: Some(1000),
                height: Some(400),
                anchor: SpecialAnchor::Bottom,
                on_demand: true,
                animate: true,
            },
        );
        let manager = SpecialWorkspaceManager::from_config(&config);
        assert_eq!(
            manager.geometry_for(0),
            Some(SpecialGeometry {
                width: Some(800),
                height: None,
                anchor: SpecialAnchor::Top,
                animate: false,
            })
        );
        assert_eq!(
            manager.geometry_for(SPECIAL_NUMBERED_SLOTS),
            Some(geometry(Some(1000), Some(400), SpecialAnchor::Bottom))
        );
    }

    /// Slot identity survives a config reload as long as the name does, so
    /// re-applying the same section must not reshuffle named specials.
    #[test]
    fn apply_config_keeps_slot_stability_across_reloads() {
        let mut config = SpecialConfig {
            named: BTreeMap::new(),
            ..Default::default()
        };
        config.named.insert("a".to_string(), Default::default());
        config.named.insert("b".to_string(), Default::default());
        let mut manager = SpecialWorkspaceManager::from_config(&config);
        let slot_a = manager.slot_for_name("a");
        manager.apply_config(&config);
        assert_eq!(manager.slot_for_name("a"), slot_a);
        // A name dropped from the config loses its slot mapping, but the
        // other named specials keep theirs.
        config.named.remove("a");
        manager.apply_config(&config);
        assert_eq!(manager.slot_for_name("a"), None);
        assert_eq!(manager.slot_for_name("b"), Some(SPECIAL_NUMBERED_SLOTS));
    }

    #[test]
    fn toggle_hidden_special_makes_it_visible() {
        let mut store = SpecialStore::<char>::default();
        assert!(!store.shown());
        assert!(store.begin_toggle());
        store.settle();
        assert!(store.shown());
    }

    #[test]
    fn toggle_visible_special_hides_it() {
        let mut store = SpecialStore::<char>::default();
        store.begin_toggle();
        store.settle();
        assert!(!store.begin_toggle());
        store.settle();
        assert!(!store.shown());
    }

    /// The state machine must be total: any toggle from any state yields a
    /// coherent next state, and settling resolves the transient ones. This
    /// is what keeps a mid-animation retoggle from wedging the slot in a
    /// half-shown bookkeeping state.
    #[test]
    fn visibility_toggles_are_total_and_settle() {
        // Showing resolves to Visible.
        let mut store = SpecialStore::<char>::default();
        assert!(store.begin_toggle());
        assert!(store.shown());
        store.settle();
        assert!(store.shown());
        // Retoggle mid-slide would go Showing -> Hiding; from the settled
        // Visible it goes Hiding all the same.
        assert!(!store.begin_toggle());
        assert!(!store.shown());
        store.settle();
        assert!(!store.shown());
        // Toggling again shows it once more.
        assert!(store.begin_toggle());
        store.settle();
        assert!(store.shown());
    }

    #[test]
    fn settle_without_toggle_is_a_no_op() {
        let mut store = SpecialStore::<char>::default();
        store.settle();
        assert!(!store.shown());
    }

    #[test]
    fn hidden_special_retains_windows() {
        let mut store = SpecialStore::<char>::default();
        store.send('a');
        store.begin_toggle();
        store.settle();
        store.begin_toggle();
        store.settle();
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
        store.begin_toggle();
        store.settle();
        assert!(store.is_empty());
        assert!(store.shown());
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
        first.begin_toggle();
        first.settle();
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

    /// The configured 80%x40% dropdown footprint fallback, centered.
    #[test]
    fn geometry_without_size_falls_back_to_centered_dropdown_footprint() {
        let rect = special_rectangle(geometry(None, None, SpecialAnchor::Center), zone());
        assert_eq!(rect, Rectangle::new((192, 324).into(), (1536, 432).into()));
    }

    /// An explicit config size wins over the fallback.
    #[test]
    fn geometry_uses_configured_size() {
        let rect = special_rectangle(
            geometry(Some(1000), Some(400), SpecialAnchor::Center),
            zone(),
        );
        assert_eq!(rect, Rectangle::new((460, 340).into(), (1000, 400).into()));
    }

    /// Unset dimensions individually fall back to the footprint fraction.
    #[test]
    fn geometry_falls_back_per_dimension() {
        let rect = special_rectangle(geometry(Some(800), None, SpecialAnchor::Center), zone());
        assert_eq!(rect.size, Size::from((800, 432)));
        let rect = special_rectangle(geometry(None, Some(200), SpecialAnchor::Center), zone());
        assert_eq!(rect.size, Size::from((1536, 200)));
    }

    #[test]
    fn geometry_anchors_to_each_edge() {
        let top = special_rectangle(geometry(None, None, SpecialAnchor::Top), zone());
        assert_eq!(top.loc, (192, 0).into());
        let bottom = special_rectangle(geometry(None, None, SpecialAnchor::Bottom), zone());
        assert_eq!(bottom.loc, (192, 648).into());
        let left = special_rectangle(geometry(None, None, SpecialAnchor::Left), zone());
        assert_eq!(left.loc, (0, 324).into());
        let right = special_rectangle(geometry(None, None, SpecialAnchor::Right), zone());
        assert_eq!(right.loc, (384, 324).into());
        let center = special_rectangle(geometry(None, None, SpecialAnchor::Center), zone());
        assert_eq!(center.loc, (192, 324).into());
    }

    /// A non-empty zone offset (panels reserve space) shifts the anchor
    /// accordingly: everything is computed relative to the zone.
    #[test]
    fn geometry_is_relative_to_the_non_exclusive_zone() {
        let zone = Rectangle::new((64, 32).into(), (1000, 800).into());
        let rect = special_rectangle(geometry(Some(500), Some(200), SpecialAnchor::Top), zone);
        assert_eq!(rect, Rectangle::new((314, 32).into(), (500, 200).into()));
    }

    /// Configured sizes larger than the output are clamped so a typo in the
    /// config can never push the dropdown off-screen.
    #[test]
    fn geometry_clamps_to_the_zone() {
        let rect = special_rectangle(
            geometry(Some(99999), Some(99999), SpecialAnchor::Bottom),
            zone(),
        );
        assert_eq!(rect, Rectangle::new((0, 0).into(), (1920, 1080).into()));
    }

    /// SpecialVisibility is exercised through `SpecialStore` above; this
    /// pins the raw transition table so the enum cannot drift silently.
    #[test]
    fn visibility_transition_table() {
        use SpecialVisibility::*;
        assert_eq!(Hidden.toggle(), Showing);
        assert_eq!(Hiding.toggle(), Showing);
        assert_eq!(Visible.toggle(), Hiding);
        assert_eq!(Showing.toggle(), Hiding);
        assert_eq!(Showing.settle(), Visible);
        assert_eq!(Hiding.settle(), Hidden);
        assert_eq!(Hidden.settle(), Hidden);
        assert_eq!(Visible.settle(), Visible);
        assert!(Showing.is_shown() && Visible.is_shown());
        assert!(!Hidden.is_shown() && !Hiding.is_shown());
    }
}
