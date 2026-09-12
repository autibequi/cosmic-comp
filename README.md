# cosmic-comp (fork)

This is a fork of [pop-os/cosmic-comp](https://github.com/pop-os/cosmic-comp)
(the COSMIC compositor), maintained at `autibequi/cosmic-comp`.

**Fork feature: special workspaces** (scratchpad-style overlays), developed on
the `special-workspace` branch. Everything else tracks upstream.

## Special workspaces (fork feature)

Special workspaces are a second visibility domain, separate from the regular
workspace stack: windows living in a hidden special are detached from the
normal layout entirely, and a shown special floats over the active workspace.

### Keybinds

| Chord | Action |
|---|---|
| `logo+F1..F12` | Toggle special workspace *N* (1..12) |
| `logo+shift+F1..F12` | Send the focused window to special workspace *N* |

Both chords are intercepted internally (in `src/input/mod.rs`, as
`PrivateAction::ToggleSpecial` / `PrivateAction::SendToSpecial`), so they do
not collide with user-configured bindings.

### Semantics

- **Hidden**: the special's windows are detached (they keep their state but
  leave the visible tree).
- **Shown**: the special's windows float on the *active* workspace of the
  output.
- **Focus**: hiding the special restores focus to the most recently used
  (MRU) window outside it; showing it focuses the special's
  `last_focused` window.
- Sending a **fullscreen** window unfullscreens it first; a **maximized**
  window is restored.
- Multi-output: the special follows `seat.active_output()`.

### Configuration (optional, `[special]` in cosmic-comp-config)

The whole section is optional — an absent `[special]` table behaves exactly
like a compositor built without this feature (zero change).

Two independent kinds:

```toml
[special.numbered]
# Shared geometry for special 1..12. Absent fields fall back to the
# built-in 80%x40% centered dropdown footprint.
width = 1000        # optional
height = 700        # optional
anchor = "center"   # center | top | bottom | left | right
animate = true      # slide animation for show/hide (default true)

# Named specials, keyed by name. Each gets its own slot (past the
# numbered range), independent geometry, and flags.
[special.named.term]
width = 1200
height = 800
anchor = "top"
on_demand = false   # parsed but not acted on yet (see limitations)
animate = true      # per-slot slide animation flag (default true)
```

Numbered specials are fixed (`special 1..12`, one shared geometry); named
specials are stable across config reloads (slot assignment follows sorted
name order).

### Known limitations

- Special visibility does **not** persist across compositor restarts.
- `on_demand = true` is parsed but the quake-style command spawn is not
  implemented yet.
- A configured size still respects the client's `min_size` — the compositor
  will not shrink a window below what the client demands.
