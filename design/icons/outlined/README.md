# Rounded outlined icons

Editable SVG sources for the rounded outlined icon family. Each icon uses a 24-unit
canvas, 1.75-unit stroke, round ends/joins, and at least 1.5 units of visible
margin. Ordinary corners use a 3-unit quadratic setback; directional tips and
small details use smaller bends. Geometry is baked into the SVG coordinates.

[Preview the set](../outlined-preview.svg).

## Android resources

Convert the SVGs in this directory from the repository root:

```sh
python3 tools/scripts/svg2vd.py --from design/icons/outlined --force
```

The converter only reads this directory, not its `morph` subdirectory. The
`oi_check` resource serves Android notification actions; `oi_code` serves the
resource-based settings list. Compose controls use the matching morph glyphs.

| Icon | Source |
| --- | --- |
| Broom | [oi_broom.svg](oi_broom.svg) |
| Camera | [oi_camera.svg](oi_camera.svg) |
| Camera permission | [oi_camera_permission.svg](oi_camera_permission.svg) |
| Check | [oi_check.svg](oi_check.svg) |
| Code | [oi_code.svg](oi_code.svg) |
| Copy | [oi_copy.svg](oi_copy.svg) |
| Edit | [oi_edit.svg](oi_edit.svg) |
| Export | [oi_export.svg](oi_export.svg) |
| External link | [oi_external_link.svg](oi_external_link.svg) |
| File attachment | [oi_file_attachment.svg](oi_file_attachment.svg) |
| Film | [oi_film.svg](oi_film.svg) |
| Flash | [oi_flash.svg](oi_flash.svg) |
| Flash auto | [oi_flash_auto.svg](oi_flash_auto.svg) |
| Flash off | [oi_flash_off.svg](oi_flash_off.svg) |
| Flip lens | [oi_camera_flip.svg](oi_camera_flip.svg) |
| Forward | [oi_forward.svg](oi_forward.svg) |
| Gallery | [oi_gallery.svg](oi_gallery.svg) |
| Image | [oi_image.svg](oi_image.svg) |
| Info | [oi_info.svg](oi_info.svg) |
| Lock | [oi_lock.svg](oi_lock.svg) |
| Lock open | [oi_lock_open.svg](oi_lock_open.svg) |
| Media unavailable | [oi_image_unavailable.svg](oi_image_unavailable.svg) |
| Paperclip | [oi_paperclip.svg](oi_paperclip.svg) |
| Pin | [oi_pin.svg](oi_pin.svg) |
| Replay | [oi_replay.svg](oi_replay.svg) |
| Reply | [oi_reply.svg](oi_reply.svg) |
| Save to gallery | [oi_image_save.svg](oi_image_save.svg) |
| Search | [oi_search.svg](oi_search.svg) |
| Settings | [oi_settings.svg](oi_settings.svg) |
| Torch | [oi_torch.svg](oi_torch.svg) |
| Trash | [oi_trash.svg](oi_trash.svg) |
| Unpin | [oi_unpin.svg](oi_unpin.svg) |
| User | [oi_user.svg](oi_user.svg) |
| User circle | [oi_user_circle.svg](oi_user_circle.svg) |
| Volume | [oi_volume.svg](oi_volume.svg) |
| Volume off | [oi_volume_off.svg](oi_volume_off.svg) |

Reply and forward are reflected copies of the same geometry. External link,
search, and paperclip retain their existing centerlines. Settings matches the
skill's current reference geometry and margin.

## Morph glyphs

`MorphIcon.kt` uses two five-vertex polylines, each with three animated interior
corner setbacks. A missing contour collapses to the center. The SVGs below are
editable endpoint references with the same geometry, including baked rotations.
Keep their coordinates and `MorphGlyph` definitions aligned when changing a glyph;
also update the root check/code SVGs when those glyphs change.

Close and plus share the same stroke lengths and differ only by rotation. The
close's diagonal bounds are intentionally smaller so it does not look oversized.

[Back](morph/oi_back.svg) · [Chevron left](morph/oi_chevron_left.svg) ·
[Chevron right](morph/oi_chevron_right.svg) · [Chevron up](morph/oi_chevron_up.svg) ·
[Chevron down](morph/oi_chevron_down.svg) · [Close](morph/oi_close.svg) ·
[Plus](morph/oi_plus.svg) · [Check](morph/oi_check.svg) · [Code](morph/oi_code.svg) ·
[Play](morph/oi_play.svg) · [Pause](morph/oi_pause.svg).

Both `MorphIcon` overloads accept `strokeWidth: Dp = 1.75.dp`. This thickness is
independent of `Modifier.size`; geometry makes room for the chosen stroke while
retaining proportional outer padding. Top bars use `TopBarMorphIcon`, whose
shared `TopBarIconDefaults` set the size to 22 dp and stroke to 2.25 dp. Its glyph
and hoisted-state overloads preserve the same morph behavior, tint, and semantics.
Change those defaults to adjust all top-bar morph icons together, including
navigation, search/selection close buttons, and search result arrows. Tiny
badge glyphs use 1.25 dp. The SVG references represent 24 dp icons at the default
1.75 dp weight. A layer transform such as `graphicsLayer { scaleX = ... }` still
scales the entire rendered icon, including its stroke.

```kotlin
MorphIcon(
    MorphGlyph.Back,
    description = "Back",
    modifier = Modifier.size(24.dp),
    strokeWidth = 2.dp,
)
```

## Static variants

Pin/unpin and volume/volume-off are static SVGs in this directory, not morph
glyphs. Each pair shares identical base coordinates and scale. The unpin slash
connects to the left side of the pin, with 1.5 units of visible clearance on the
opposite side.
Volume and volume-off share concentric circular waves. Volume-off adds a
diagonal slash, connected on the left with the same 1.5-unit visible clearance
on the opposite side. Volume names describe the depicted sound state, so
choose the action label independently at the call site. Trash uses a plain open
bin with a rounded handle.

## Media and camera controls

The [media/camera asset guide](../media-camera/README.md) covers composed controls,
file-type badges, Lottie markers, progress rings, and the interactive preview.
Their size and color requirements are separate from the ordinary outlined SVGs.
Rebuild those platform assets with `python3 tools/scripts/build_media_icon_assets.py`.
Camera permission uses a camera with a lock and keeps the original centered lens.
The unused camera-off extra is deferred pending a better alternative.

## Deferred families

Bells, clear-list, sticker, and message variants remain outside this pass.
Future user variants should retain the base geometry of `oi_user.svg`.
Their redesign should use one identical base geometry across variants, reserving
room for secondary marks across the entire family. Keep future trash variants
aligned with `oi_trash.svg`. Do not fit each variant independently and
inadvertently resize its base. `DrawableIcon`'s existing overflow behavior remains
available while those assets are in use.
