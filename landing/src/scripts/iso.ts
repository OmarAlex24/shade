/**
 * Isometric projection helpers (build-time / server-side).
 *
 * Used by the isometric illustration components to turn 3D box coordinates into
 * 2D SVG polygons. Standard 2:1 isometric:
 *   screenX = (x - y) * cos(30°)
 *   screenY = (x + y) * sin(30°) - z
 *
 * Axes (after projection):
 *   +x → lower-right   +y → lower-left   +z → up
 */

const C = Math.cos(Math.PI / 6); // ≈ 0.8660
const S = Math.sin(Math.PI / 6); // 0.5

export interface IsoOpts {
  ox?: number; // screen origin x (viewBox units)
  oy?: number; // screen origin y
  scale?: number;
}

export function project(x: number, y: number, z: number, o: IsoOpts = {}): [number, number] {
  const { ox = 0, oy = 0, scale = 1 } = o;
  return [ox + (x - y) * C * scale, oy + ((x + y) * S - z) * scale];
}

export function pts(arr: Array<[number, number]>): string {
  return arr.map(([x, y]) => `${round(x)},${round(y)}`).join(' ');
}

const round = (n: number): number => Math.round(n * 100) / 100;

export interface BoxFaces {
  top: string;
  left: string;
  right: string;
  /** projected centre of the top face — handy for placing logos/labels */
  topCenter: [number, number];
}

/**
 * Build the three visible faces of an axis-aligned box.
 * (x0,y0) is the near-origin footprint corner; sx/sy footprint, sz height.
 */
export function box(
  x0: number,
  y0: number,
  sx: number,
  sy: number,
  sz: number,
  o: IsoOpts = {}
): BoxFaces {
  const P = (x: number, y: number, z: number): [number, number] => project(x, y, z, o);

  const top = pts([P(x0, y0, sz), P(x0 + sx, y0, sz), P(x0 + sx, y0 + sy, sz), P(x0, y0 + sy, sz)]);
  const right = pts([
    P(x0 + sx, y0, 0),
    P(x0 + sx, y0 + sy, 0),
    P(x0 + sx, y0 + sy, sz),
    P(x0 + sx, y0, sz),
  ]);
  const left = pts([
    P(x0, y0 + sy, 0),
    P(x0 + sx, y0 + sy, 0),
    P(x0 + sx, y0 + sy, sz),
    P(x0, y0 + sy, sz),
  ]);

  const topCenter = P(x0 + sx / 2, y0 + sy / 2, sz);
  return { top, left, right, topCenter };
}

/** Straight polyline (in screen space) between two projected 3D points. */
export function edge(
  a: [number, number, number],
  b: [number, number, number],
  o: IsoOpts = {}
): string {
  const [ax, ay] = project(a[0], a[1], a[2], o);
  const [bx, by] = project(b[0], b[1], b[2], o);
  return `M ${round(ax)} ${round(ay)} L ${round(bx)} ${round(by)}`;
}
