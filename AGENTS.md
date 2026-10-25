# AGENTS.md

Guide for coding agents working in this repo. For the full design, read
[architecture/README.md](architecture/README.md); for ideas not yet scheduled,
[architecture/improvements.md](architecture/improvements.md).

## What this is

`caddy` is a code-first CAD playground. You write a model in TypeScript
(`models/*.ts`, using the DSL in `dsl/cad.ts`). Rust evaluates it with
rustyscript (V8), builds B-rep geometry with OpenCascade (or the truck
kernel), and shows it in a three-d window that hot-reloads on save. It can
also export a 3MF for slicers, and `caddy init` starts new projects.

```
models/*.ts --(cad-script)--> ModelViews IR --(Kernel)--> TessellatedScene --> viewer | 3MF
```

## Commands

```sh
cargo test --workspace                                # the main way to verify anything
cargo run -p caddy -- models/bookshelf.ts           # opens the viewer window (OCC kernel by default); a folder works too
cargo run -p caddy -- export models/bookshelf.ts [-o out.3mf] [--view auto|print|assembled]  # headless
cargo run -p caddy -- init path/to/new-project      # writes printer.toml, dsl/cad.ts, a starter model
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
| `crates/cad-kernel-truck` | The real kernel: spec tree to bodies (`build_bodies`), then tessellation (normals, colors, edges, part ranges). |
| `crates/cad-kernel-occ` | OpenCascade kernel via cadrum. `tests/conformance.rs` keeps it in step with truck. |
| `crates/cad-script` | `ScriptHost` trait and the rustyscript host: sandboxed imports and dependency tracking. |
| `crates/cad-view` | `SceneUpdate`, view selection logic, viewer protocol (`ViewerMessage`/`ViewerCommand`), `ViewportApp` trait (no GPU code). |
| `crates/cad-view-threed` | three-d + egui window. |
| `crates/cad-export` | `TessellatedScene` to 3MF (welded, watertight, one object per part). |
| `crates/caddy` | The `caddy` binary plus library: CLI (clap), `Pipeline`, `run_session` (viewer commands, file list, watcher), `export_3mf`. Most integration tests live here. |
| `dsl/cad.ts` | The TS DSL (also built into the binary for `caddy init`): `rect`, `polygon`, `circle`, `cutout`, `extrude`, `revolve`, `box`, `cube`, `translate`, `rotate`, `group`, `union`, `difference`, `intersect`, `fillet`, `chamfer`, `color`, `part`, `arrange`. |
| `models/` | Example models (`cube`, `square`, `bookshelf`, `coasters`, `desk_organizer`, `rounded_box`). The user edits these live. |
| `printer.toml` | Optional bed size, used by `arrange` in print views. Watched like an import. |
| `architecture/` | Living design docs. Update them with every change. |

## Rules of the house

- **Verify with tests, not the window.** Don't launch `caddy` to check
  rendering, and don't take screenshots of it. Pull logic out of GPU and
  window code into pure functions and test those. Examples are `framing`,
  `edge_transform`, `vertex_colors`, `part_list`, `ViewSelection` and
  `needs_reframe`. Headless CLI runs (`export`, `init`) are fine.
- **Commit each finished feature on its own,** with tests green and the
  matching `architecture/README.md` update in the same commit. End commit
  messages with the attribution trailer the harness provides.
- **Millimeters everywhere, +Z up.** Slicers assume mm.
- **The IR is the seam.** Crates talk only through `cad-ir` and the traits.
  Never import truck, three-d or rustyscript types across crate boundaries.
- **Model files are user territory.** The user edits `models/*.ts` while you
  work, so expect changes you didn't make; don't revert them. Tests of model
  files check structure only (e.g. "builds", "parts named X"). Exact-value
  tests use inline TS sources or explicit parameters.

## Adding a `ModelSpec` node (checklist)

Every new node touches the same places. Missing one usually shows up as a
non-exhaustive `match` error, but not always.

1. `cad-ir`: the variant, with a doc comment, plus a JSON test matching the
   TS shape.
2. Both kernels: `cad-kernel-truck` and `cad-kernel-occ` (add a
   conformance case too). In `cad-kernel-truck`:
   - `build_bodies` (solids and transforms);
   - `build_face` if it can wrap a 2D profile;
   - `profile_style` if it carries style (color or part name) that an
     extrusion should inherit.
3. `dsl/cad.ts`: the `*Spec` type, the `ModelSpec` union (and `ProfileSpec`
   if it applies), and a builder function. Validate input in TS with a
   message that names the function, e.g. `rotate axis must be…`.
4. Tests in each layer: kernel geometry, DSL output and errors (in
   `cad-script`), and end to end in `caddy` if it affects models or export.
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
- **Boolean results get explicit curves** (`explicit_curves`). truck's
  implicit intersection curves can hang tessellation after a transform.
  Never keep `Curve::IntersectionCurve` around, and build anything that
  could hang under a timeout in tests (`build_within`).
- **OCC is the default kernel.** truck can't fillet, and its booleans fail
  on coplanar faces. Any new IR node needs a conformance case in
  `crates/cad-kernel-occ/tests/conformance.rs` if both kernels support it,
  or an "unsupported" error in truck if only OCC does.
- **Booleans fail on coplanar faces (truck).** truck-shapeops returns `None` or
  panics when faces line up exactly, so the kernel wraps it in
  `catch_unwind` and returns a hinted error. Model subtractively, with
  cutters that overshoot the faces they cut through.
- **Profiles are wound counter-clockwise about +Z.** Sweeps must face the
  face along the sweep direction (`facing`), or the solid comes out
  inside-out with negative volume. The export volume tests catch this.
- **Bounds come from sampled edges plus tessellation vertices** (64
  segments per curved edge). That's exact for extrusions and within 0.01 mm
  for bulging surfaces like a torus. Mesh bounds of curved shapes are inscribed
  polygons, so compare them with the tessellation tolerance (0.01 mm), not
  1e-4. Inscribed holes leave slightly more material: volumes can land a
  little above *or* below the exact value.
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
- **`cad_ir` in `caddy`.** It's a normal dependency there, used outside
  tests too. Test-only crates (`tempfile`, `zip`) are dev-dependencies.
