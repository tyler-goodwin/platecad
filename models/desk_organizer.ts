import {
  arrange,
  box,
  circle,
  color,
  cutout,
  difference,
  extrude,
  group,
  part,
  polygon,
  rect,
  revolve,
  translate,
  union,
  type ColorInput,
  type Model,
  type ModelSpec,
} from "../dsl/cad.ts";

/**
 * A three-piece desk organizer, all printable without supports:
 *
 * - Pen cup: an L-shaped polygon (wall plus floor) revolved into a cup.
 * - Card holder: a base block fused with a back rest leaning at `lean`
 *   degrees (a slanted extrude), with slanted card slots cut into the base.
 * - Base plate: a plate with round holes for press-fit magnets (a cutout).
 *
 * Booleans need faces that cross rather than line up, so the back rest
 * starts inside the base block and stops short of its sides, and the slot
 * cutters start inside the block and poke out of its top.
 */
export type OrganizerParams = {
  cupRadius: number;
  cupHeight: number;
  /** Wall and floor thickness of the cup, mm. */
  cupWall: number;
  /** Card holder base, mm. */
  holderWidth: number;
  holderDepth: number;
  holderHeight: number;
  /** How far the back rest and slots lean back from vertical, degrees. */
  lean: number;
  /** Number of card slots in front of the back rest. */
  slots: number;
  /** Magnet holes: diameter, and inset from the plate's corners, mm. */
  magnet: number;
  magnetInset: number;
  colors: { cup: ColorInput; holder: ColorInput; plate: ColorInput };
};

export const defaults: OrganizerParams = {
  cupRadius: 35,
  cupHeight: 95,
  cupWall: 2.4,
  holderWidth: 90,
  holderDepth: 50,
  holderHeight: 12,
  lean: 20,
  slots: 2,
  magnet: 10,
  magnetInset: 12,
  colors: { cup: "#4fa3a5", holder: "#e8b04f", plate: "#2f3a4a" },
};

const PLATE_THICKNESS = 5;
const GAP = 10;

/** Cup: revolve the outline of one side of its cross-section (X = radius, Y = height). */
function cup(p: OrganizerParams): ModelSpec {
  const [r, h, w] = [p.cupRadius, p.cupHeight, p.cupWall];
  const section = polygon([
    [0, 0],
    [r, 0],
    [r, h],
    [r - w, h],
    [r - w, w],
    [0, w],
  ]);
  return revolve(section);
}

/** Direction leaning back (toward +Y) by `degrees` from vertical. */
function leaning(degrees: number): [number, number, number] {
  const a = (degrees * Math.PI) / 180;
  return [0, Math.sin(a), Math.cos(a)];
}

function cardHolder(p: OrganizerParams): ModelSpec {
  const [w, d, h] = [p.holderWidth, p.holderDepth, p.holderHeight];
  const direction = leaning(p.lean);
  const base = box({ width: w, depth: d, height: h });

  // Back rest: a 4 mm slab starting 2 mm up inside the base (so its faces
  // cross the base's top rather than sharing the bottom), 5 mm in from each side.
  const rest = translate(
    extrude(rect({ width: w - 10, height: 4 }), { distance: 70, direction }),
    [5, d - 16, 2],
  );

  // Card slots: 3 mm wide, cut 6 mm deep, starting inside the block and
  // poking out of its top.
  const cutters = Array.from({ length: p.slots }, (_, i) =>
    translate(extrude(rect({ width: w - 6, height: 3 }), { distance: 20, direction }), [3, 8 + i * 10, h - 6]),
  );

  return difference(union(base, rest), ...cutters);
}

function basePlate(p: OrganizerParams, width: number, depth: number): ModelSpec {
  const inset = p.magnetInset;
  const holes = [
    [inset, inset],
    [width - inset, inset],
    [inset, depth - inset],
    [width - inset, depth - inset],
  ].map(([x, y]) => circle(p.magnet / 2, { at: [x, y] }));
  return extrude(cutout(rect({ width, height: depth }), ...holes), { distance: PLATE_THICKNESS });
}

export function organizer(overrides: Partial<OrganizerParams> = {}): Model {
  const p = { ...defaults, ...overrides };
  if (!(p.cupWall > 0 && p.cupWall < p.cupRadius && p.cupWall < p.cupHeight)) {
    throw new Error(`cupWall must be between 0 and the cup's radius and height (got ${p.cupWall})`);
  }
  if (!Number.isInteger(p.slots) || p.slots < 0 || 8 + p.slots * 10 > p.holderDepth - 16) {
    throw new Error(`${p.slots} card slots don't fit in front of the back rest`);
  }

  const cupPart = color(part("pen cup", cup(p)), p.colors.cup);
  const holderPart = color(part("card holder", cardHolder(p)), p.colors.holder);

  // The plate spans both pieces side by side, with a margin all round.
  const cupSize = 2 * p.cupRadius;
  const plateWidth = GAP + cupSize + GAP + p.holderWidth + GAP;
  const plateDepth = GAP + Math.max(cupSize, p.holderDepth) + GAP;
  const platePart = color(part("base plate", basePlate(p, plateWidth, plateDepth)), p.colors.plate);

  const onPlate = PLATE_THICKNESS;
  return {
    assembled: group(
      platePart,
      translate(cupPart, [GAP + p.cupRadius, GAP + p.cupRadius, onPlate]),
      translate(holderPart, [GAP + cupSize + GAP, GAP, onPlate]),
    ),
    print: arrange([platePart, cupPart, holderPart]),
  };
}

export default function model() {
  return organizer();
}
