import {
  arrange,
  box,
  circle,
  color,
  cone,
  cylinder,
  difference,
  group,
  part,
  polygon,
  revolve,
  rotate,
  sphere,
  torus,
  translate,
  union,
  type ColorInput,
  type Model,
  type ModelSpec,
  type Vec3,
} from "../dsl/cad.ts";

/**
 * A sitting cat, facing -Y: a stress test for the kernel and mesher. Nearly
 * everything is curved (spheres, a revolved egg of a body, cone ears, a
 * curled tail, a torus collar), and the fur is one big union of curved
 * solids trimmed flat at z = 0 so it prints standing up. Eyes, nose, whiskers, collar and bell are
 * separate colored parts for a multi-filament print. Needs the OCC kernel
 * (the default): truck's booleans can't fuse this many curved solids.
 *
 * `segments` sets how many flat bands the body's outline has; raise it to
 * make the kernel and mesher work harder.
 */
export type CatParams = {
  /** Points along the body outline. */
  segments: number;
  /** Whiskers per side. */
  whiskers: number;
  fur: ColorInput;
  muzzle: ColorInput;
  eyes: ColorInput;
  pupils: ColorInput;
  nose: ColorInput;
  collar: ColorInput;
  bell: ColorInput;
};

export const defaults: CatParams = {
  segments: 48,
  whiskers: 3,
  fur: "#e08a3c",
  muzzle: "#f4ead8",
  eyes: "#6cc24a",
  pupils: "#1b1b1b",
  nose: "#e88a9a",
  collar: "#c0392b",
  bell: "#e8c547",
};

/** Body axis: the body is a solid of revolution about the vertical line through here. */
const BODY: [number, number] = [0, 5];
const HEAD: Vec3 = [0, -4, 62];
const HEAD_R = 16;

function at(shape: ModelSpec, p: Vec3): ModelSpec {
  return translate(shape, p);
}

/**
 * The body: an egg standing on its cut-off bottom, narrower at the shoulders.
 * Outline in revolve's (radius, height) plane.
 */
function body(segments: number): ModelSpec {
  const radius = 22;
  const middle = 24;
  const half = 34;
  const bottom = Math.asin(-middle / half);
  const points: [number, number][] = [[0, 0]];
  for (let i = 0; i <= segments; i++) {
    const t = bottom + (i / segments) * (Math.PI / 2 - bottom);
    const s = Math.sin(t);
    const taper = 1 - 0.3 * (s + 1) / 2;
    points.push([radius * Math.cos(t) * taper, middle + half * s]);
  }
  points[points.length - 1][0] = 0;
  return at(revolve(polygon(points)), [BODY[0], BODY[1], 0]);
}


function ear(side: 1 | -1): ModelSpec {
  const tilted = rotate(cone({ radius: 7, height: 14 }), { axis: "y", degrees: side * 25 });
  return at(rotate(tilted, { axis: "x", degrees: 10 }), [side * 8.5, HEAD[1] + 1, HEAD[2] + 10]);
}

function tail(): ModelSpec {
  // Curls around the base from the back (+Y) past the right side to the front.
  const ring = 19;
  const tube = 4;
  const z = tube - 0.5;
  const start = -40;
  // Part of a ring around the body axis: a revolved circle, swept 130 degrees.
  const arc = at(revolve(circle(tube, { at: [ring, 0] }), { degrees: 130 }), [BODY[0], BODY[1], 0]);
  const curl = rotate(arc, { axis: "z", degrees: start, origin: [BODY[0], BODY[1], 0] });
  const a = (start * Math.PI) / 180;
  const tip = sphere(tube, { at: [BODY[0] + ring * Math.cos(a), BODY[1] + ring * Math.sin(a), 0] });
  return at(group(curl, tip), [0, 0, z]);
}

/** Everything furry, fused and trimmed flat at z = 0. */
function fur(p: CatParams): ModelSpec {
  const legs = [-1, 1].flatMap((side) => [
    cylinder({ radius: 5, height: 30, at: [side * 8, -12, 0] }),
    sphere(6, { at: [side * 8, -14, 5.5] }),
    sphere(12, { at: [side * 14, 10, 12] }),
  ]);
  const solid = union(body(p.segments), sphere(HEAD_R, { at: HEAD }), ear(-1), ear(1), ...legs, tail());
  const ground = box({ width: 200, depth: 200, height: 10, at: [-100, -100, -10] });
  return color(part("fur", difference(solid, ground)), p.fur);
}

/** A point on the head's surface, `out` mm beyond it, in the direction of `toward`. */
function onHead(toward: Vec3, out = 0): Vec3 {
  const len = Math.hypot(...toward);
  const k = (HEAD_R + out) / len;
  return [HEAD[0] + toward[0] * k, HEAD[1] + toward[1] * k, HEAD[2] + toward[2] * k];
}

function face(p: CatParams): ModelSpec {
  const eyes = [-1, 1].map((side) => sphere(3.8, { at: onHead([side * 6.5, -12, 4], -1.8) }));
  const pupils = [-1, 1].map((side) => sphere(1.4, { at: onHead([side * 6.5, -12, 4], 1) }));
  const muzzle = [-1, 1].map((side) => sphere(5, { at: [side * 3.5, -17.5, 56.5] }));
  const whiskers = [-1, 1].flatMap((side) =>
    Array.from({ length: p.whiskers }, (_, i) => {
      const lift = (i - (p.whiskers - 1) / 2) * 1.8;
      // A rod up +Z, laid over to point sideways (fanning up and down), then swept forward a little.
      const rod = rotate(cylinder({ radius: 0.4, height: 18 }), { axis: "y", degrees: side * (90 - lift * 4) });
      return at(rotate(rod, { axis: "z", degrees: -side * 10 }), [side * 6, -20, 56.5 + lift]);
    }),
  );
  return group(
    color(part("eyes", group(...eyes)), p.eyes),
    color(part("pupils", group(...pupils)), p.pupils),
    color(part("muzzle", union(...muzzle)), p.muzzle),
    color(part("nose", sphere(2.2, { at: [0, -21.5, 60.5] })), p.nose),
    ...(whiskers.length > 0 ? [color(part("whiskers", group(...whiskers)), p.muzzle)] : []),
  );
}

function collar(p: CatParams): ModelSpec {
  return group(
    color(part("collar", torus({ radius: 14.5, tube: 2, at: [BODY[0], BODY[1], 44] })), p.collar),
    color(part("bell", sphere(3, { at: [0, BODY[1] - 14.5 - 3, 41] })), p.bell),
  );
}

export function cat(overrides: Partial<CatParams> = {}): Model {
  const p = { ...defaults, ...overrides };
  if (!Number.isInteger(p.segments) || p.segments < 4) {
    throw new Error(`segments must be a whole number >= 4 (got ${p.segments})`);
  }
  if (!Number.isInteger(p.whiskers) || p.whiskers < 0) {
    throw new Error(`whiskers must be a whole number >= 0 (got ${p.whiskers})`);
  }
  const whole = group(fur(p), face(p), collar(p));
  return { assembled: whole, print: arrange([whole]) };
}

export default function model() {
  return cat();
}
