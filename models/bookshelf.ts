import {
  arrange,
  box,
  color,
  group,
  type ColorInput,
  type Model,
  type ModelSpec,
  type Vec3,
} from "../dsl/cad.ts";

export type BookshelfParams = {
  /** Outer size, mm. X × Y × Z. */
  width: number;
  depth: number;
  height: number;
  /** Board thickness for sides, top, bottom and shelves. */
  thickness: number;
  /** Back panel thickness. */
  back: number;
  /** Adjustable shelves between bottom and top. */
  shelves: number;
  /** Sides, top, bottom and shelves. */
  wood: ColorInput;
  /** Back panel. */
  backing: ColorInput;
};

export const defaults: BookshelfParams = {
  width: 1000,
  depth: 300,
  height: 800,
  thickness: 18,
  back: 6,
  shelves: 4,
  wood: "#b07a4f",
  backing: "#e8e0d0",
};

/** An axis-aligned board: `size` is its X × Y × Z extent where it sits in the shelf. */
type Board = { name: string; size: Vec3; at: Vec3 };

/** The board where it goes in the assembled shelf. */
function placed(b: Board): ModelSpec {
  const [width, depth, height] = b.size;
  return box({ width, depth, height, at: b.at });
}

/** The same board lying on its largest face, ready to print. */
function flat(b: Board): ModelSpec {
  const [length, width, thickness] = [...b.size].sort((x, y) => y - x);
  return box({ width: length, depth: width, height: thickness });
}

function boards(p: BookshelfParams): { frame: Board[]; back: Board } {
  const t = p.thickness;
  const innerWidth = p.width - 2 * t;
  const shelfDepth = p.depth - p.back;

  if (!Number.isInteger(p.shelves) || p.shelves < 0) {
    throw new Error(`shelves must be a whole number >= 0 (got ${p.shelves})`);
  }
  // Equal gaps between bottom, each shelf, and top.
  const gap = (p.height - 2 * t - p.shelves * t) / (p.shelves + 1);
  if (!(gap > 0)) {
    throw new Error(`${p.shelves} shelves don't fit in height ${p.height}`);
  }

  const shelves = Array.from({ length: p.shelves }, (_, i): Board => ({
    name: `shelf ${i + 1}`,
    size: [innerWidth, shelfDepth, t],
    at: [t, 0, t + (i + 1) * gap + i * t],
  }));

  return {
    frame: [
      { name: "left side", size: [t, p.depth, p.height], at: [0, 0, 0] },
      { name: "right side", size: [t, p.depth, p.height], at: [p.width - t, 0, 0] },
      { name: "bottom", size: [innerWidth, shelfDepth, t], at: [t, 0, 0] },
      { name: "top", size: [innerWidth, shelfDepth, t], at: [t, 0, p.height - t] },
      ...shelves,
    ],
    back: { name: "back panel", size: [innerWidth, p.back, p.height], at: [t, shelfDepth, 0] },
  };
}

export function bookshelf(overrides: Partial<BookshelfParams> = {}): Model {
  const p = { ...defaults, ...overrides };
  const { frame, back } = boards(p);

  return {
    assembled: group(
      color(group(...frame.map(placed)), p.wood),
      color(placed(back), p.backing),
    ),
    print: arrange([
      ...frame.map((b) => color(flat(b), p.wood)),
      color(flat(back), p.backing),
    ]),
  };
}

export default function model() {
  return bookshelf();
}
