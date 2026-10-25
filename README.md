# platecad

Code-first CAD for 3D printing. You write models in TypeScript and see them
as solids in a native 3D viewer that reloads every time you save. When a
model is ready, you lay its parts out on your printer's bed and export a
3MF for your slicer.

```ts
import { arrange, box, circle, color, difference, extrude, fillet, part, translate } from "../dsl/cad.ts";

const size = 90; // mm

// A rounded square with a pocket underneath for a 10 mm magnet.
function coaster(name: string) {
  const body = fillet(box({ width: size, depth: size, height: 5 }), 8, { edges: "|Z" });
  const pocket = translate(extrude(circle(5.1), { distance: 3 }), [size / 2, size / 2, -1]);
  return part(name, color(difference(body, pocket), "#2f6f9f"));
}

const set = ["one", "two", "three", "four"].map(coaster);

export default () => ({
  assembled: set[0],
  print: arrange(set), // packed onto the bed from printer.toml
});
```

## Features

- **TypeScript models.** Models are plain functions, so loops, parameters
  and helper modules all just work. Your editor type-checks them against
  the DSL.
- **Real solids.** Geometry is B-rep, built with OpenCascade, with booleans,
  fillets, chamfers, extrude and revolve.
- **Hot reload.** Saving a model, or any file it imports, rebuilds it in the
  viewer. Errors show in the viewer and keep the last good model on screen.
- **Two views per model.** `assembled` shows how the parts go together, and
  `print` shows them oriented and packed onto plates the size of your bed.
  The viewer warns about parts that don't fit.
- **3MF export.** Each named part becomes its own watertight object, with
  colors, ready for multi-material slicing.
- **Sandboxed scripts.** Models can only import files inside their project.
  They get no filesystem or network access.

## Getting started

platecad is written in Rust and is currently developed on Windows. You need
a Rust toolchain and the MSVC build tools.

```sh
cargo install --path crates/platecad   # installs the `plate` command
```

The first build compiles V8 and links a prebuilt OpenCascade, so it takes a
while. If the build fails while downloading OpenCascade
("max_headers_size"), see
[Building with OCC](architecture/README.md#kernels-truck-and-occ).

Then start a project and open it:

```sh
plate init my-project   # printer.toml, the DSL, a starter model, tsconfig.json
plate my-project        # opens the viewer on the project
```

You can do the same from the viewer: **New project…** sets up a folder, and
**Open folder…** browses an existing one. Pick a model from the file list,
then edit it in any editor and save.

## Commands

```sh
plate [PATH]                                 # open the viewer on a model or folder
plate export MODEL [-o FILE.3mf] [--view auto|print|assembled]
plate init [DIR]                             # set up a new project
```

Exports go to `outputs/<model>_<view>.3mf` in the project unless you pass
`-o`. The viewer's **Export 3MF** button writes the view on screen to the
same place. Use `--kernel truck` (or `PLATE_KERNEL=truck`) to try the
pure-Rust kernel instead of OpenCascade. It can't do fillets.

## A project

```
my-project/
  printer.toml    # bed size, used by arrange() and the fit warnings
  dsl/cad.ts      # the modeling DSL (marks the project root)
  models/cube.ts  # your models; organize them however you like
  tsconfig.json   # editor support only
  outputs/        # exports (git-ignored)
```

`printer.toml` starts as a Bambu Lab P2S (256 × 256 × 256 mm). Edit it for
your printer, and the print view re-packs as you save.

## The DSL

All lengths are millimeters, and +Z is up.

| | |
|---|---|
| 2D profiles | `rect`, `circle`, `polygon`, `cutout` (a profile with holes) |
| Solids | `extrude`, `revolve`, `box`, `cube` |
| Placement | `translate`, `rotate`, `group` |
| Booleans | `union`, `difference`, `intersect` |
| Edges | `fillet`, `chamfer`, with edge selectors such as `"\|Z"` and `">Z"` |
| Printing | `part` (names an exported object), `color`, `arrange` (packs parts onto the bed) |

See [`dsl/cad.ts`](dsl/cad.ts) for the details, and [`models/`](models)
for examples: a bookshelf, coasters with a holder, a desk organizer and a
rounded box.

## How it works

```
models/*.ts → V8 (rustyscript) → model spec (JSON) → OpenCascade → triangles → viewer | 3MF
```

The model script only describes the shape as data; Rust does all the
geometry. [architecture/README.md](architecture/README.md) covers the
design, crate by crate, and [AGENTS.md](AGENTS.md) is the guide for coding
agents working in the repo. Run the tests with `cargo test --workspace`.

## License

MIT. The binary statically links OpenCascade (OCCT), which is LGPL-2.1 with
the OCCT exception. That's fine for personal use, but distributing builds
carries LGPL relinking obligations.
