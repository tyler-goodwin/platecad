import { arrange, box, color, difference, fillet, part, type Model, type ModelSpec } from "../dsl/cad.ts";

/**
 * A small open-top box with rounded corners and even walls.
 *
 * The outside is a block with its vertical edges rounded; the cavity is the
 * same shape inset by `wall` on every side (its corner radius is the outer
 * radius minus the wall, so the corners are as thick as the sides). The
 * cavity pokes out of the top so the subtraction cuts cleanly.
 */
export type RoundedBoxParams = {
  /** Outside size, mm. */
  width: number;
  depth: number;
  height: number;
  /** Wall and floor thickness, mm. */
  wall: number;
  /** Outside corner radius, mm. Must exceed `wall`. */
  radius: number;
  color: string;
};

export const defaults: RoundedBoxParams = {
  width: 60,
  depth: 40,
  height: 30,
  wall: 3,
  radius: 6,
  color: "#4fa3a5",
};

function roundedBlock(width: number, depth: number, height: number, radius: number, at: [number, number, number]): ModelSpec {
  return fillet(box({ width, depth, height, at }), radius, { edges: "|Z" });
}

export function roundedBox(overrides: Partial<RoundedBoxParams> = {}): Model {
  const p = { ...defaults, ...overrides };
  if (!(p.radius > p.wall)) {
    throw new Error(`radius (${p.radius}) must be larger than the wall (${p.wall}) to round the inside too`);
  }
  if (!(2 * p.radius <= Math.min(p.width, p.depth))) {
    throw new Error(`radius ${p.radius} is too big for a ${p.width} x ${p.depth} box`);
  }
  if (!(p.height > p.wall)) {
    throw new Error(`height (${p.height}) must be larger than the floor (${p.wall})`);
  }

  const w = p.wall;
  const outside = roundedBlock(p.width, p.depth, p.height, p.radius, [0, 0, 0]);
  // Inset by the wall on every side, starting on top of the floor and
  // running 1 mm past the top.
  const cavity = roundedBlock(p.width - 2 * w, p.depth - 2 * w, p.height - w + 1, p.radius - w, [w, w, w]);

  const shell = color(part("box", difference(outside, cavity)), p.color);
  return { assembled: shell, print: arrange([shell]) };
}

export default function model() {
  return roundedBox();
}
