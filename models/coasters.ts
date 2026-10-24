import {
  arrange,
  box,
  color,
  group,
  part,
  rotate,
  translate,
  type ColorInput,
  type Model,
  type ModelSpec,
} from "../dsl/cad.ts";

/**
 * A set of square coasters with a raised stripe inlay in a second color: a
 * classic multi-filament print (base and inlay print together, one filament
 * each). Assembled: the set stacked. Print: packed onto the bed from
 * printer.toml (95 mm coasters fit four to a 256 mm plate).
 */
export type CoasterParams = {
  /** How many coasters. */
  count: number;
  /** Side length, mm. */
  size: number;
  /** Base thickness, mm. */
  base: number;
  /** Height of the raised inlay, mm. */
  inlay: number;
  /** Plain border around the inlay, mm. */
  margin: number;
  /** Stripes per coaster (stripes and gaps are equally wide). */
  stripes: number;
  baseColor: ColorInput;
  /** Inlay colors, used in turn from one coaster to the next. */
  inlayColors: ColorInput[];
};

export const defaults: CoasterParams = {
  count: 4,
  size: 95,
  base: 4,
  inlay: 1,
  margin: 8,
  stripes: 5,
  baseColor: "#2f3a4a",
  inlayColors: ["#e8b04f", "#4fa3a5"],
};

function validate(p: CoasterParams) {
  if (!Number.isInteger(p.count) || p.count < 1) {
    throw new Error(`count must be a whole number >= 1 (got ${p.count})`);
  }
  if (!Number.isInteger(p.stripes) || p.stripes < 1) {
    throw new Error(`stripes must be a whole number >= 1 (got ${p.stripes})`);
  }
  if (!(p.size > 2 * p.margin)) {
    throw new Error(`size ${p.size} leaves no room inside a ${p.margin} mm margin`);
  }
  if (p.inlayColors.length === 0) {
    throw new Error("inlayColors needs at least one color");
  }
}

/** Coaster `i` (0-based): base plus stripes, stripes turned 90 degrees on every other one. */
function coaster(p: CoasterParams, i: number): ModelSpec {
  const n = i + 1;
  const inner = p.size - 2 * p.margin;
  const stripe = inner / (2 * p.stripes - 1);
  const stripes = group(
    ...Array.from({ length: p.stripes }, (_, s) =>
      box({ width: inner, depth: stripe, height: p.inlay, at: [p.margin, p.margin + 2 * s * stripe, p.base] }),
    ),
  );
  const pattern =
    i % 2 === 0
      ? stripes
      : rotate(stripes, { axis: "z", degrees: 90, origin: [p.size / 2, p.size / 2, 0] });

  return group(
    color(part(`coaster ${n} base`, box({ width: p.size, depth: p.size, height: p.base })), p.baseColor),
    color(part(`coaster ${n} inlay`, pattern), p.inlayColors[i % p.inlayColors.length]),
  );
}

export function coasters(overrides: Partial<CoasterParams> = {}): Model {
  const p = { ...defaults, ...overrides };
  validate(p);
  const set = Array.from({ length: p.count }, (_, i) => coaster(p, i));
  const height = p.base + p.inlay;

  return {
    assembled: group(...set.map((c, i) => translate(c, [0, 0, i * height]))),
    print: arrange(set),
  };
}

export default function model() {
  return coasters();
}
