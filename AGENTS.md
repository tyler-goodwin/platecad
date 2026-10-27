# AGENTS.md

Guide for coding agents working in this repo. For the full design, read
[architecture/README.md](architecture/README.md); for ideas not yet scheduled,
[architecture/improvements.md](architecture/improvements.md).

## What this is

Plate CAD is a code-first CAD playground. You write a model in TypeScript
(`models/*.ts`, using the DSL in `dsl/cad.ts`). Rust evaluates it with
rustyscript (V8), builds B-rep geometry with OpenCascade, and shows it in a three-d window that hot-reloads on save. It can
also export a 3MF for slicers, and `plate init` starts new projects.

```
models/*.ts --(cad-script)--> ModelViews IR --(Kernel)--> TessellatedScene --> viewer | 3MF
```

## Commands

```sh
cargo test --workspace                                # the main way to verify anything
cargo run -p platecad -- models/bookshelf.ts           # opens the viewer window; a folder works too
cargo run -p platecad -- export models/bookshelf.ts [-o out.3mf] [--view auto|print|assembled] [--quality draft|normal|fine]  # headless
cargo run -p platecad -- init path/to/new-project      # writes printer.toml, dsl/cad.ts, a starter model
```

- The first build compiles V8 and takes a while. Later builds are fast.
- The OCC kernel links a prebuilt static OCCT cached under `target/`. If a
  build fails in `cadrum`'s build script ("max_headers_size"), follow
  "Building with OCC" in architecture/README.md.
- A `LNK4098 libcmt.lib` linker warning on Windows is expected; it comes
  from V8.
- `cargo test` prints "future incompatibility" notes for `nom`/`quick-xml`.
  These come from dependencies; ignore them.

## Layout

| Path | Role |
|---|---|
| `crates/cad-ir` | Serde IR shared by everything: `ModelSpec`, `Views<T>`/`ModelViews`, `TessellatedScene`, `ScenePart`, `Rgb`. No kernel or renderer types. |
| `crates/cad-kernel` | `Kernel` trait, `KernelError`, and shared `pack` and `validate_polygon`. |
| `crates/cad-kernel-occ` | The kernel: OpenCascade via cadrum. Spec tree to solids (`build_solids`, with the solid cache), then tessellation (normals, colors, edges, part ranges). `tests/geometry.rs` checks it against exact volumes and bounds. |
| `crates/cad-script` | `ScriptHost` trait and the rustyscript host: sandboxed imports and dependency tracking. |
| `crates/cad-view` | `SceneUpdate`, view selection logic, viewer protocol (`ViewerMessage`/`ViewerCommand`), `ViewportApp` trait (no GPU code). |
| `crates/cad-view-threed` | three-d + egui window. |
| `crates/cad-export` | `TessellatedScene` to 3MF (welded, watertight, one object per part). |
| `crates/platecad` | The `plate` binary plus library: CLI (clap), `Pipeline`, `run_session` (viewer commands, file list, watcher), `export_3mf`. Most integration tests live here. |
| `dsl/cad.ts` | The TS DSL (also built into the binary for `plate init`): `rect`, `roundedRect`, `slot`, `polygon`, `circle`, `cutout`, `extrude`, `revolve`, `box`, `cube`, `sphere`, `cylinder`, `cone`, `torus`, `translate`, `rotate`, `group`, `union`, `difference`, `intersect`, `fillet`, `chamfer`, `color`, `part`, `arrange`, `bounds` (a kernel query), and `pipeline` with its `step.*` helpers. |
| `models/` | Example models (`cube`, `square`, `bookshelf`, `coasters`, `desk_organizer`, `rounded_box`, `cat`). The user edits these live. |
| `assets/` | The app icon (`plate.ico`, drawn by `make_icon.py`; rerun it after editing the design). `build.rs` embeds it in `plate.exe`. |
| `printer.toml` | Optional bed size, used by `arrange` in print views. Watched like an import. |
| `architecture/` | Living design docs. Update them with every change. |

## Rules of the house

- **Verify with tests, not the window.** Don't launch `plate` to check
  rendering, and don't take screenshots of it. Pull logic out of GPU and
  window code into pure functions and test those. Examples are `framing`,
  `edge_transform`, `vertex_colors`, `part_list`, `ViewSelection` and
  `needs_reframe`. Headless CLI runs (`export`, `init`) are fine.
- **Commit each finished feature on its own,** with tests green and the
  matching `architecture/README.md` update in the same commit. End commit
  messages with the attribution trailer the harness provides.
- **Millimeters everywhere, +Z up.** Slicers assume mm.
- **The IR is the seam.** Crates talk only through `cad-ir` and the traits.
  Never import cadrum, three-d or rustyscript types across crate boundaries.
- **Model files are user territory.** The user edits `models/*.ts` while you
  work, so expect changes you didn't make; don't revert them. Tests of model
  files check structure only (e.g. "builds", "parts named X"). Exact-value
  tests use inline TS sources or explicit parameters.

## Adding a `ModelSpec` node (checklist)

Every new node touches the same places. Missing one usually shows up as a
non-exhaustive `match` error, but not always.

1. `cad-ir`: the variant, with a doc comment, plus a JSON test matching the
   TS shape.
2. The kernel, `cad-kernel-occ`: `build_uncached` for solids and
   transforms, `build_profile` if it's a 2D profile or can wrap one, and
   `SolidCache::key` if it's slow enough to cache. Add a case with its exact
   volume and bounds to `tests/geometry.rs`. Shared validation goes in
   `cad-kernel` (e.g. `validate_primitive`).
3. `dsl/cad.ts`: the `*Spec` type, the `ModelSpec` union (and `ProfileSpec`
   if it applies), and a builder function. Validate input in TS with a
   message that names the function, e.g. `rotate axis must be…`.
4. Tests in each layer: kernel geometry, DSL output and errors (in
   `cad-script`), and end to end in `platecad` if it affects models or export.
5. The IR table in `architecture/README.md`.

## Gotchas learned the hard way

- **Each eval gets a fresh rustyscript `Runtime` on purpose.** deno caches
  modules by URL, so a reused runtime serves stale imports after an edit.
- **TS drops imports whose bindings are never used.** Those files are never
  loaded, so they aren't dependencies. A test that needs an import to load
  has to use the binding.
- **Hot reload follows dependencies, not folders.** `Pipeline::dependencies`
  is replaced after a successful eval and extended after a failed one. The
  watcher watches each dependency's folder non-recursively, which survives
  editors that save via temp-file-and-rename.
- **Watcher tests use a real `notify` watcher on temp dirs,** with a 300 ms
  settle before editing and timeouts of 5 s (reload expected) or 600 ms (no
  reload expected). If you touch the watcher, run them several times to
  check for flakiness.
- **Two mesh forms.** The viewer mesh is de-indexed per (position, normal),
  so corners stay crisp. Export must weld vertices by position, or slicers
  see non-manifold edges.
- **OCC's bounding box is padded,** and loose around curved faces. Don't
  use `Solid::bounding_box` to place things: `arrange` measures a fine mesh
  (`tight_bounds`), and edge selectors use sampled edges (`edge_bounds`).
- **Meshing sticks to the shape.** cadrum's mesher stores the triangulation
  on the OCC shape, and a later coarser mesh keeps the finer one. Mesh a
  deep copy (`solid.clone()`) for anything but the final tessellation.
- **Bare profiles become 0.01 mm plates** (`FLAT_THICKNESS`), anywhere they
  appear outside a sweep, because OCC meshes solids only.
- **Mesh bounds of curved shapes are inscribed polygons,** so compare them
  with the tessellation tolerance (0.01 mm), not 1e-4. Inscribed holes leave
  slightly more material: volumes can land a little above *or* below the
  exact value.
- **Exact pins.** `serde` and `serde_json` are pinned exactly in the
  workspace `Cargo.toml` for rustyscript compatibility. Don't loosen them
  casually.
- **Editing files on Windows.**
  - Python defaults to cp1252. Open files with `encoding="utf-8"`, and keep
    Rust escapes like `\u{..}` out of Python string literals.
  - Bash heredocs containing complex quoting can fail to parse. Put larger
    edit scripts in a file instead.
- **Rust raw strings.** A raw string holding a hex color like `"#ff0000"`
  needs `r##"..."##`, because `"#` ends a `r#"..."#` string.
- **`cad_ir` in `platecad`.** It's a normal dependency there, used outside
  tests too. Test-only crates (`tempfile`, `zip`) are dev-dependencies.
