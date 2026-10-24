# AGENTS.md

Guide for coding agents working in this repo. For the full design, read
[architecture/README.md](architecture/README.md); for ideas not yet scheduled,
[architecture/improvements.md](architecture/improvements.md).

## What this is

A code-first CAD playground. You write a model in TypeScript (`models/*.ts`,
using the DSL in `dsl/cad.ts`). Rust evaluates it with rustyscript (V8),
builds B-rep geometry with the truck kernel, and shows it in a three-d
window that hot-reloads on save. It can also export a 3MF for slicers.

```
models/*.ts --(cad-script)--> ModelViews IR --(Kernel)--> TessellatedScene --> viewer | 3MF
```

## Commands

```sh
cargo test --workspace                                # the main way to verify anything
cargo run -p cad-app -- models/bookshelf.ts           # opens the viewer window
cargo run -p cad-app -- models/bookshelf.ts --export out.3mf [--view auto|print|assembled]  # headless
```

- The first build compiles V8 and takes a while. Later builds are fast.
- A `LNK4098 libcmt.lib` linker warning on Windows is expected; it comes
  from V8.
- `cargo test` prints "future incompatibility" notes for `nom`/`quick-xml`.
  These come from dependencies; ignore them.

## Layout

| Path | Role |
|---|---|
| `crates/cad-ir` | Serde IR shared by everything: `ModelSpec`, `Views<T>`/`ModelViews`, `TessellatedScene`, `ScenePart`, `Rgb`. No kernel or renderer types. |
| `crates/cad-kernel` | `Kernel` trait and `KernelError`. |
| `crates/cad-kernel-truck` | The real kernel: spec tree to bodies (`build_bodies`), then tessellation (normals, colors, edges, part ranges). |
| `crates/cad-kernel-occ` | OpenCascade stub that always returns `Unsupported`. |
| `crates/cad-script` | `ScriptHost` trait and the rustyscript host: sandboxed imports and dependency tracking. |
| `crates/cad-view` | `SceneUpdate`, view selection logic, `ViewportApp` trait (no GPU code). |
| `crates/cad-view-threed` | three-d + egui window. |
| `crates/cad-export` | `TessellatedScene` to 3MF (welded, watertight, one object per part). |
| `crates/cad-app` | Binary plus library: CLI (clap), `Pipeline`, file watcher, `export_3mf`. Most integration tests live here. |
| `dsl/cad.ts` | The TS DSL: `rect`, `polygon`, `circle`, `cutout`, `extrude`, `box`, `cube`, `translate`, `rotate`, `group`, `union`, `difference`, `intersect`, `color`, `part`, `arrange`. |
| `models/` | Example models (`cube`, `square`, `bookshelf`, `coasters`). The user edits these live. |
| `printer.toml` | Optional bed size, used by `arrange` in print views. Watched like an import. |
| `architecture/` | Living design docs. Update them with every change. |

## Rules of the house

- **Verify with tests, not the window.** Don't launch `cad-app` to check
  rendering, and don't take screenshots of it. Pull logic out of GPU and
  window code into pure functions and test those. Examples are `framing`,
  `edge_transform`, `vertex_colors`, `part_list`, `ViewSelection` and
  `needs_reframe`. Headless CLI runs (`--export`) are fine.
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
2. `cad-kernel-truck`:
   - `build_bodies` (solids and transforms);
   - `build_face` if it can wrap a 2D profile;
   - `profile_style` if it carries style (color or part name) that an
     extrusion should inherit.
3. `dsl/cad.ts`: the `*Spec` type, the `ModelSpec` union (and `ProfileSpec`
   if it applies), and a builder function. Validate input in TS with a
   message that names the function, e.g. `rotate axis must be…`.
4. Tests in each layer: kernel geometry, DSL output and errors (in
   `cad-script`), and end to end in `cad-app` if it affects models or export.
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
- **Booleans fail on coplanar faces.** truck-shapeops returns `None` or
  panics when faces line up exactly, so the kernel wraps it in
  `catch_unwind` and returns a hinted error. Model subtractively, with
  cutters that overshoot the faces they cut through.
- **Bounds come from sampled edges** (64 segments per curved edge), which
  is exact for extrusions. Mesh bounds of curved shapes are inscribed
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
- **`cad_ir` in `cad-app`.** It's a normal dependency there, used outside
  tests too. Test-only crates (`tempfile`, `zip`) are dev-dependencies.
