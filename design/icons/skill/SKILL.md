---
name: rounded-stroke-icons
description: "Create and adapt app icons in the user's rounded stroke style from words, reference images, or SVGs, including unique combinations and small variations. Deliver editable 24-unit SVGs with 1.75 thickness, nominal roundness 3, and a 1.5-unit minimum visible margin. Use for this icon family or converting outlined icons to consistent stroked versions."
---

# Rounded Stroke Icons

Create clear, compact, rounded icons for the user's app. The primary output is a real, editable SVG made of stroked centerlines, with its appearance controlled by one stroke-width value. Apply the saved defaults without asking the user to reconfirm them. An explicit user override takes precedence; change the saved defaults only when the user asks to update the skill.

## Locked visual contract

| Property | Default |
| --- | --- |
| Canvas | `viewBox="0 0 24 24"`, `width="24"`, `height="24"` |
| Thickness | `stroke-width="1.75"`, in final 24-unit coordinates |
| Roundness | Nominal **3-unit quadratic corner setback**, adapted locally for small details |
| Margin | **1.5 units minimum from the visible stroke edge** to the canvas edge |
| Paint | `fill="none"`, `stroke="currentColor"` |
| Ends and joins | `stroke-linecap="round"`, `stroke-linejoin="round"` |
| Background | Transparent |
| Slash clearance | **1.5 units minimum of visible space** on the separated side |

### Thickness and margin

With a 1.75-unit stroke, the half-width is 0.875. The default safe centerline envelope is therefore **2.375 through 21.625** on both axes. Its width and height are 19.25. Visible artwork can extend from **1.5 through 22.5**.

Fit the complete icon, including a combination's secondary details, uniformly into this envelope and center it around `(12,12)`. Aim to use the available space on the longer axis. A naturally narrower icon, including a six-tooth gear, can have larger margins on the other axis. Do not stretch one axis just to fill a square. Check the actual stroked silhouette: control-point bounds are not necessarily curve bounds, and caps contribute to visible bounds.

When fitting existing centerline geometry with bounds `(xmin,ymin,xmax,ymax)`, a starting uniform scale is `19.25 / max(xmax-xmin, ymax-ymin)`. Recenter coordinates around `(12,12)`, then set the final stroke width to 1.75 and recheck curves and spacing. Bake scaling into coordinates for the final SVG. Do not crop the viewBox or scale the stroke along with the geometry to fake the requested margin.

### Roundness

The saved value **3** follows the established quadratic-bend convention, not an exact circular radius. On an ordinary right-angle corner, stop 3 units before the vertex and use a quadratic curve through that vertex to a point 3 units along the next edge. For example, a turn at `(5,5)` can be `M8 5 Q5 5 5 8`.

Use actual connected curves to soften both the inner and outer contours. Round caps or joins alone do not produce this treatment on a box drawn from independent straight segments.

For short edges, teeth, notches, and arrow tips, reduce the setback locally so adjacent bends do not collide or erase the symbol. Preserve a recognizable gear tooth or directional tip over imposing 3 units on every tiny corner. Keep tangent transitions smooth; avoid corners whose inner offset becomes a sharp cusp at the chosen thickness. The reference gear demonstrates this adaptation. The arrow uses a smaller tip bend, about 40% of the nominal value, to retain directionality. Circles remain circles.

Apply rounding in the final 24-unit coordinate space. After resizing an existing icon, retune large corners to the nominal 3-unit treatment instead of unintentionally increasing all corner setbacks with the scale.

## Variants and slashes

For state variants such as pin/unpin or sound on/off, keep the same base coordinates, scale, and placement. Reserve space for secondary marks across the family, then fit the family together. Do not independently recenter or resize each variant. Interruptions for a slash trim the original curves; they do not reshape the remaining base.

### Slash convention

- Use a continuous diagonal slash from upper-left to lower-right, with the shared stroke weight and round caps. Fit the entire variant, including the slash, within the visible margin.
- Keep the base connected to the slash on its lower-left side. Separate the upper-right side wherever the slash crosses the base or details. Do not introduce gaps on both sides.
- Leave **at least 1.5 units of visible clearance**, measured perpendicular from the slash's painted edge to the nearest painted edge of the separated geometry, including round caps. A centerline offset is not the visible gap.
- For two 1.75-unit strokes, the separated centerline needs at least `1.5 + 0.875 + 0.875 = 3.25` units of perpendicular distance. For a 45-degree slash on `y = x`, the separated side therefore ends at `x - y >= 3.25 * sqrt(2)`, approximately **4.6**. Remove geometry in the strip `0 < x - y < 4.6`; preserve the connected side. For another slash angle, use perpendicular distance to that line.
- Trim actual paths and arcs. Keep the SVG transparent and editable; do not cover intersections with background-colored paint. Inspect the complete remaining curves for clearance, not only their endpoints.

[Unpin](assets/unpin.svg) and [speaker off](assets/speaker-off.svg) demonstrate the approved connection and spacing. Use these when creating or refining slashed variants.

### Speaker waves

Draw inner and outer waves as concentric circular arcs with a common center and consistent radial spacing, like parts of a ring. Keep the speaker body identical across sound states. The default muted version retains both waves and adds the slash with the clearance above. If a cross variant is explicitly requested, replace both waves with the cross; give it enough separation and size to remain readable.

## Interpret the input

- **Words:** Infer a familiar silhouette from the requested concept and context. A short phrase such as "download", "settings", or "saved cloud" is enough to begin. Ask only if the meaning is materially ambiguous.
- **Reference image:** Inspect the image. Extract the identifying silhouette, openings, orientation, and distinguishing details, then redraw those as clean centerlines. Simplify detail that cannot survive at 24 px. The image guides the design; it does not become a bitmap embedded in the SVG.
- **Reference SVG:** Read and render it. Preserve its identity while reconstructing the stroke skeleton. For a filled outline or compound path, do not merely replace its fill with a stroke: that outlines both borders and creates doubled contours. Source dimensions, clipping, padding, and thickness do not override this family's defaults.
- **Several icons:** Follow whether the user requested separate icons, a set, or one combined symbol. Do not merge a set unless requested.
- **Unique combination:** Identify the role of each concept and integrate them into one readable silhouette. Prefer shared structure, a replaced meaningful feature, or a merged contour. A badge is appropriate when it communicates the requested relationship, but arbitrary overlapping or stacking is not a substitute for a designed combination. Use one shared canvas, weight, and outer margin.
- **Slight variation:** Preserve the original identity and proportions as far as the requested change permits. Change the specified characteristic rather than redesigning unrelated features.

Use the supplied references and local design work. Internet reference hunting or a raster-generation workflow is unnecessary for an ordinary SVG request. Paths, circles, arcs, and simple shapes are all suitable; do not constrain every icon to straight segments.

## Build and inspect

Use [assets/open-in-new.svg](assets/open-in-new.svg) and [assets/settings.svg](assets/settings.svg) as editable style references when useful. [assets/style-reference.png](assets/style-reference.png) shows them together. These assets use the final locked values, superseding the earlier 1.5-weight and larger-margin experiments.

1. Construct the simplest geometry that clearly communicates the requested icon. Favor generous openings, restrained detail, and rounded contours consistent with the references.
2. Fit and round the geometry under the locked contract. In a set, compare optical weight and apparent size across the icons. Avoid duplicate coincident strokes, doubled outlines, and dense intersections that produce dark knots.
3. Write a standalone SVG. Prefer direct paths and basic SVG geometry, one shared stroke style, and concise stable coordinates. Keep it free of external resources, embedded raster images, filters, font dependencies, or unnecessary transforms. Use enough coordinate precision to preserve symmetry and margins; do not over-optimize before checking the shape.
4. Parse the SVG and render it. Inspect **24, 20, and 16 px actual-size renders** as well as an enlarged view; check legibility, inner corners, gaps, clipping, and joins. Check on light and dark backgrounds. Slash gaps and secondary details must remain distinct at the small sizes; an enlarged preview alone is insufficient. A local SVG renderer such as Sharp is appropriate for inspection; rendering to PNG does not change the SVG deliverable. Use available workspace runtimes rather than hardcoded machine-specific paths.
5. Verify `viewBox`, final stroke width, visible margins, slash clearance where applicable, and clean geometry after the last change. Fix defects before presenting the icon. For a combination, make sure both intended concepts remain recognizable at actual size.

No morph is required by default. When the user asks for animation or Kotlin glyph data, group compatible geometries and supply that additional representation. Do not force an unrelated gear or curved icon into the original three-line morph family. Preserve the SVG and shared stroke style.

## Project integration

When adding icons to an existing project, read its icon README and reuse its established names and source locations. Prefer names for the depicted symbol or state, rather than a call-site action that may mean the opposite. Generate matching platform assets with the existing converter and update the catalog or preview when that is part of the established workflow. Preserve unrelated assets and call sites; a standalone icon request does not imply project integration without task or session context authorizing it.

## Deliver

Save each final icon as a clearly named `.svg` in the task's output directory and provide a clickable download plus a visible preview. Include SVG source inline when requested or when a short snippet is useful. For a set, provide individual SVGs and a compact shared preview; a ZIP is useful for a larger batch. Return one polished result per requested icon unless alternatives were requested or a meaningful ambiguity warrants them.

State any material design interpretation briefly, such as smaller local bends for gear teeth or extra side space caused by a narrow silhouette. Do not imply that nominal roundness 3 is an exact radius at every vertex. Keep the response focused on the finished icons and their use.
