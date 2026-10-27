import {
  arrange,
  box,
  color,
  group,
  part,
  pipeline,
  rotate,
  step,
  translate,
  type ColorInput,
  type Model,
  type ModelSpec,
} from "../dsl/cad.ts";

/**
 * A set of square coasters with a raised stripe inlay in a second color: a
 * classic multi-filament print (base and inlay print together, one filament
 * each), plus a holder. Assembled: the set stacked in its holder. Print:
 * packed onto the bed from printer.toml.
 *
 * The holder is a tray: a floor and three walls, open at the front so the
 * coasters slide out. It's a block with a cutter subtracted; the cutter
 * overshoots the top and front, as booleans need.
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
  /** Include the holder. */
  holder: boolean;
  /** Holder wall thickness, mm. */
  wall: number;
  /** Holder floor thickness, mm. */
  floor: number;
  /** Gap between the coasters and the holder walls, mm. */
  clearance: number;
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
  holder: true,
  wall: 3,
  floor: 2,
  clearance: 1,
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
  if (!(p.wall > 0) || !(p.floor > 0) || !(p.clearance >= 0)) {
    throw new Error(
      `wall and floor must be positive and clearance >= 0 (got ${p.wall}, ${p.floor}, ${p.clearance})`,
    );
  }
}

/** How far cutters poke past the faces they cut through, mm. */
const OVERSHOOT = 1;

/**
 * A tray around a `size` square opening (plus clearance): floor, side walls
 * and back wall, open toward -Y. Walls rise `stack` mm above the floor. The
 * opening's corner is at (wall, 0, floor).
 */
function holder(p: CoasterParams, stack: number): ModelSpec {
  const t = p.wall;
  const opening = p.size + 2 * p.clearance;
  const block = box({ width: opening + 2 * t, depth: opening + t, height: p.floor + stack });
  const cutter = box({
    width: opening,
    depth: opening + OVERSHOOT,
    height: stack + OVERSHOOT,
    at: [t, -OVERSHOOT, p.floor],
  });
  return pipeline(block, step.cut(cutter), step.part("holder"), step.color(p.baseColor));
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
  const stack = group(...set.map((c, i) => translate(c, [0, 0, i * height])));

  if (!p.holder) {
    return { assembled: stack, print: arrange(set) };
  }
  const caddy = holder(p, p.count * height);
  const inside = p.wall + p.clearance;
  return {
    assembled: group(caddy, translate(stack, [inside, p.clearance, p.floor])),
    print: arrange([caddy, ...set]),
  };
}

export default function model() {
  return coasters();
}
