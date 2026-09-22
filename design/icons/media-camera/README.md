# Media, camera and attachment assets

44 primary SVGs (37 new, 7 reused), 9 reusable file-badge layers, and 6 Lottie animations. Open [the interactive preview](preview.html) to review motion, static artwork, light/dark rendering, 16–24 px sizes, progress, and the zoom chip. The preview runs offline.

## Scope

The package supplies artwork and motion assets for upcoming features. It does not wire new screens or controls into the app. The optional wand, fit/fill, grid, timer, animated sound slash and lens-spin transition are deferred. The static lens-flip icon is included. Selection uses the existing contact-picker badge; captured-photo flash remains app-drawn.

A slash means **off**, not permission denied. Permission artwork is a camera with a lock. The existing QR permission screen has text and a Settings button, so this new illustration is available for both permission screens without changing either screen here.

## Static assets

- `../outlined/`: editable 24-unit, 1.75-stroke geometry. Use `oi_export` for share, `oi_volume` / `oi_volume_off` for sound, `oi_image_save` for save to gallery, and `oi_image_unavailable` for a cleaned-up file.
- `../controls/`: size-specific, colored or translucent controls. Hero states are 72 dp; play badge 28 dp; shutter states 80 dp; lock target 32 dp; recording indicator 10 dp; unavailable artwork 48 dp; permission artwork 96 dp. Their explicit size/paint requirements override the usual 24-unit outline contract.
- The hero play triangle is centered by its bounding box, then shifted 2 dp right. The disc is black at 40% and has a 1 dp white ring at 30%. It provides translucency; backdrop blur belongs to the host view if desired.
- Both speaker states retain the same speaker and concentric waves. Slashed variants have at least 1.5 units of visible clearance on the separated side.
- Flash on/off/auto share one bolt. The unused camera-off extra has been removed; camera permission keeps the original lens center and radius.
- `oi_camera_flip` has the circular arrows centered on the lens position; its current asset is static.
- Use `oi_camera` at 20 dp for the live-preview tile, `ic_media_play_badge` at 28 dp beside duration, and `oi_film` at 14 dp in preview lines.

## Two-tone file glyphs

`ic_file_pdf`, `zip`, `doc`, `xls`, `apk`, `audio`, and `generic` are composed defaults with a blue badge and white label. Text is vector geometry with no font dependency.

For per-bubble coloring, stack the matching 28 dp layers from `../controls/file-parts/`:

1. `ic_file_base`: content-color page outline.
2. `ic_file_badge`: bubble-accent fill.
3. `ic_file_label_<kind>`: a contrasting label color, white by default.

Matching Android resources are supplied for every layer. Tint the layers independently. Applying one global `Icon` tint to a composed multicolor drawable would remove its two-tone appearance. Hero controls also need their original colors and alpha preserved.

## Lottie playback

All files are self-contained vector shape layers at 60 fps, with no images, external fonts, expressions or effects. They were rendered with Lottie Web 5.12.2. Android assets are provided, but Android-device playback is not yet verified and no Lottie dependency was added to the app.

| File | Markers / frames | Playback |
| --- | --- | --- |
| `media_play_pause.json` | `play` 0, `play_to_pause` 0–18; `pause` 18, `pause_to_play` 18–36 | 300 ms each direction; two pieces form the triangle and separate into bars; no opacity fade |
| `media_buffering.json` | `loop` 0–72 | Loop at 1.2 s; contains disc + 2 dp arc only; replaces play/pause |
| `media_download_done.json` | `download` 0, `complete` 0–15, `done` 15 | 250 ms; shaft retracts into the tick elbow; progress ring is external |
| `camera_shutter.json` | `idle` 0, `press` 6, `record` 18, `locked` 30, `release` 42–54 | Event-driven state asset; never loop the full timeline |
| `camera_lock.json` | `open` 0, `lock` 0–20, `locked` 20 | Close the shackle, pulse once, then hold |
| `camera_recording.json` | `loop` 0–60 | Breathing red dot, 1 s loop |

State markers have zero duration and name exact poses; transition/loop markers have a duration. A loop begins at its marker and excludes the end frame. After a finite transition, settle explicitly at the destination state frame, since a player's segment end may be exclusive. The preview demonstrates this behavior. See [Lottie marker playback](https://github.com/airbnb/lottie-web/wiki/Markers) and [Android Compose progress control](https://github.com/airbnb/lottie/blob/master/android-compose.md).

For rapid reversal, drive from the current pose rather than restarting at the first frame. Play/pause can reverse along frames 0–18. Shutter can reverse its current state toward idle; a completed locked pose can use release 42–54. Releasing during an unfinished lock transition should reverse from the current frame, not jump to the full locked pose.

Shutter geometry: the 72 dp idle ring grows to 80 dp **in diameter**, keeping its center fixed. The white disc is inset 6 dp from the ring's inner edge and scales to 90% on press. Recording uses a red rounded square. Locked recording adds a white closed padlock inside the red square. It means hands-free recording continues, not pause. Reserve the full 80 dp canvas from the start.

Draw the recording progress arc over the ring track at center `(40,40)`, radius `38`, width `4`, starting at 12 o'clock. Feed actual `elapsed / maxClipLength`; no clip length is baked into the animation. Likewise, draw download progress separately, and play the completion morph only after the transfer succeeds. The preview ring is 36 dp with a 24 dp glyph. If motion is disabled, use the static destination SVG/drawable directly.

## Zoom chip

[control-style.json](control-style.json) records the 32 dp pill, 30% black background and `labelMedium` text. The preview demonstrates label dragging into an expanded slider. Use the camera's supported zoom range and the app typography at integration time; the preview's 1–8 range and 218 dp expanded width are illustrative.

## Project layout and regeneration

In the app repository, ordinary outlines are installed in `design/icons/outlined/`, composed controls in `design/icons/controls/`, Lottie sources in `design/icons/lottie/`, and this guide/preview in `design/icons/media-camera/`. Android drawables are in `res/drawable/`; Lottie JSONs are in `res/raw/`.

Run from the repository root:

```sh
python3 tools/scripts/build_media_icon_assets.py
```

This uses the existing `svg2vd.py`, preserves control colors/opacity and intrinsic dimensions, converts the file-badge layers, and copies Lottie sources into `res/raw/`. Reused icons remain managed by the outlined-icon workflow.

## Verification

All SVGs and Android XMLs parse, and all 53 drawable resources plus six raw animations compile with Android AAPT2; outlined icons were checked for visible margins and rendered on light/dark backgrounds at 16, 20 and 24 px. All six Lotties were rendered at endpoints and intermediate frames with no player errors. Preview checks covered rapid play reversal, release during shutter locking, repeated download completion, lock completion, zoom adjustment and filtering. These checks validate the supplied assets and preview, not unimplemented Android screen behavior.
