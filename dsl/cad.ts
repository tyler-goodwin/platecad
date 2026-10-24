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

export type ExtrudeSpec = {
  kind: "Extrude";
  profile: ProfileSpec;
  distance: number;
};

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
export type ProfileSpec = RectSpec | TranslateSpec | RotateSpec | ColorSpec | PartSpec;

export type ModelSpec =
  | RectSpec
  | ExtrudeSpec
  | TranslateSpec
  | RotateSpec
  | GroupSpec
  | ColorSpec
  | PartSpec
  | ArrangeSpec;

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

/** Sweeps a 2D profile along +Z into a solid. */
export function extrude(profile: ProfileSpec, opts: { distance: number }): ExtrudeSpec {
  positive("extrude distance", opts.distance);
  return { kind: "Extrude", profile, distance: opts.distance };
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
