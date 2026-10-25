# Potential improvements

Ideas to look at next time. They aren't scheduled; the committed plan is the
roadmap in [README.md](README.md). Each entry gives what's wrong today, then
the improvement.

## Modeling

- **Coplanar booleans.** Booleans exist but fail whenever faces line up
  exactly (see README, "Booleans and coplanar faces"). Options: detect
  coplanar contact and nudge a tool by a sub-tolerance epsilon
  automatically, try a newer truck, or use an OCC backend whose booleans
  handle coplanar faces.
- **Quiet boolean panics.** A caught truck panic still prints to stderr.
  A process-wide panic hook that silences panics from the kernel's boolean
  calls would keep the console clean.
- **More profiles.** `Rect`, `Polygon`, `Circle` and `Cutout` (holes)
  exist. Add polylines with arcs, rounded rectangles, and slots.
- **Hide seam edges.** A cylinder shows the seam edges truck puts on its
  side face. Skip edges whose two faces meet smoothly (tangent normals).
- **Slow curved booleans (truck only).** Subtracting a cylinder takes about
  3 s in a debug build with `--kernel truck`; OCC, the default, does it in
  milliseconds. Matters only if truck stays around.
- **Geometry queries from scripts.** Scripts can't measure anything, which
  is why `arrange` lives in the kernel. A hybrid query op such as
  `bounds(shape)` would let layouts and fits be written in TS.
- **Cheaper bounds.** Bounds tessellate each shell an extra time (on top
  of the tessellation for display). Cache that tessellation, or sample
  surfaces directly.

## OCC kernel (spike follow-ups)

- **Bare 2D profiles in OCC.** These fall back to truck. cadrum can't build
  a planar face from edge loops or mesh a lone face; that needs OCCT's
  `BRepBuilderAPI_MakeFace` exposed.
- **More OCC features.** Loft, sweep along a path, shell (hollowing) and
  STEP export are in cadrum but not exposed.
- **Smarter edge selection.** Selectors can't tell a seam edge (a
  cylinder's side seam, a revolve's profile edge) from a real one, and
  can't pick by length, by face, or by index. OCC errors on filleting a
  seam; the error says so.
- **Reproducible OCCT fetch.** The documented curl workaround is manual.
  Vendor a fetch script, or switch to cadrum's `source` build (official
  sources; needs CMake).
- **Drop truck's workarounds** (explicit curves, coplanar hints) if OCC
  becomes the only kernel.
- **Parallel booleans.** cadrum runs each boolean as one
  `BOPAlgo_CellsBuilder` pass (`builder_cells` in its `src/ffi.cpp`) without
  calling `SetRunParallel(true)`, so a big union uses one core. It's a
  one-line change, but inside cadrum's C++, so it needs a `[patch]` or
  vendored fork (or an upstream PR). `SetUseOBB(true)` (oriented bounding
  boxes, to skip non-touching face pairs sooner) is worth trying at the
  same time. Measure on `cat.ts`: its fused fur is a single heavy
  `CellsBuilder` pass.

## Print workflow

- **Smarter packing.** `Arrange` packs in input order, row by row, and never
  rotates parts. It could try turning a part 90 degrees to fit, sort parts
  by size, or use a real 2D bin-packing algorithm.
- **Multi-plate 3MF.** Export puts every plate's parts into one coordinate
  space. Bambu/Orca/Prusa have their own metadata for separate plates.
- **Auto-orient.** Lay a part on its largest planar face automatically
  instead of hand-building print orientations.
- **Filament mapping.** 3MF export writes core-spec colors only. Merge
  colors into N filament slots, and add slicer-specific extruder metadata
  (Bambu/Orca `model_settings.config`, Prusa `Slic3r_PE_model.config`) so
  parts arrive pre-assigned.
- **Open surfaces in export.** A 2D `rect` exports as a single-face object,
  which isn't watertight and slicers will reject. `ExportObject::is_closed()`
  already detects this, so warn on or skip non-closed parts at export.
- **Export from the viewer.** Export is CLI-only. Add an Export 3MF button
  to the overlay.
- **Units in the UI.** Show model dimensions in mm in the overlay, which
  catches cm/inch mistakes early.

## Script host

- **Better TS errors.** Errors print the rustyscript message. Map them to
  file:line with a source excerpt, and type-check with `tsc` on demand.
- **Faster reload.** A fresh runtime on every eval costs about 15–20 ms.
  Measure it before optimizing, and consider snapshotting the DSL module.
- **Bare imports.** Support an import map for `import { box } from "cad"`
  instead of relative `../dsl/cad.ts`.
- **Eval timeouts.** An infinite loop in a model hangs the reload thread.
  Run evals with a timeout or a V8 terminate.

## Viewer

- **Grid and axes.** There's no ground reference. Draw a mm grid and an XYZ
  triad, plus the print bed outline in Print view.
- **Camera presets.** Front, top, iso, and fit-to-view, on keyboard
  shortcuts.
- **Part picking.** The overlay lists parts, but you can't select one.
  Add click-to-highlight in the viewport and in the list.
- **Measurements.** Measure distances and bounds of a selected part.
- **Wireframe and transparency toggles.**

## Engineering

- **CI.** Run `cargo test --workspace` on push. V8 makes the first build
  slow, so cache `target/`.
- **Windows linker warning.** `LNK4098 libcmt.lib` comes from linking V8.
  Silence or fix it once it matters.
- **Exact dependency pins.** `serde`/`serde_json` are pinned exactly in the
  workspace for rustyscript compatibility. Revisit when rustyscript updates.
- **Golden-image tests.** An offscreen render plus image diff would catch
  regressions in lighting, colors and edges without a person looking.
