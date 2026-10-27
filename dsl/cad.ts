/**
 * CAD DSL. All lengths are millimeters (what slicers expect); +Z is up.
 *
 * A model file's default export returns either a shape or a {@link Model}
 * with separate assembled and print views.
 */

export type Vec3 = [number, number, number];

/** sRGB, 0-255 per channel. */
export type Rgb = [number, number, number];

/** `"#rgb"`, `"#rrggbb"`, or `[r, g, b]` with 0-255 channels. */
export type ColorInput = string | Rgb;

export type RectSpec = {
  kind: "Rect";
  width: number;
  height: number;
};

export type PolygonSpec = {
  kind: "Polygon";
  points: [number, number][];
};

export type CircleSpec = { kind: "Circle"; radius: number };
export type RoundedRectSpec = { kind: "RoundedRect"; width: number; height: number; radius: number };
export type CutoutSpec = { kind: "Cutout"; outer: ProfileSpec; holes: ProfileSpec[] };

export type ExtrudeSpec = {
  kind: "Extrude";
  profile: ProfileSpec;
  distance: number;
  direction?: Vec3;
};

export type RevolveSpec = {
  kind: "Revolve";
  profile: ProfileSpec;
  degrees: number;
};

export type SphereSpec = { kind: "Sphere"; radius: number };
export type CylinderSpec = { kind: "Cylinder"; radius: number; height: number };
export type ConeSpec = { kind: "Cone"; radius: number; top: number; height: number };
export type TorusSpec = { kind: "Torus"; radius: number; tube: number };

export type TranslateSpec = {
  kind: "Translate";
  offset: Vec3;
  shape: ModelSpec;
};

export type RotateSpec = {
  kind: "Rotate";
  axis: Vec3;
  degrees: number;
  origin: Vec3;
  shape: ModelSpec;
};

export type UnionSpec = { kind: "Union"; children: ModelSpec[] };
export type DifferenceSpec = { kind: "Difference"; shape: ModelSpec; tools: ModelSpec[] };
export type IntersectSpec = { kind: "Intersect"; children: ModelSpec[] };

/**
 * Picks edges, CadQuery style: `|Z` straight edges parallel to Z; `>Z` / `<Z`
 * edges lying at the shape's max / min Z (likewise X, Y); `all`.
 */
export type EdgeSelector = "all" | "|X" | "|Y" | "|Z" | ">X" | "<X" | ">Y" | "<Y" | ">Z" | "<Z";

export type FilletSpec = { kind: "Fillet"; shape: ModelSpec; radius: number; edges: EdgeSelector[] };
export type ChamferSpec = { kind: "Chamfer"; shape: ModelSpec; distance: number; edges: EdgeSelector[] };

export type GroupSpec = {
  kind: "Group";
  children: ModelSpec[];
};

export type ColorSpec = {
  kind: "Color";
  rgb: Rgb;
  shape: ModelSpec;
};

export type PartSpec = {
  kind: "Part";
  name: string;
  shape: ModelSpec;
};

/** A printer's build volume, mm. */
export type Bed = { width: number; depth: number; height: number };

export type ArrangeSpec = {
  kind: "Arrange";
  gap: number;
  children: ModelSpec[];
  bed?: Bed;
};

/** 2D shapes that can be swept into solids. */
export type ProfileSpec =
  | RectSpec
  | RoundedRectSpec
  | PolygonSpec
  | CircleSpec
  | CutoutSpec
  | TranslateSpec
  | RotateSpec
  | ColorSpec
  | PartSpec;

export type ModelSpec =
  | RectSpec
  | RoundedRectSpec
  | PolygonSpec
  | CircleSpec
  | CutoutSpec
  | ExtrudeSpec
  | RevolveSpec
  | SphereSpec
  | CylinderSpec
  | ConeSpec
  | TorusSpec
  | TranslateSpec
  | RotateSpec
  | GroupSpec
  | UnionSpec
  | DifferenceSpec
  | IntersectSpec
  | FilletSpec
  | ChamferSpec
  | ColorSpec
  | PartSpec
  | ArrangeSpec;

/** An axis-aligned box around a shape, mm. `size` is `max - min`. */
export type Bounds = { min: Vec3; max: Vec3; size: Vec3; center: Vec3 };

/** What a model file exports: how it looks put together, and optionally how it prints. */
export type Model = {
  assembled: ModelSpec;
  /** Parts oriented for printing, usually wrapped in {@link arrange}. */
  print?: ModelSpec;
};

function positive(name: string, value: number) {
  if (!(value > 0)) {
    throw new Error(`${name} must be positive (got ${value})`);
  }
}

/** Rectangle in the XY plane, corner at the origin. */
export function rect(opts: { width: number; height: number }): RectSpec {
  positive("rect width", opts.width);
  positive("rect height", opts.height);
  return { kind: "Rect", width: opts.width, height: opts.height };
}

/**
 * Rectangle in the XY plane, corner at the origin, with its corners rounded
 * by `radius` (true arcs, so it extrudes with smooth corners and needs no
 * fillet). `radius` can be at most half the shorter side.
 */
export function roundedRect(opts: { width: number; height: number; radius: number }): RoundedRectSpec {
  positive("roundedRect width", opts.width);
  positive("roundedRect height", opts.height);
  positive("roundedRect radius", opts.radius);
  if (2 * opts.radius > Math.min(opts.width, opts.height) + 1e-9) {
    throw new Error(
      `roundedRect radius must be at most half the shorter side (got ${opts.radius} for ${opts.width} x ${opts.height})`,
    );
  }
  return { kind: "RoundedRect", width: opts.width, height: opts.height, radius: opts.radius };
}

/**
 * Slot in the XY plane: `length` end to end along X, `width` across, with
 * fully round ends, centered on `at` (default the origin). Cut an extruded
 * one out of a plate for a bolt that can slide. Rotate it to run along Y.
 */
export function slot(opts: { length: number; width: number; at?: [number, number] }): TranslateSpec {
  positive("slot width", opts.width);
  if (!(opts.length >= opts.width) || !Number.isFinite(opts.length)) {
    throw new Error(`slot length must be at least its width (got ${opts.length} and ${opts.width})`);
  }
  const [x, y] = opts.at ?? [0, 0];
  const profile = roundedRect({ width: opts.length, height: opts.width, radius: opts.width / 2 });
  return translate(profile, [x - opts.length / 2, y - opts.width / 2, 0]);
}

/**
 * Simple polygon in the XY plane from its outline, e.g. an L or U shape.
 * Either winding works and the closing point is implied. It must not cross
 * itself.
 */
export function polygon(points: [number, number][]): PolygonSpec {
  if (!Array.isArray(points) || points.length < 3) {
    throw new Error("polygon needs at least 3 points");
  }
  for (const p of points) {
    if (!Array.isArray(p) || p.length !== 2 || !p.every(Number.isFinite)) {
      throw new Error(`polygon points must be finite [x, y] pairs (got ${JSON.stringify(p)})`);
    }
  }
  return { kind: "Polygon", points: points.map(([x, y]) => [x, y]) };
}

/** Circle in the XY plane, centered on `at` (default the origin). */
export function circle(radius: number, opts: { at?: [number, number] } = {}): CircleSpec | TranslateSpec {
  positive("circle radius", radius);
  const spec: CircleSpec = { kind: "Circle", radius };
  return opts.at ? translate(spec, [opts.at[0], opts.at[1], 0]) : spec;
}

/**
 * A profile with holes: `outer` minus each hole (all flat in the same plane).
 * Holes must sit inside the outline without touching each other. Unlike a
 * 3D difference, this never fails on lined-up faces.
 */
export function cutout(outer: ProfileSpec, ...holes: ProfileSpec[]): CutoutSpec {
  if (holes.length === 0) {
    throw new Error("cutout needs at least one hole");
  }
  return { kind: "Cutout", outer, holes };
}

/**
 * Sweeps a 2D profile into a solid: `distance` mm along `direction`
 * (default +Z; any length, it's normalized).
 */
export function extrude(profile: ProfileSpec, opts: { distance: number; direction?: Vec3 }): ExtrudeSpec {
  positive("extrude distance", opts.distance);
  const spec: ExtrudeSpec = { kind: "Extrude", profile, distance: opts.distance };
  if (opts.direction) {
    const d = opts.direction;
    if (d.length !== 3 || !d.every(Number.isFinite) || d.every((c) => c === 0)) {
      throw new Error(`extrude direction must be a non-zero [x, y, z] (got ${JSON.stringify(d)})`);
    }
    spec.direction = [d[0], d[1], d[2]];
  }
  return spec;
}

/**
 * Lathe: draw the profile in XY with X as the radius and Y as the height; it
 * is spun around +Z, so the result stands on the XY plane. Keep the profile
 * at x >= 0. `degrees` defaults to a full turn.
 */
export function revolve(profile: ProfileSpec, opts: { degrees?: number } = {}): RevolveSpec {
  const degrees = opts.degrees ?? 360;
  if (!(degrees > 0 && degrees <= 360)) {
    throw new Error(`revolve degrees must be in (0, 360] (got ${degrees})`);
  }
  return { kind: "Revolve", profile, degrees };
}

function placed<T extends ModelSpec>(name: string, spec: T, at: Vec3 | undefined): T | TranslateSpec {
  if (at === undefined) {
    return spec;
  }
  if (!Array.isArray(at) || at.length !== 3 || !at.every(Number.isFinite)) {
    throw new Error(`${name} at must be a finite [x, y, z] (got ${JSON.stringify(at)})`);
  }
  return translate(spec, at);
}

/**
 * Solid sphere centered on `at` (default the origin). A true sphere, unlike
 * a revolved polygon, is one smooth face, so booleans with it stay cheap.
 */
export function sphere(radius: number, opts: { at?: Vec3 } = {}): SphereSpec | TranslateSpec {
  positive("sphere radius", radius);
  return placed("sphere", { kind: "Sphere", radius }, opts.at);
}

/** Solid cylinder standing on the XY plane: base centered on `at` (default the origin), `height` up +Z. */
export function cylinder(opts: { radius: number; height: number; at?: Vec3 }): CylinderSpec | TranslateSpec {
  positive("cylinder radius", opts.radius);
  positive("cylinder height", opts.height);
  return placed("cylinder", { kind: "Cylinder", radius: opts.radius, height: opts.height }, opts.at);
}

/**
 * Solid cone standing on the XY plane: a base of `radius` centered on `at`
 * (default the origin), narrowing to `top` (default 0, a point) at `height`.
 * A `top` above 0 makes a frustum; `radius: 0` with a `top` stands it on its tip.
 */
export function cone(opts: { radius: number; top?: number; height: number; at?: Vec3 }): ConeSpec | TranslateSpec {
  const top = opts.top ?? 0;
  positive("cone height", opts.height);
  if (!(opts.radius >= 0 && top >= 0) || !Number.isFinite(opts.radius) || !Number.isFinite(top)) {
    throw new Error(`cone radius and top must be 0 or more (got ${opts.radius}, ${top})`);
  }
  if (opts.radius === 0 && top === 0) {
    throw new Error("cone needs a radius or top above 0");
  }
  return placed("cone", { kind: "Cone", radius: opts.radius, top, height: opts.height }, opts.at);
}

/**
 * Solid torus (a ring) lying flat around +Z, centered on `at` (default the
 * origin). The tube's center runs on a circle of `radius`; `tube` is the
 * tube's own radius, and must be smaller than `radius`.
 */
export function torus(opts: { radius: number; tube: number; at?: Vec3 }): TorusSpec | TranslateSpec {
  positive("torus tube", opts.tube);
  if (!(opts.radius > opts.tube) || !Number.isFinite(opts.radius)) {
    throw new Error(`torus radius must be larger than its tube (got ${opts.radius} and ${opts.tube})`);
  }
  return placed("torus", { kind: "Torus", radius: opts.radius, tube: opts.tube }, opts.at);
}

/** Moves a shape by `offset`. */
export function translate<T extends ModelSpec>(shape: T, offset: Vec3): TranslateSpec {
  if (!offset.every(Number.isFinite)) {
    throw new Error(`translate offset must be finite (got [${offset}])`);
  }
  return { kind: "Translate", offset, shape };
}

const AXES: Record<"x" | "y" | "z", Vec3> = { x: [1, 0, 0], y: [0, 1, 0], z: [0, 0, 1] };

/**
 * Rotates a shape by `degrees` (right-hand rule: positive is counter-clockwise
 * looking down the axis) about the line through `origin` (default the world
 * origin) along `axis`.
 */
export function rotate<T extends ModelSpec>(
  shape: T,
  opts: { axis: "x" | "y" | "z" | Vec3; degrees: number; origin?: Vec3 },
): RotateSpec {
  const axis = typeof opts.axis === "string" ? AXES[opts.axis] : opts.axis;
  if (!axis || axis.length !== 3 || !axis.every(Number.isFinite) || axis.every((c) => c === 0)) {
    throw new Error(`rotate axis must be "x", "y", "z" or a non-zero [x, y, z] (got ${JSON.stringify(opts.axis)})`);
  }
  if (!Number.isFinite(opts.degrees)) {
    throw new Error(`rotate degrees must be finite (got ${opts.degrees})`);
  }
  const origin = opts.origin ?? [0, 0, 0];
  if (!origin.every(Number.isFinite)) {
    throw new Error(`rotate origin must be finite (got [${origin}])`);
  }
  return { kind: "Rotate", axis, degrees: opts.degrees, origin, shape };
}

/** Shows several shapes together. No boolean union: overlapping solids stay separate. */
export function group(...children: ModelSpec[]): GroupSpec {
  if (children.length === 0) {
    throw new Error("group needs at least one child");
  }
  return { kind: "Group", children };
}

/** Parses `"#rgb"`, `"#rrggbb"` or `[r, g, b]` (0-255) into an sRGB triple. */
export function parseColor(value: ColorInput): Rgb {
  if (Array.isArray(value)) {
    if (value.length !== 3 || !value.every((c) => Number.isInteger(c) && c >= 0 && c <= 255)) {
      throw new Error(`color must be [r, g, b] with integers 0-255 (got [${value}])`);
    }
    return [value[0], value[1], value[2]];
  }
  const hex = /^#([0-9a-f]{3}|[0-9a-f]{6})$/i.exec(value)?.[1];
  if (!hex) {
    throw new Error(`color must be "#rgb" or "#rrggbb" (got ${JSON.stringify(value)})`);
  }
  const full = hex.length === 3 ? [...hex].map((c) => c + c).join("") : hex;
  return [0, 2, 4].map((i) => parseInt(full.slice(i, i + 2), 16)) as Rgb;
}

/** Paints a shape. The innermost color wins; a colored profile colors its extrusion. */
export function color(shape: ModelSpec, value: ColorInput): ColorSpec {
  return { kind: "Color", rgb: parseColor(value), shape };
}

/**
 * Names a shape as a part: it becomes its own object in exports and shows up in
 * the part list. The innermost name wins.
 */
export function part(name: string, shape: ModelSpec): PartSpec {
  if (typeof name !== "string" || name.trim() === "") {
    throw new Error(`part name must be a non-empty string (got ${JSON.stringify(name)})`);
  }
  return { kind: "Part", name: name.trim(), shape };
}

/**
 * Print layout: puts each part on the plate (its lowest point at z = 0), in
 * rows `gap` mm apart. Orient parts yourself first (e.g. build boards lying
 * flat); this only moves them.
 *
 * Rows wrap at the bed width and spill onto more plates when the bed fills.
 * The bed comes from `printer.toml` at the project root unless given here.
 */
export function arrange(parts: ModelSpec[], opts: { gap?: number; bed?: Bed } = {}): ArrangeSpec {
  const gap = opts.gap ?? 10;
  if (parts.length === 0) {
    throw new Error("arrange needs at least one part");
  }
  if (!(gap >= 0)) {
    throw new Error(`arrange gap must be >= 0 (got ${gap})`);
  }
  const spec: ArrangeSpec = { kind: "Arrange", gap, children: parts };
  if (opts.bed) {
    const { width, depth, height } = opts.bed;
    if (![width, depth, height].every((d) => d > 0 && Number.isFinite(d))) {
      throw new Error(`arrange bed sizes must be positive (got ${JSON.stringify(opts.bed)})`);
    }
    spec.bed = { width, depth, height };
  }
  return spec;
}

/**
 * Measures `shape`: the box its solids fill, as tight as `arrange` measures
 * (exact on flat faces, within 0.01 mm on curved ones). The kernel builds the
 * shape to find out, so this is how a model fits parts to each other, e.g.
 * sitting a lid on whatever height a body came out. What it builds is cached,
 * so measuring a shape you then use costs no second build. A bare 2D profile
 * measures as its 0.01 mm display plate.
 */
export function bounds(shape: ModelSpec): Bounds {
  const query = (globalThis as any).rustyscript?.functions?.plate_bounds;
  if (typeof query !== "function") {
    throw new Error("bounds needs a kernel to measure with, and this host has none");
  }
  const { min, max } = query(shape) as { min: Vec3; max: Vec3 };
  const each = (f: (i: number) => number): Vec3 => [f(0), f(1), f(2)];
  return { min, max, size: each((i) => max[i] - min[i]), center: each((i) => (min[i] + max[i]) / 2) };
}

/**
 * Fuses solids into one. Solids that only touch or share a face plane fuse
 * too. Solids that don't touch at all stay separate pieces of one part.
 */
export function union(...shapes: ModelSpec[]): UnionSpec {
  if (shapes.length === 0) {
    throw new Error("union needs at least one shape");
  }
  return { kind: "Union", children: shapes };
}

/**
 * Cuts `tools` out of `shape`. A tool may end flush with the faces it cuts
 * through.
 */
export function difference(shape: ModelSpec, ...tools: ModelSpec[]): DifferenceSpec {
  if (tools.length === 0) {
    throw new Error("difference needs at least one tool");
  }
  return { kind: "Difference", shape, tools };
}

const EDGE_SELECTORS: EdgeSelector[] = ["all", "|X", "|Y", "|Z", ">X", "<X", ">Y", "<Y", ">Z", "<Z"];

function edgeSelectors(name: string, edges: EdgeSelector | EdgeSelector[] | undefined): EdgeSelector[] {
  const list = edges === undefined ? [] : Array.isArray(edges) ? edges : [edges];
  for (const e of list) {
    if (!EDGE_SELECTORS.includes(e)) {
      throw new Error(`${name} edges must be one of ${EDGE_SELECTORS.join(", ")} (got ${JSON.stringify(e)})`);
    }
  }
  return [...list];
}

/**
 * Rounds edges of a solid with `radius` mm: every edge, or those picked by
 * `edges` (e.g. "|Z" for corners seen from above).
 */
export function fillet(shape: ModelSpec, radius: number, opts: { edges?: EdgeSelector | EdgeSelector[] } = {}): FilletSpec {
  positive("fillet radius", radius);
  return { kind: "Fillet", shape, radius, edges: edgeSelectors("fillet", opts.edges) };
}

/**
 * Bevels edges of a solid by `distance` mm (e.g. "<Z" for a first-layer
 * chamfer).
 */
export function chamfer(shape: ModelSpec, distance: number, opts: { edges?: EdgeSelector | EdgeSelector[] } = {}): ChamferSpec {
  positive("chamfer distance", distance);
  return { kind: "Chamfer", shape, distance, edges: edgeSelectors("chamfer", opts.edges) };
}

/** Keeps the volume common to all shapes. */
export function intersect(...shapes: ModelSpec[]): IntersectSpec {
  if (shapes.length === 0) {
    throw new Error("intersect needs at least one shape");
  }
  return { kind: "Intersect", children: shapes };
}

/**
 * Axis-aligned box: width (X) × depth (Y) × height (Z), with its min corner at
 * `at` (default the origin).
 */
export function box(opts: {
  width: number;
  depth: number;
  height: number;
  at?: Vec3;
}): ExtrudeSpec | TranslateSpec {
  const solid = extrude(rect({ width: opts.width, height: opts.depth }), {
    distance: opts.height,
  });
  return opts.at ? translate(solid, opts.at) : solid;
}

export function cube(size: number): ExtrudeSpec | TranslateSpec {
  return box({ width: size, depth: size, height: size });
}

/** One process step of a {@link pipeline}: takes the shape so far, returns the next. */
export type Step = (shape: ModelSpec) => ModelSpec;

/**
 * Runs `shape` through `steps` in order, so a chain of operations reads top
 * to bottom instead of inside out:
 *
 * ```ts
 * pipeline(
 *   rect({ width: 200, height: 100 }),
 *   step.extrude({ distance: 3 }),
 *   step.fillet(3, { edges: "|Z" }),
 *   step.cut(...holes),
 * )
 * ```
 *
 * A step is any function from shape to shape: one from {@link step}, or your
 * own, e.g. `(s) => mirrored(s)`.
 */
export function pipeline(shape: ModelSpec, ...steps: Step[]): ModelSpec {
  let current = shape;
  steps.forEach((s, i) => {
    if (typeof s !== "function") {
      throw new Error(`pipeline step ${i + 1} must be a function (got ${JSON.stringify(s)})`);
    }
    current = s(current);
    if (typeof current !== "object" || current === null || typeof current.kind !== "string") {
      throw new Error(`pipeline step ${i + 1} must return a shape (got ${JSON.stringify(current)})`);
    }
  });
  return current;
}

/**
 * The DSL's operations as {@link pipeline} steps: the same arguments, minus
 * the shape they act on. Booleans take the other shapes: `cut` subtracts them
 * from the shape so far, `add` fuses them on, and `intersect` keeps the
 * common volume.
 */
export const step = {
  extrude: (opts: { distance: number; direction?: Vec3 }): Step => (s) => extrude(s as ProfileSpec, opts),
  revolve: (opts: { degrees?: number } = {}): Step => (s) => revolve(s as ProfileSpec, opts),
  translate: (offset: Vec3): Step => (s) => translate(s, offset),
  rotate: (opts: { axis: "x" | "y" | "z" | Vec3; degrees: number; origin?: Vec3 }): Step => (s) => rotate(s, opts),
  fillet: (radius: number, opts: { edges?: EdgeSelector | EdgeSelector[] } = {}): Step => (s) => fillet(s, radius, opts),
  chamfer: (distance: number, opts: { edges?: EdgeSelector | EdgeSelector[] } = {}): Step => (s) => chamfer(s, distance, opts),
  cut: (...tools: ModelSpec[]): Step => (s) => difference(s, ...tools),
  add: (...shapes: ModelSpec[]): Step => (s) => union(s, ...shapes),
  intersect: (...shapes: ModelSpec[]): Step => (s) => intersect(s, ...shapes),
  color: (value: ColorInput): Step => (s) => color(s, value),
  part: (name: string): Step => (s) => part(name, s),
};
