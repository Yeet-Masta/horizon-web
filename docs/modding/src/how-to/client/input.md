# Pressing gameplay key mappings

A client mod can press the player's vanilla gameplay mappings as their keys do: hold one down and let
it go, or click it. The game then acts through its own key handling, with every check it makes for the
player's keys, so the server sees only the packets of a normal client, at the vanilla moments.

```rust
// Hold sprint, and attack once on the next tick.
host.set_mapping_down("key.sprint", true)?;
host.click_mapping("key.attack")?;
```

`ClientHost::set_mapping_down(key, down)` sets the held state (`KeyMapping.setDown`), without the click a
press adds. `ClientHost::click_mapping(key)` adds one click (`KeyMapping.click`), without holding the
mapping. A press of the key itself does both; a mod that wants a held attack that also starts at once
calls both.

Queued actions are taken oldest first, at most **64 entries per queue per drain**, with at most
**1024 pending entries**. A burst spills into later drains, normally later client ticks, rather than
all acting on the next tick. Hotbar selection can drain more than once per tick, so its limit is per
drain, not per tick. A full queue refuses the new entry, logs its first refusal once for that queue's
lifetime, and the call returns normally (`Ok(())`), like a lost physical press. Clearing pending
input does not reset that diagnostic. Physical input using these queues has the same limits; ordinary
input below the limits keeps its order and timing.

`key` is a vanilla mapping of the movement, gameplay or inventory category: `key.forward`, `key.left`,
`key.back`, `key.right`, `key.jump`, `key.sneak`, `key.sprint`, `key.attack`, `key.use`, `key.pickItem`,
`key.drop`, `key.inventory`, `key.swapOffhand` and `key.hotbar.1` to `key.hotbar.9`. Both return
`Result<(), MappingError>`:

| Error | When |
|---|---|
| `MappingError::NoWorld` | no player is in a world |
| `MappingError::Unknown { key }` | no vanilla mapping has that name (`"nope"`) |
| `MappingError::NotGameplay { key }` | a vanilla mapping of another category (`key.screenshot`, `key.fullscreen`, `key.chat`...) |

## What the game does with them

The call only sets the held flag or adds the click. The client tick's key step (`handleKeybinds`) does
the rest:

- Attack, use and pick clicks are taken only with no screen or overlay open. The miss cooldown, busy
  hands and the item checks of `startAttack` and `startUseItem` apply, an entity attack respects the
  item's attack range, and the target is the one under the crosshair.
- An attack on a block sends the break start with its prediction sequence, then the punch packet; an
  attack on nothing sends the punch packet and sets the miss cooldown to 10 ticks (not in creative).
  With the attack mapping not held, the same tick lets go of the break and clears the cooldown, as a
  tap between two ticks does: hold it (`set_mapping_down("key.attack", true)`) for a break that goes on.
- A held use repeats every four ticks and does nothing more while the item is in use; letting go
  releases it (a bow shoots).
- A click on an inventory key acts as its key's press: a hotbar slot is selected on a subsequent tick, and
  drop, the inventory and the hand swap act as they do for the key.
- Sneak, sprint, attack and use toggle when their toggle option is on: `true` flips them and `false`
  does nothing, as `ToggleKeyMapping.setDown`.

With a screen or overlay open, a click is dropped and a press holds nothing. Opening one lets go of every
mapping a mod holds, as `KeyMapping.releaseAll` does; hold it again once the screen is closed.

`interaction_key_mapping_triggered` fires at the attack or use as it does for the player's click, and
its cancel and swing work the same. `key_mapping_pre` is not asked: it is about physical key and mouse
button events. `ClientHost::mapping(name)` reads a held mapping as down, whoever holds it.

The game has no bare swing to add: in 26.3 a swing sends nothing of its own, and the attack mapping
produces the swing and its packet.

## Shaping gameplay controller sticks (native)

Use `Hooks::controller_move` and `Hooks::controller_look` to replace a native mod's raw stick
response without replacing engine functions. Returning `None` keeps Horizon's stock calculation;
the first mod returning `Some` supplies the answer. Both hooks run after the existing controller,
screen, focus and radial-menu guards. They see non-centered `[right, up]` axes in `[-1, 1]` before
the gameplay dead zones, not keyboard or mouse input. Neither changes menu navigation or bindings.

- `controller_move` runs when the on-foot client samples movement for a player tick. It receives the
  configured dead zone and the most recent advancing frame's elapsed seconds; several ticks can
  sample the same frame. Return `ControllerMoveOverride::new([left, forward], bypass_square_movement)`.
  The game bounds the vector to unit length; `[0.0, 0.0]` suppresses the stick, but not held keys.
  The optional bypass affects only the selected analog direction for this tick. The game still
  applies 0.98, item-use and crouch multipliers. If a later `movement_input` hook replaces the
  direction, the square-movement opt-out is discarded. Mounted movement uses the stock path.
- `controller_look` runs in the frame after mouse look. `event.input` contains the raw axes, applied
  looking sensitivity and inversion, the configured dead zone, elapsed seconds (capped at 0.1 as in
  stock look), and whether a spyglass is in use in first person. Return `[yaw, pitch]` in degrees,
  positive right and down, for that frame. The mod applies whichever sensitivity, inversion and scope
  slowdown it wants. The game clamps the result to ±180 degrees per axis, then applies cinematic
  smoothing and turns the view through its ordinary camera path. Centering the stick still lets
  cinematic turns ease out without calling the hook.

Both hooks discard a non-finite answer and use stock shaping. Their `external_server` flag refers to
the current multiplayer connection, not the integrated world's in-memory protocol session. A native
mod can return `None` on external servers to keep stock movement, or remap which digital direction
flags the stick selects there. External movement still uses full-speed digital directions and the
ordinary input packets; a stick override cannot enable analog movement or skip square shaping on
an external server.
