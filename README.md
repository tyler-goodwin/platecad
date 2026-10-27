# <img src="assets/plate.png" width="48" alt=""> Plate CAD

Code-first CAD for 3D printing. You write models in TypeScript and see them
as solids in a native 3D viewer that reloads every time you save. When a
model is ready, you lay its parts out on your printer's bed and export a
3MF for your slicer.

```ts
import { arrange, circle, extrude, pipeline, roundedRect, step, translate } from "../dsl/cad";

const size = 90; // mm

// A rounded square with a pocket underneath for a 10 mm magnet.
function coaster(name: string) {
  const pocket = translate(extrude(circle(5.1), { distance: 3 }), [size / 2, size / 2, -1]);
  return pipeline(
    roundedRect({ width: size, height: size, radius: 8 }),
    step.extrude({ distance: 5 }),
    step.cut(pocket),
    step.color("#2f6f9f"),
    step.part(name),
  );
}

const set = ["one", "two", "three", "four"].map(coaster);

export default () => ({
  assembled: set[0],
  print: arrange(set), // packed onto the bed from printer.toml
});
```

## Features

- **TypeScript models.** Models are plain functions, so loops, parameters
  and helper modules all just work. Imports resolve the way TypeScript's
  do, so `.ts` is optional. Your editor type-checks them against the DSL.
- **Real solids.** Geometry is B-rep, built with OpenCascade, with booleans,
  fillets, chamfers, extrude, revolve, and true spheres, cylinders, cones
  and tori.
- **Measure as you build.** `bounds(shape)` asks the kernel how big a shape
  really came out, so parts can be fitted to each other instead of to
  hand-worked numbers.
- **Hot reload.** Saving a model, or any file it imports, rebuilds it in the
  viewer. Errors show in the viewer and keep the last good model on screen.
- **Two views per model.** `assembled` shows how the parts go together, and
  `print` shows them oriented and packed onto plates the size of your bed.
  The viewer warns about parts that don't fit.
- **3MF export.** Each named part becomes its own watertight object, with
  colors, ready for multi-material slicing.
- **Sandboxed scripts.** Models can only import files inside their project.
  They get no filesystem or network access, and a script stuck in an
  endless loop is stopped after 10 seconds.

## Getting started

Plate CAD is written in Rust and is currently developed on Windows. You need
a Rust toolchain and the MSVC build tools.

```sh
cargo install --path crates/platecad   # installs the `plate` command
```

The first build compiles V8 and links a prebuilt OpenCascade, so it takes a
while. If the build fails while downloading OpenCascade
("max_headers_size"), see
[Building with OCC](architecture/README.md#kernel).

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
plate export MODEL [-o FILE.3mf] [--view auto|print|assembled] [--quality draft|normal|fine]
plate init [DIR]                             # set up a new project
```

Exports go to `outputs/<model>_<view>.3mf` in the project unless you pass
`-o`. The viewer's **Export 3MF** button writes the view on screen to the
same place.

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
| 2D profiles | `rect`, `roundedRect`, `slot`, `circle`, `polygon`, `cutout` (a profile with holes) |
| Solids | `extrude`, `revolve`, `box`, `cube`, `sphere`, `cylinder`, `cone`, `torus` |
| Placement | `translate`, `rotate`, `group` |
| Booleans | `union`, `difference`, `intersect` |
| Edges | `fillet`, `chamfer`, with edge selectors such as `"\|Z"` and `">Z"` |
| Printing | `part` (names an exported object), `color`, `arrange` (packs parts onto the bed) |
| Steps | `pipeline` with `step.extrude`, `step.cut`, `step.fillet`, … (see below) |
| Measuring | `bounds` (a shape's `min`, `max`, `size` and `center`, from the kernel) |

`pipeline` runs a shape through a list of steps, so a chain of operations
reads top to bottom instead of inside out, as in the coaster above. The
`step.*` helpers are the usual operations minus the shape they act on
(`cut`, `add` and `intersect` are the booleans), and any function from
shape to shape works as a step too.

`bounds` builds the shape to measure it, and the result is cached for the
real build, so measuring costs little:

```ts
const b = bounds(body);
const lid = box({ width: b.size[0], depth: b.size[1], height: 2, at: [b.min[0], b.min[1], b.max[2]] });
```

See [`dsl/cad.ts`](dsl/cad.ts) for the details, and [`models/`](models)
for examples: a bookshelf, coasters with a holder, a desk organizer, a
rounded box, and a cat that stress-tests the kernel.

## How it works

```
models/*.ts → V8 (rustyscript) → model spec (JSON) → OpenCascade → triangles → viewer | 3MF
```

The model script only describes the shape as data; Rust does all the
geometry. [architecture/README.md](architecture/README.md) covers the
design, crate by crate, and [AGENTS.md](AGENTS.md) is the guide for coding
agents working in the repo. Run the tests with `cargo test --workspace`.

## License

Plate CAD is MIT licensed. See [LICENSE](LICENSE).

## Third-party software

Plate CAD uses facilities provided by
**[Open CASCADE Technology](https://dev.opencascade.org/)** (OCCT) for its
geometry. OCCT is licensed under the
[GNU LGPL 2.1](https://www.gnu.org/licenses/old-licenses/lgpl-2.1.html)
with the
[Open CASCADE exception](https://github.com/Open-Cascade-SAS/OCCT/blob/master/OCCT_LGPL_EXCEPTION.txt).
Its source is at [Open-Cascade-SAS/OCCT](https://github.com/Open-Cascade-SAS/OCCT).
The `plate` binary statically links an unmodified prebuilt OCCT 8.0.1
fetched by [cadrum](https://github.com/lzpel/cadrum). Since Plate CAD's
source is public, you can rebuild it against a modified OCCT. If you
distribute builds, ship `LICENSE_LGPL_21.txt` and `OCCT_LGPL_EXCEPTION.txt`
(found in the OCCT download under `target/`) alongside them.

The other dependencies (Rust crates, including V8 via rustyscript) are under
permissive licenses such as MIT, Apache-2.0, BSD, ISC and Zlib, with a few
MPL-2.0 crates.
