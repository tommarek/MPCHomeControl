// The House card's 3D view: a real, orbitable model of both floors built from /static/floorplan.json at
// the storeys' true heights (slabs with the stairwell cut out, walls with real window and door holes,
// glass in the windows, door leaves ajar), lit by the sun from where it actually stands over the house
// now. Room floors take the same temperature-against-band colour as the 2D plan (fpFill), heated rooms
// glow, and hovering or tapping a room opens the very same card. Loaded lazily by app.js (after the
// vendored three.min.js) only when someone picks 3D; it reads app.js's globals (fpState, fpFill,
// fpRoomState, fpOpenCard, ...) and never forks their logic. Rendering is on demand: no frame runs while
// nothing moves. Pure helpers first (unit-tested in dashboard_test.js), the scene after.

const FP3D_SLAB = 0.25; // floor slab thickness, m
const FP3D_LIFT = 3.4; // how far the upper floor rises in the lifted view, m
const FP3D_DOOR_HEAD = 2.0; // head of a door with no head_m in the plan, m
const FP3D_GARAGE_HEAD = 2.2; // a sectional garage door's head, m
const FP3D_AJAR = 0.35; // a swing door's leaf, as a fraction of its drawn 90-degree swing
const FP3D_GLASS_T = 0.03; // pane thickness, m
const FP3D_FLOORS = [['both', 'Both'], ['lifted', 'Lifted'], ['ground', 'Ground'], ['first', 'First']];

// ---- pure helpers ----
// Unit vector in the scene (x = plan x, y = up, z = plan y, which points down the screen in the plan)
// towards something at compass azimuth `az` and elevation `el` (degrees). The plan's screen-up faces
// azimuth `upAz` and its x axis faces upAz + 90 (floorplan.json `frame`).
function fp3dSceneDir(az, el, upAz) {
  const r = Math.PI / 180, a = (az - upAz) * r, c = Math.cos(el * r);
  return [c * Math.sin(a), Math.sin(el * r), -c * Math.cos(a)];
}
// Base height of each storey's floor (the top of its slab) and its clear height: the ground floor sits
// on the table at 0, each storey above stands on the one below plus a slab.
function fp3dFloorLevels(geom, slab) {
  const out = {};
  let base = 0;
  geom.floors.filter((f) => f.id !== 'roof').sort((a, b) => a.level - b.level).forEach((f) => {
    const h = f.height_m > 0 ? f.height_m : 2.5;
    out[f.id] = { base, height: h };
    base += h + slab;
  });
  return out;
}
// What fills an opening's hole through the wall, bottom to top: `solid` wall below a window's sill and
// above any head (the lintel), `glass` where a pane stands. Heights are metres above the storey's floor.
function fp3dOpeningParts(o, H) {
  const parts = [];
  const clampH = (v) => Math.max(0, Math.min(H, v));
  const add = (kind, y0, y1) => { if (y1 - y0 > 0.005) parts.push({ kind, y0, y1 }); };
  if (o.kind === 'window') {
    const sill = clampH(o.sill_m != null ? o.sill_m : 0.9);
    const head = Math.max(sill, clampH(o.head_m != null ? o.head_m : 2.3));
    add('solid', 0, sill);
    add('glass', sill, head);
    add('solid', head, H);
  } else {
    const head = clampH(o.head_m != null ? o.head_m : o.type === 'sectional' ? FP3D_GARAGE_HEAD : FP3D_DOOR_HEAD);
    if (o.type === 'glazed') add('glass', 0, head);
    else if (o.type === 'sectional') add('panel', 0, head);
    add('solid', head, H);
  }
  return parts;
}
// The opening's footprint through the wall as a plan polygon: jamb a to jamb b, `depth` across the wall
// (centred on the wall line; the full wall depth when omitted).
function fp3dFootprint(o, depth) {
  const h = (depth != null ? depth : o.depth_m) / 2, [nx, ny] = o.n;
  const P = (p, k) => [p[0] + nx * k, p[1] + ny * k];
  return [P(o.a, -h), P(o.b, -h), P(o.b, h), P(o.a, h)];
}
// A swing door's leaf from its hinge, turned `frac` of the way from closed to the drawn open position.
function fp3dLeaf(o, frac) {
  const { hinge: H, r, leaf: F, closed: C } = o.swing;
  const a0 = Math.atan2(C[1] - H[1], C[0] - H[0]);
  let d = Math.atan2(F[1] - H[1], F[0] - H[0]) - a0;
  while (d > Math.PI) d -= 2 * Math.PI;
  while (d <= -Math.PI) d += 2 * Math.PI;
  const a = a0 + d * frac;
  return [H, [H[0] + r * Math.cos(a), H[1] + r * Math.sin(a)]];
}
// A segment as a thin plan rectangle `w` wide (for stair treads and door leaves).
function fp3dStrip(p, q, w) {
  const dx = q[0] - p[0], dy = q[1] - p[1], L = Math.hypot(dx, dy) || 1;
  const nx = -dy / L * w / 2, ny = dx / L * w / 2;
  return [[p[0] + nx, p[1] + ny], [q[0] + nx, q[1] + ny], [q[0] - nx, q[1] - ny], [p[0] - nx, p[1] - ny]];
}
function fp3dInside(pt, poly) {
  let inside = false;
  for (let i = 0, j = poly.length - 1; i < poly.length; j = i++) {
    const [xi, yi] = poly[i], [xj, yj] = poly[j];
    if ((yi > pt[1]) !== (yj > pt[1]) && pt[0] < (xj - xi) * (pt[1] - yi) / (yj - yi) + xi) inside = !inside;
  }
  return inside;
}
// Inside the polygon or within `tol` of its outline (a stair line drawn along a stairwell's edge counts).
function fp3dWithin(pt, poly, tol) {
  if (fp3dInside(pt, poly)) return true;
  for (let i = 0, j = poly.length - 1; i < poly.length; j = i++) {
    const [ax, ay] = poly[j], [bx, by] = poly[i], dx = bx - ax, dy = by - ay;
    const t = Math.max(0, Math.min(1, ((pt[0] - ax) * dx + (pt[1] - ay) * dy) / (dx * dx + dy * dy || 1)));
    if (Math.hypot(pt[0] - ax - t * dx, pt[1] - ay - t * dy) <= tol) return true;
  }
  return false;
}
// A corner firebox from floorplan.json: the black frame's plan box (the faces' extent, standing a
// little proud of each glass face) and one glass pane per face, inset from the frame's edges.
function fp3dFireboxParts(fb) {
  const PROUD = 0.015, INSET = 0.04, GAP = 0.025;
  const xs = fb.faces.flatMap((f) => [f.a[0], f.b[0]]), ys = fb.faces.flatMap((f) => [f.a[1], f.b[1]]);
  let [x0, x1, y0, y1] = [Math.min(...xs), Math.max(...xs), Math.min(...ys), Math.max(...ys)];
  for (const f of fb.faces) {
    if (f.n[0] < 0) x0 -= PROUD; if (f.n[0] > 0) x1 += PROUD;
    if (f.n[1] < 0) y0 -= PROUD; if (f.n[1] > 0) y1 += PROUD;
  }
  const panes = fb.faces.map((f) => {
    const L = Math.hypot(f.b[0] - f.a[0], f.b[1] - f.a[1]) || 1;
    const u = [(f.b[0] - f.a[0]) / L, (f.b[1] - f.a[1]) / L];
    const at = (p, k) => [p[0] + u[0] * k + f.n[0] * GAP, p[1] + u[1] * k + f.n[1] * GAP];
    return { a: at(f.a, INSET), b: at(f.b, -INSET), n: f.n, y0: fb.sill_m + INSET, y1: fb.head_m - INSET };
  });
  return { frame: [[x0, y0], [x1, y0], [x1, y1], [x0, y1]], y0: fb.sill_m, y1: fb.head_m, panes };
}
// Ember flicker for a lit firebox, 0..1: a few incommensurate sines, so it never visibly repeats.
function fp3dFlicker(ms) {
  const t = ms / 1000;
  const v = 0.55 + 0.2 * Math.sin(t * 7.3) + 0.15 * Math.sin(t * 13.1 + 1.7) + 0.1 * Math.sin(t * 2.9 + 0.4);
  return Math.max(0, Math.min(1, v));
}
// Vertex colour multiplier for an outside face with sun incidence `inc` (0..1): white when unlit, a
// warm, slightly brighter tint towards full sun (the wall colour is the material's, this multiplies it).
function fp3dSunTint(inc) {
  const k = Math.max(0, Math.min(1, inc)) * 0.9;
  return [1 + 0.2 * k, 1 - 0.05 * k, 1 - 0.32 * k];
}
function fp3dSegDist(p, a, b) {
  const dx = b[0] - a[0], dy = b[1] - a[1];
  const t = Math.max(0, Math.min(1, ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / (dx * dx + dy * dy || 1)));
  return Math.hypot(p[0] - a[0] - t * dx, p[1] - a[1] - t * dy);
}
// A CSS colour token (#rgb, #rrggbb, rgb(), rgba()) as { hex: '#rrggbb', alpha }, or null.
function fp3dParseColor(s) {
  s = String(s || '').trim();
  let m = s.match(/^#([0-9a-f]{3}|[0-9a-f]{6})$/i);
  if (m) {
    const h = m[1].length === 3 ? m[1].split('').map((c) => c + c).join('') : m[1];
    return { hex: '#' + h.toLowerCase(), alpha: 1 };
  }
  m = s.match(/^rgba?\(\s*([\d.]+)[\s,]+([\d.]+)[\s,]+([\d.]+)(?:[\s,/]+([\d.]+%?))?\s*\)$/i);
  if (!m) return null;
  const hex = '#' + [m[1], m[2], m[3]].map((v) => Math.max(0, Math.min(255, Math.round(+v))).toString(16).padStart(2, '0')).join('');
  const a = m[4] == null ? 1 : m[4].endsWith('%') ? parseFloat(m[4]) / 100 : +m[4];
  return { hex, alpha: Math.max(0, Math.min(1, a)) };
}
// How much daylight there is: 0 at night (sun 6 degrees or more below the horizon), 1 once the sun is
// 6 degrees up, eased in between (civil twilight into the low sun).
function fp3dDayFactor(el) {
  const t = Math.max(0, Math.min(1, (el + 6) / 12));
  return t * t * (3 - 2 * t);
}
// The scene's one direct light: the sun down to 6 degrees, then an eased swing over the top to the
// moon-ish fill opposite the set sun, complete 4 degrees below the horizon (`moon` 0..1 is how far
// across). The sun is held at least 2 degrees up so its light never comes from under the table. A
// fixed afternoon light without a site.
function fp3dLightDir(sun, upAz) {
  if (!sun) return { dir: fp3dSceneDir(215, 40, upAz), moon: 0 };
  const t = Math.max(0, Math.min(1, (6 - sun.elevation) / 10)), moon = t * t * (3 - 2 * t);
  const s = fp3dSceneDir(sun.azimuth, Math.max(2, sun.elevation), upAz);
  const m = fp3dSceneDir((sun.azimuth + 180) % 360, 38, upAz);
  const v = s.map((x, i) => x + (m[i] - x) * moon), n = Math.hypot(...v);
  return { dir: v.map((x) => x / n), moon };
}
// The instant of wall-clock hour `h` on the local day that starts at `dayStart` (dayStart + h hours is
// an hour off on the days the clocks change).
function fp3dClockHour(dayStart, h) {
  const d = new Date(dayStart);
  d.setHours(h, 0, 0, 0);
  return d.getTime();
}
// ---- the scene ----
const fp3d = (() => {
  let S = null; // everything the mounted view owns; null when not mounted

  const T = () => window.THREE;
  const token = (name, fallback) => {
    const v = getComputedStyle(document.getElementById('fp-card') || document.documentElement).getPropertyValue(name);
    return fp3dParseColor(v) || fp3dParseColor(fallback) || { hex: '#888888', alpha: 1 };
  };
  const reduced = () => typeof fpReduced === 'function' && fpReduced();
  // the instant the sun is drawn for: the time bar's (fpSunMs: now or ?fpSunTime=, moved by the bar)
  const sunTime = () => fpSunMs();

  // ---- geometry ----
  function makeShape(poly, c) {
    const TH = T();
    return new TH.Shape(poly.map((p) => new TH.Vector2(p[0] - c[0], -(p[1] - c[1]))));
  }
  // A plan polygon stood up between heights y0 and y1 (scene: x = plan x, y up, z = plan y).
  function prism(poly, y0, y1, c, holes) {
    const TH = T();
    const shape = makeShape(poly, c);
    for (const h of holes || []) shape.holes.push(new TH.Path(h.map((p) => new TH.Vector2(p[0] - c[0], -(p[1] - c[1])))));
    const g = new TH.ExtrudeGeometry(shape, { depth: Math.max(0.001, y1 - y0), bevelEnabled: false, curveSegments: 1 });
    g.rotateX(-Math.PI / 2);
    g.translate(0, y0, 0);
    return g;
  }
  // Inside, the floors are lit by the sky alone; the sun only lands on them through the glass. A low sun
  // adds little to a horizontal floor, so its patches are boosted fourfold to read clearly on the room colour.
  function sunPatchBoost(shader) {
    shader.fragmentShader = shader.fragmentShader.replace('#include <lights_fragment_end>', '#include <lights_fragment_end>\n\treflectedLight.directDiffuse *= 4.0;');
  }
  function merged(list) {
    const TH = T();
    const ok = list.filter((g) => g && g.attributes.position && g.attributes.position.count);
    if (!ok.length) return null;
    const m = TH.mergeGeometries(ok, false);
    ok.forEach((g) => g.dispose());
    return m;
  }

  function buildFloor(f, lvl, c, mats) {
    const TH = T();
    const H = lvl.height;
    const group = new TH.Group();
    group.userData.floor = f.id;
    const fl = { id: f.id, f, group, base: lvl.base, height: H, lift: 0, fade: 1, rooms: [], mats: [], occluders: [] };
    const own = (m) => { fl.mats.push({ m, base: m.opacity, transparent: m.transparent, depthWrite: m.depthWrite }); return m; };
    // slab with the stairwell cut out
    const slabMat = own(mats.slab.clone());
    const slab = new TH.Mesh(prism(f.outline, -FP3D_SLAB, 0, c, (f.voids || []).map((v) => v.polygon)), slabMat);
    slab.castShadow = true; slab.receiveShadow = true;
    group.add(slab);
    fl.occluders.push(slab);
    const lip = new TH.LineSegments(new TH.EdgesGeometry(slab.geometry, 30), own(mats.lip.clone()));
    group.add(lip);
    // walls, with what stays of the wall around each opening (sill blocks and lintels)
    const walls = (f.walls || []).map((w) => prism(w, 0, H, c));
    const glass = [], panels = [];
    fl.panes = [];
    for (const o of f.openings || []) {
      for (const p of fp3dOpeningParts(o, H)) {
        if (p.kind === 'solid') walls.push(prism(fp3dFootprint(o), p.y0, p.y1, c));
        else if (p.kind === 'glass') {
          // one mesh per pane, so a sunlit window can glint on its own
          const g = prism(fp3dFootprint(o, FP3D_GLASS_T), p.y0, p.y1, c);
          glass.push(g.clone());
          const m = own(mats.glass.clone());
          const gm = new TH.Mesh(g, m);
          gm.renderOrder = 2;
          group.add(gm);
          fl.panes.push({ o, mat: m });
        }
        else panels.push(prism(fp3dFootprint(o, 0.05), p.y0, p.y1, c));
      }
      if (o.type === 'swing' && o.swing) {
        const [h, e] = fp3dLeaf(o, FP3D_AJAR);
        panels.push(prism(fp3dStrip(h, e, 0.04), 0.01, Math.min(H, FP3D_DOOR_HEAD) - 0.02, c));
      }
    }
    const wallGeo = merged(walls);
    if (wallGeo) {
      // which triangles are outside faces (on an outline edge, facing out): the sun tints those
      const facades = fpOutlineFacades(f.outline);
      const pos = wallGeo.attributes.position, nor = wallGeo.attributes.normal;
      const face = new Int8Array(pos.count).fill(-1);
      for (let i = 0; i + 2 < pos.count; i += 3) {
        if (Math.abs(nor.getY(i)) > 0.5) continue;
        const pc = [(pos.getX(i) + pos.getX(i + 1) + pos.getX(i + 2)) / 3 + c[0], (pos.getZ(i) + pos.getZ(i + 1) + pos.getZ(i + 2)) / 3 + c[1]];
        const k = facades.findIndex((F) => nor.getX(i) * F.n[0] + nor.getZ(i) * F.n[1] > 0.95 && fp3dSegDist(pc, F.a, F.b) < 0.03);
        if (k >= 0) face[i] = face[i + 1] = face[i + 2] = k;
      }
      wallGeo.setAttribute('color', new TH.Float32BufferAttribute(new Float32Array(pos.count * 3).fill(1), 3));
      fl.facade = { geo: wallGeo, face, facades };
      const wm = new TH.Mesh(wallGeo, own(mats.wall.clone()));
      wm.castShadow = true; wm.receiveShadow = true;
      group.add(wm);
      fl.occluders.push(wm);
      group.add(new TH.LineSegments(new TH.EdgesGeometry(wallGeo, 25), own(mats.edge.clone())));
    }
    const panelGeo = merged(panels);
    if (panelGeo) {
      const pm = new TH.Mesh(panelGeo, own(mats.door.clone()));
      pm.castShadow = true; pm.receiveShadow = true;
      group.add(pm);
    }
    const glassGeo = merged(glass);
    if (glassGeo) {
      group.add(new TH.LineSegments(new TH.EdgesGeometry(glassGeo, 30), own(mats.frame.clone())));
      glassGeo.dispose();
    }
    // stair treads, where they stand on this floor (not drawn across its own stairwell)
    const voids = (f.voids || []).map((v) => v.polygon);
    const treads = ((f.details && f.details.stairs) || [])
      .filter((s) => !voids.some((v) => fp3dWithin([(s[0][0] + s[1][0]) / 2, (s[0][1] + s[1][1]) / 2], v, 0.06)))
      .map((s) => prism(fp3dStrip(s[0], s[1], 0.035), 0, 0.03, c));
    const treadGeo = merged(treads);
    if (treadGeo) group.add(new TH.Mesh(treadGeo, own(mats.tread.clone())));
    // fixtures: the fireplace's chimney block with its firebox, plinth, hatch and vent; the chimney
    // rising through an upper floor (from the slab's underside, so it passes through it)
    for (const x of f.features || []) {
      const solid = [], dark = [];
      if (x.kind === 'fireplace') solid.push(prism(x.polygon, 0, H, c));
      else if (x.kind === 'chimney') solid.push(prism(x.polygon, -FP3D_SLAB, H, c));
      for (const d of x.details || []) {
        const poly = fp3dStrip([d.a[0] + d.n[0] * 0.005, d.a[1] + d.n[1] * 0.005], [d.b[0] + d.n[0] * 0.005, d.b[1] + d.n[1] * 0.005], 0.012);
        (d.kind === 'vent' ? dark : solid).push(prism(poly, d.y0, d.y1, c));
      }
      const blockGeo = merged(solid);
      if (blockGeo) {
        const bm = new TH.Mesh(blockGeo, own(mats.chimney.clone()));
        bm.castShadow = true; bm.receiveShadow = true;
        group.add(bm);
        fl.occluders.push(bm);
        group.add(new TH.LineSegments(new TH.EdgesGeometry(blockGeo, 25), own(mats.edge.clone())));
      }
      if (x.plinth) {
        const p = x.plinth;
        dark.push(prism(p.polygon, p.top_m - p.thickness_m, p.top_m, c));
      }
      if (x.firebox) {
        const fbp = fp3dFireboxParts(x.firebox);
        dark.push(prism(fbp.frame, fbp.y0, fbp.y1, c));
        const panes = merged(fbp.panes.map((q) => prism(fp3dStrip(q.a, q.b, 0.012), q.y0, q.y1, c)));
        if (panes) {
          const m = own(mats.fireglass.clone());
          group.add(new TH.Mesh(panes, m));
          fl.fires = (fl.fires || []).concat([{ zone: x.zone, mat: m }]);
        }
      }
      const darkGeo = merged(dark);
      if (darkGeo) {
        const dm = new TH.Mesh(darkGeo, own(mats.stone.clone()));
        dm.castShadow = true; dm.receiveShadow = true;
        group.add(dm);
      }
    }
    // room floors: coloured per room, the pick targets
    for (const r of f.rooms) {
      const g = new TH.ShapeGeometry(makeShape(r.polygon, c));
      g.rotateX(-Math.PI / 2);
      g.translate(0, 0.012, 0);
      const m = own(mats.room.clone());
      m.onBeforeCompile = sunPatchBoost;
      m.customProgramCacheKey = () => 'fp3d-floor';
      const mesh = new TH.Mesh(g, m);
      mesh.receiveShadow = true;
      mesh.userData.zone = r.zone;
      group.add(mesh);
      const ring = new TH.LineSegments(new TH.EdgesGeometry(g), own(mats.ring.clone()));
      ring.position.y = 0.02;
      ring.visible = false;
      group.add(ring);
      fl.rooms.push({ zone: r.zone, room: r, mesh, mat: m, ring, heating: false, nosensor: false });
    }
    // an invisible ceiling that only casts shadow: the sun reaches the outside faces and, through the
    // glass, the floors, never the open-topped rooms from above
    const capGeo = new TH.ShapeGeometry(makeShape(f.outline, c));
    capGeo.rotateX(-Math.PI / 2);
    capGeo.translate(0, H + 0.002, 0);
    const cap = new TH.Mesh(capGeo, mats.cap);
    cap.castShadow = true;
    group.add(cap);
    group.position.y = lvl.base;
    return fl;
  }

  // the table under the model: shadows land on it, a faint metre grid fades out around the house
  function buildTable(R, mats) {
    const TH = T();
    const g = new TH.Group();
    const shadow = new TH.Mesh(new TH.CircleGeometry(R * 2.4, 64), mats.shadow);
    shadow.rotateX(-Math.PI / 2);
    shadow.position.y = -FP3D_SLAB - 0.002;
    shadow.receiveShadow = true;
    g.add(shadow);
    const pos = [], col = [];
    const ext = Math.ceil(R * 1.7), fade = (x, z) => Math.pow(Math.max(0, 1 - Math.hypot(x, z) / (R * 1.7)), 1.6);
    for (let k = -ext; k <= ext; k++) {
      for (let s = -ext; s < ext; s++) {
        for (const [x0, z0, x1, z1] of [[k, s, k, s + 1], [s, k, s + 1, k]]) {
          pos.push(x0, 0, z0, x1, 0, z1);
          col.push(1, 1, 1, fade(x0, z0), 1, 1, 1, fade(x1, z1));
        }
      }
    }
    const gg = new TH.BufferGeometry();
    gg.setAttribute('position', new TH.Float32BufferAttribute(pos, 3));
    gg.setAttribute('color', new TH.Float32BufferAttribute(col, 4));
    const grid = new TH.LineSegments(gg, mats.grid);
    grid.position.y = -FP3D_SLAB - 0.001;
    g.add(grid);
    return g;
  }

  function hatchTexture() {
    const TH = T();
    const cv = document.createElement('canvas');
    cv.width = cv.height = 64;
    const x = cv.getContext('2d');
    x.fillStyle = '#fff'; x.fillRect(0, 0, 64, 64);
    x.strokeStyle = 'rgba(0,0,0,0.32)'; x.lineWidth = 5;
    for (let i = -64; i <= 128; i += 22) { x.beginPath(); x.moveTo(i, 64); x.lineTo(i + 64, 0); x.stroke(); }
    const t = new TH.CanvasTexture(cv);
    t.wrapS = t.wrapT = TH.RepeatWrapping;
    t.repeat.set(2.2, 2.2);
    t.colorSpace = TH.SRGBColorSpace;
    return t;
  }

  function makeMaterials() {
    const TH = T();
    const lam = (o) => new TH.MeshLambertMaterial(o);
    const m = {
      slab: lam({ color: 0x222a3a }),
      lip: new TH.LineBasicMaterial({ color: 0xaabbdd, transparent: true, opacity: 0.3 }),
      wall: lam({ color: 0xc3ccdc, vertexColors: true }),
      cap: lam({ colorWrite: false, depthWrite: false, side: TH.DoubleSide }),
      compass: new TH.LineBasicMaterial({ color: 0xc3d2f0, transparent: true, opacity: 0.35, depthWrite: false }),
      arc: new TH.LineBasicMaterial({ color: 0xffc46b, transparent: true, opacity: 0.75, depthWrite: false }),
      ray: new TH.LineBasicMaterial({ color: 0xffc46b, transparent: true, opacity: 0.28, depthWrite: false }),
      edge: new TH.LineBasicMaterial({ color: 0x343e54, transparent: true, opacity: 0.5 }),
      door: lam({ color: 0x9aa5ba }),
      tread: lam({ color: 0x8892a8 }),
      glass: new TH.MeshStandardMaterial({ color: 0xa6ecff, roughness: 0.08, metalness: 0, transparent: true, opacity: 0.26, depthWrite: false, side: TH.DoubleSide }),
      frame: new TH.LineBasicMaterial({ color: 0xd6eeff, transparent: true, opacity: 0.55 }),
      room: lam({ color: 0x262d3c }),
      chimney: lam({ color: 0xd2d6de }),
      stone: new TH.MeshStandardMaterial({ color: 0x0c0d10, roughness: 0.35, metalness: 0.1 }),
      fireglass: new TH.MeshStandardMaterial({ color: 0x16120f, roughness: 0.1, metalness: 0.2, emissive: 0x000000 }),
      ring: new TH.LineBasicMaterial({ color: 0xffffff, transparent: true, opacity: 0.95, depthTest: false }),
      shadow: new TH.ShadowMaterial({ opacity: 0.32 }),
      grid: new TH.LineBasicMaterial({ vertexColors: true, transparent: true, opacity: 0.1, depthWrite: false }),
    };
    for (const k in m) m[k].userData.role = k; // clones keep it: update() recolours each floor's copies by role
    return m;
  }

  // ---- DOM ----
  function mountDom(stage) {
    const wrap = document.createElement('div');
    wrap.className = 'fp3';
    wrap.id = 'fp3';
    const isTouch = !(window.matchMedia && matchMedia('(hover: hover) and (pointer: fine)').matches);
    wrap.innerHTML = `
      <div class="fp3-view">
        <canvas class="fp3-canvas" role="img" tabindex="0" aria-label="3D model of the house, both floors, room floors coloured by temperature against the comfort band, lit by the sun from where it stands. Drag to turn it, scroll or pinch to zoom; the room list below the legend has every reading."></canvas>
        <div class="fp3-labels" id="fp3-labels"></div>
        <div class="fp3-tools">
          <div class="fp3-floors" role="group" aria-label="Floors shown">${FP3D_FLOORS.map(([k, t]) => `<button type="button" data-floor3="${k}" aria-pressed="false">${t}</button>`).join('')}</div>
          <button type="button" class="fp3-reset" aria-label="Reset view" title="Reset view"><svg viewBox="0 0 16 16" aria-hidden="true"><path d="M3.2 6.2A5 5 0 1 1 3 9.6"/><path d="M2.6 2.8v3.6h3.6"/></svg></button>
        </div>
        <div class="fp3-sunbar">
          <div class="fp3-sun" id="fp3-sun"></div>
        </div>
        <div class="fp3-hint" id="fp3-hint">${isTouch ? 'Swipe sideways to turn, pinch to zoom' : 'Drag to turn, scroll to zoom, right-drag to pan'}</div>
      </div>
`;
    stage.appendChild(wrap);
    return wrap;
  }

  // ---- mount ----
  // Throws (leaving nothing behind) when WebGL 2 is missing or the scene cannot be built.
  function mount(stage) {
    if (S) dispose();
    if (!window.THREE || !window.WebGL2RenderingContext) throw new Error('webgl2 unavailable');
    const wrap = mountDom(stage);
    try {
      return build(wrap);
    } catch (e) {
      if (S) dispose(); else wrap.remove();
      throw e;
    }
  }
  function build(wrap) {
    const TH = T();
    const t0 = performance.now();
    const canvas = wrap.querySelector('canvas');
    // ask for the context ourselves: a browser without WebGL 2 gets the stacked fallback, not three's console errors
    let context = null;
    try { context = canvas.getContext('webgl2', { antialias: true, alpha: true, powerPreference: 'low-power' }); } catch (e) { context = null; }
    if (!context) throw new Error('webgl2 unavailable');
    let renderer = null;
    try {
      renderer = new TH.WebGLRenderer({ canvas, context, antialias: true, alpha: true, powerPreference: 'low-power' });
      return buildScene(wrap, renderer, t0);
    } catch (e) {
      // once S is set, mount's dispose() frees everything; before that only the context is held
      if (!S) {
        if (renderer) renderer.dispose();
        const lose = context.getExtension('WEBGL_lose_context');
        if (lose) lose.loseContext();
      }
      throw e;
    }
  }
  function buildScene(wrap, renderer, t0) {
    const TH = T();
    const st = fpState, geom = st.geom;
    const canvas = renderer.domElement;
    renderer.setClearColor(0x000000, 0);
    renderer.outputColorSpace = TH.SRGBColorSpace;
    renderer.toneMapping = TH.NoToneMapping;
    renderer.shadowMap.enabled = true;
    renderer.shadowMap.autoUpdate = false; // renderFrame asks for it on every frame that can move a shadow
    renderer.shadowMap.type = TH.PCFShadowMap;
    renderer.setPixelRatio(Math.min(2, window.devicePixelRatio || 1));

    const tGeo = performance.now();
    const scene = new TH.Scene();
    const camera = new TH.PerspectiveCamera(30, 1, 0.1, 400);
    const b = geom.bounds;
    const c = [(b.min[0] + b.max[0]) / 2, (b.min[1] + b.max[1]) / 2];
    const R = Math.hypot(b.max[0] - b.min[0], b.max[1] - b.min[1]) / 2;
    const levels = fp3dFloorLevels(geom, FP3D_SLAB);
    const mats = makeMaterials();
    const hatch = hatchTexture();
    const floors = fpFloors(geom).map((f) => buildFloor(f, levels[f.id], c, mats));
    floors.forEach((fl) => scene.add(fl.group));
    const top = Math.max(...floors.map((fl) => fl.base + fl.height));
    scene.add(buildTable(R, mats));

    const hemi = new TH.HemisphereLight(0xdfe8ff, 0x30343e, 1.4);
    scene.add(hemi);
    const sun = new TH.DirectionalLight(0xfff4e6, 2);
    sun.castShadow = true;
    const big = wrap.getBoundingClientRect().width >= 600;
    sun.shadow.mapSize.set(big ? 2048 : 1024, big ? 2048 : 1024);
    sun.shadow.radius = 3;
    sun.shadow.bias = -0.0004;
    sun.shadow.normalBias = 0.03;
    const sc = sun.shadow.camera;
    sc.left = -R - 2; sc.right = R + 2; sc.top = R + 2; sc.bottom = -R - 2; sc.near = 1; sc.far = 4 * R + 40;
    sun.target.position.set(0, top / 3, 0);
    scene.add(sun, sun.target);

    const controls = new TH.OrbitControls(camera, canvas);
    controls.enableDamping = true;
    controls.dampingFactor = 0.09;
    controls.screenSpacePanning = true;
    controls.maxPolarAngle = 82 * Math.PI / 180;
    controls.minPolarAngle = 0.02;
    controls.zoomSpeed = 0.9;
    controls.zoomToCursor = true;
    canvas.style.touchAction = 'pan-y'; // one finger up/down still scrolls the page; sideways turns the model

    S = {
      wrap, canvas, renderer, scene, camera, controls, hemi, sun, floors, mats, hatch, c, R, top, levels,
      t0, raf: 0, pulseT: 0, anims: [], floorMode: st.floor3d || 'both', userMoved: false, hot: null,
      labels: {}, w: 0, h: 0, inView: true, down: null, lastTheme: null, ro: null, io: null, offs: [],
      view: wrap.querySelector('.fp3-view'), arcDay: null,
    };
    buildLabels();
    buildSky();
    wire();
    resize(true);
    applyFloorMode(false);
    update();
    // a row hovered or pinned while three.js was loading set fpState.hot without reaching the scene
    if (st.hot) setHot(st.hot, true);
    // buildMs: everything up to the first frame; sceneMs: the geometry, materials and labels alone
    S.stats = { buildMs: Math.round(performance.now() - t0), sceneMs: Math.round(performance.now() - tGeo), triangles: 0 };
    window.__fp3dStats = S.stats;
    return S.stats;
  }

  function buildLabels() {
    const box = S.wrap.querySelector('#fp3-labels');
    for (const fl of S.floors) {
      for (const rm of fl.rooms) {
        const el = document.createElement('button');
        el.type = 'button';
        el.className = 'fp3-lab';
        el.dataset.zone = rm.zone;
        el.innerHTML = '<span class="fp3-name"></span><span class="fp3-temp"><i class="fp3-flame" aria-hidden="true"></i><b></b><span class="fp3-deg">°</span></span><span class="fp3-kw"></span>';
        box.appendChild(el);
        S.labels[rm.zone] = { el, name: el.querySelector('.fp3-name'), temp: el.querySelector('b'), kw: el.querySelector('.fp3-kw'), rm, fl, w: 0, h: 0 };
      }
    }
    const mark = (cls, text) => {
      const e = document.createElement('div');
      e.className = cls;
      e.textContent = text;
      e.setAttribute('aria-hidden', 'true');
      box.appendChild(e);
      return e;
    };
    S.compassMarks = [['N', 0], ['E', 90], ['S', 180], ['W', 270]].map(([t, az]) => ({ el: mark('fp3-north' + (t === 'N' ? ' is-n' : ''), t), az }));
    S.hourMarks = [8, 12, 16].map((h) => ({ el: mark('fp3-hour', String(h)), h, p: null }));
    S.sunDisc = mark('fp3-sundisc', '');
  }

  // ---- the sky: a compass ring on the table and today's sun path over the house ----
  const ringR = () => S.R * 1.12;
  function buildSky() {
    const TH = T();
    const g = new TH.Group();
    const pos = [], y = -FP3D_SLAB + 0.004, r = ringR();
    for (let i = 0; i < 96; i++) {
      const a0 = i / 96 * Math.PI * 2, a1 = (i + 1) / 96 * Math.PI * 2;
      pos.push(Math.cos(a0) * r, y, Math.sin(a0) * r, Math.cos(a1) * r, y, Math.sin(a1) * r);
    }
    const up = fpState.geom.frame ? fpState.geom.frame.up_azimuth_deg : 0;
    for (let az = 0; az < 360; az += 45) {
      const d = fp3dSceneDir(az, 0, up), k = az % 90 ? 0.35 : 0.7;
      pos.push(d[0] * r, y, d[2] * r, d[0] * (r - k), y, d[2] * (r - k));
    }
    const rg = new TH.BufferGeometry();
    rg.setAttribute('position', new TH.Float32BufferAttribute(pos, 3));
    g.add(new TH.LineSegments(rg, S.mats.compass));
    S.arc = new TH.LineSegments(new TH.BufferGeometry(), S.mats.arc);
    g.add(S.arc);
    const ray = new TH.BufferGeometry();
    ray.setAttribute('position', new TH.Float32BufferAttribute([0, 0, 0, 0, 0, 0], 3));
    S.ray = new TH.LineSegments(ray, S.mats.ray);
    S.ray.frustumCulled = false; // its endpoints move in place; three would keep the first bounding sphere
    g.add(S.ray);
    S.scene.add(g);
  }
  // scene point of the sun on the sky dome at time ms (radius = the compass ring)
  function skyPoint(sun) {
    const up = fpState.geom.frame ? fpState.geom.frame.up_azimuth_deg : 0;
    const d = fp3dSceneDir(sun.azimuth, sun.elevation, up), r = ringR();
    return new (T().Vector3)(d[0] * r, d[1] * r - FP3D_SLAB, d[2] * r);
  }
  // today's path from sunrise to sunset as short dashes, plus where the 8, 12 and 16 o'clock ticks sit
  function buildArc(dayStart, site) {
    const TH = T();
    const pos = [];
    const at = (ms) => fpSunPosition(ms, site.lat, site.lon);
    for (let m = 0; m < 1440; m += 6) {
      const a = at(dayStart + m * 60000), b = at(dayStart + (m + 3) * 60000);
      if (a.elevation < -0.5 || b.elevation < -0.5) continue;
      const p = skyPoint(a), q = skyPoint(b);
      pos.push(p.x, p.y, p.z, q.x, q.y, q.z);
    }
    S.arc.geometry.dispose();
    S.arc.geometry = new TH.BufferGeometry();
    S.arc.geometry.setAttribute('position', new TH.Float32BufferAttribute(pos, 3));
    for (const hm of S.hourMarks) {
      const sun = at(fp3dClockHour(dayStart, hm.h));
      hm.p = sun.elevation > 0 ? skyPoint(sun) : null;
    }
    S.arcDay = dayStart;
  }

  // ---- events ----
  function on(el, ev, fn, opt) { el.addEventListener(ev, fn, opt); S.offs.push(() => el.removeEventListener(ev, fn, opt)); }
  function wire() {
    const { canvas, controls, wrap } = S;
    on(controls, 'change', () => {
      // panning or zooming to the cursor never carries the pivot off the table
      const t = controls.target, lim = S.R * 0.9;
      const k = Math.hypot(t.x, t.z);
      if (k > lim) { t.x *= lim / k; t.z *= lim / k; }
      t.y = Math.max(-FP3D_SLAB, Math.min(S.top + FP3D_LIFT, t.y));
      request();
    });
    on(controls, 'start', () => {
      S.userMoved = true;
      hideHint();
      if (!fpState.pinned) { fpSetHot(null); fpCloseSoft(); }
    });
    on(canvas, 'pointerdown', (ev) => { S.down = { x: ev.clientX, y: ev.clientY, t: performance.now(), id: ev.pointerId }; });
    // a tap or click (not the end of a drag) picks; on 'click', like the plan, so the sheet's scrim
    // appears after the tap's own click has landed
    on(canvas, 'click', (ev) => {
      const d = S.down;
      S.down = null;
      if (!d || ev.button > 0 || Math.hypot(ev.clientX - d.x, ev.clientY - d.y) > 7 || performance.now() - d.t > 650) return;
      tapAt(ev.clientX, ev.clientY);
    });
    on(canvas, 'pointermove', (ev) => {
      if (ev.pointerType === 'touch' || ev.buttons || fpState.pinned || fpIsSheet()) return;
      const zone = pick(ev.clientX, ev.clientY);
      canvas.style.cursor = zone ? 'pointer' : '';
      if (zone !== fpState.hot) {
        fpSetHot(zone);
        if (zone) fpOpenCard(zone, { pin: false, rect: rect(zone) }); else fpCloseSoft();
      }
    });
    on(canvas, 'pointerleave', () => { if (!fpState.pinned && !fpIsSheet()) { fpSetHot(null); fpCloseSoft(); } });
    on(canvas, 'webglcontextlost', (ev) => { ev.preventDefault(); if (typeof fpFallback2d === 'function') fpFallback2d('The 3D view lost its graphics context, so the 2D plan is shown instead.'); });
    on(wrap, 'click', (ev) => {
      const fb = ev.target.closest && ev.target.closest('[data-floor3]');
      if (fb) { S.floorMode = fpState.floor3d = fb.dataset.floor3; applyFloorMode(true); return; }
      if (ev.target.closest && ev.target.closest('.fp3-reset')) { resetView(); return; }
      // labels let the pointer through to the model; this is a keyboard Enter / Space on a focused label
      const lab = ev.target.closest && ev.target.closest('.fp3-lab');
      if (lab) openPinned(lab.dataset.zone, true);
    });
    on(wrap, 'focusin', (ev) => {
      const lab = ev.target.closest && ev.target.closest('.fp3-lab');
      if (lab && !fpState.pinned) fpSetHot(lab.dataset.zone);
    });
    on(wrap, 'focusout', (ev) => { if (ev.target.closest && ev.target.closest('.fp3-lab') && !fpState.pinned) fpSetHot(null); });
    on(document, 'visibilitychange', () => { if (!document.hidden) { schedulePulse(); request(); } });
    if (window.ResizeObserver) { S.ro = new ResizeObserver(() => resize(false)); S.ro.observe(S.view); }
    if (window.IntersectionObserver) {
      S.io = new IntersectionObserver((es) => { S.inView = es.some((e) => e.isIntersecting); if (S.inView) { schedulePulse(); request(); } });
      S.io.observe(wrap);
    }
  }
  function hideHint() { const h = S && S.wrap.querySelector('#fp3-hint'); if (h) h.classList.add('is-gone'); }
  function openPinned(zone, fromLabel) {
    const st = fpState;
    if (!zone) { if (st.pinned) fpCloseCard(false); return; }
    if (!fpIsSheet() && st.pinned === zone && !fromLabel) { fpCloseCard(false); return; }
    st.pinned = zone;
    fpSetHot(zone);
    fpOpenCard(zone, { pin: true, rect: rect(zone), from: fromLabel && S.labels[zone] ? S.labels[zone].el : null });
  }
  function tapAt(x, y) { openPinned(pick(x, y), false); }

  // ---- picking and projection ----
  function ndcOf(x, y) {
    const r = S.canvas.getBoundingClientRect();
    return new (T().Vector2)(((x - r.left) / r.width) * 2 - 1, -((y - r.top) / r.height) * 2 + 1);
  }
  // The room under a client point: a room's label counts as the room (labels let drags and wheels
  // through to the canvas, so they are hit-tested here), else the first room floor the ray meets
  // before any wall or slab.
  function pick(x, y) {
    if (!S) return null;
    for (const z in S.labels) {
      const L = S.labels[z];
      if (L.el.classList.contains('is-off')) continue;
      const r = L.el.getBoundingClientRect();
      if (x >= r.left && x <= r.right && y >= r.top && y <= r.bottom) return z;
    }
    const TH = T();
    const rc = new TH.Raycaster();
    rc.setFromCamera(ndcOf(x, y), S.camera);
    const targets = [];
    for (const fl of S.floors) {
      if (fl.fade < 0.5) continue;
      fl.rooms.forEach((rm) => targets.push(rm.mesh));
      targets.push(...fl.occluders);
    }
    const hit = rc.intersectObjects(targets, false)[0];
    return hit && hit.object.userData.zone ? hit.object.userData.zone : null;
  }
  // screen point (CSS px inside the canvas) of a scene point, or null when it is behind the camera
  const tmpV = () => new (T().Vector3)();
  function toScreen(v) {
    const p = v.clone().project(S.camera);
    if (p.z > 1 || p.z < -1) return null;
    return [(p.x + 1) / 2 * S.w, (1 - p.y) / 2 * S.h, p.z];
  }
  function roomPoint(rm, fl, x, y, dy) {
    return tmpV().set(x - S.c[0], fl.base + fl.lift + (dy || 0), y - S.c[1]);
  }
  // the room's projected footprint as a client rect (what the floating card sits beside)
  function rect(zone) {
    const L = S && S.labels[zone];
    if (!L) return null;
    const cr = S.canvas.getBoundingClientRect();
    if (L.fl.fade < 0.5) return { left: cr.left + cr.width / 2, top: cr.top + cr.height / 2, right: cr.left + cr.width / 2, bottom: cr.top + cr.height / 2 };
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const p of L.rm.room.polygon) {
      const q = toScreen(roomPoint(L.rm, L.fl, p[0], p[1]));
      if (!q) continue;
      x0 = Math.min(x0, q[0]); x1 = Math.max(x1, q[0]); y0 = Math.min(y0, q[1]); y1 = Math.max(y1, q[1]);
    }
    if (!isFinite(x0)) return { left: cr.left + cr.width / 2, top: cr.top + cr.height / 2, right: cr.left + cr.width / 2, bottom: cr.top + cr.height / 2 };
    x0 = Math.max(0, x0); y0 = Math.max(0, y0); x1 = Math.min(S.w, x1); y1 = Math.min(S.h, y1);
    return { left: cr.left + x0, top: cr.top + y0, right: cr.left + x1, bottom: cr.top + y1 };
  }
  const labelPoint = (L) => roomPoint(L.rm, L.fl, L.rm.room.label[0], L.rm.room.label[1], Math.min(1.1, L.fl.height / 2));
  // client point of plan (x, y) at height h above a floor (the screenshot tests aim with it)
  function pointOf(x, y, h, floorId) {
    const fl = S && S.floors.find((q) => q.id === (floorId || 'ground'));
    if (!fl) return null;
    const q = toScreen(tmpV().set(x - S.c[0], fl.base + fl.lift + (h || 0), y - S.c[1]));
    if (!q) return null;
    const cr = S.canvas.getBoundingClientRect();
    return { x: cr.left + q[0], y: cr.top + q[1] };
  }
  // Point the camera at plan (x, y, h above the ground floor) from plan direction (dx, dy), `polar`
  // degrees off vertical, `dist` metres away (a screenshot / test hook; counts as the user moving it).
  function aim(x, y, h, dx, dy, polar, dist) {
    if (!S) return;
    const TH = T(), n = Math.hypot(dx, dy) || 1, p = polar * Math.PI / 180;
    S.userMoved = true;
    placeCamera(new TH.Vector3(x - S.c[0], h, y - S.c[1]), new TH.Vector3(Math.sin(p) * dx / n, Math.cos(p), Math.sin(p) * dy / n), dist);
    request();
  }
  function anchorPoint(zone) {
    const L = S && S.labels[zone];
    if (!L) return null;
    const q = toScreen(labelPoint(L));
    if (!q) return null;
    const cr = S.canvas.getBoundingClientRect();
    return { x: cr.left + q[0], y: cr.top + q[1] };
  }

  // ---- camera ----
  // Distance that fits every corner of the house's box inside the view along the current direction.
  function fitDistance(dir) {
    const TH = T();
    const { camera, R, top } = S;
    const corners = [];
    const hx = (fpState.geom.bounds.max[0] - fpState.geom.bounds.min[0]) / 2;
    const hz = (fpState.geom.bounds.max[1] - fpState.geom.bounds.min[1]) / 2;
    const yTop = top + (S.floorMode === 'lifted' ? FP3D_LIFT : 0);
    for (const x of [-hx, hx]) for (const y of [-FP3D_SLAB, yTop]) for (const z of [-hz, hz]) corners.push(new TH.Vector3(x, y, z));
    const target = S.controls.target;
    const fits = (d) => {
      camera.position.copy(target).addScaledVector(dir, d);
      camera.lookAt(target);
      camera.updateMatrixWorld();
      return corners.every((v) => { const p = v.clone().project(camera); return p.z < 1 && Math.abs(p.x) < 0.94 && Math.abs(p.y) < 0.9; });
    };
    let lo = R * 0.5, hi = R * 12;
    for (let i = 0; i < 26; i++) { const mid = (lo + hi) / 2; if (fits(mid)) hi = mid; else lo = mid; }
    return hi;
  }
  function defaultDir() {
    // the stacked plan's bearing (the plan turned 20 degrees), from higher up so the open rooms read; a
    // portrait stage turns the house a quarter so its long side runs up the screen
    const TH = T();
    const portrait = S.w / Math.max(1, S.h) < 0.85;
    const polar = (portrait ? 40 : 44) * Math.PI / 180, th = (portrait ? 68 : -20) * Math.PI / 180;
    const hx = Math.tan(th), hz = 1, n = Math.hypot(hx, hz);
    return new TH.Vector3(Math.sin(polar) * hx / n, Math.cos(polar), Math.sin(polar) * hz / n).normalize();
  }
  // where the camera should stand along `dir` for the current floor mode (computed, not applied)
  function fitFor(dir) {
    const { controls, camera } = S;
    const keepPos = camera.position.clone(), keepT = controls.target.clone();
    controls.target.set(0, (S.floorMode === 'lifted' ? S.top + FP3D_LIFT : S.top) * 0.32, 0);
    const dist = fitDistance(dir), target = controls.target.clone();
    controls.target.copy(keepT);
    camera.position.copy(keepPos);
    camera.lookAt(keepT);
    camera.updateMatrixWorld();
    return { target, dist };
  }
  function placeCamera(target, dir, dist) {
    const { controls, camera, R } = S;
    controls.target.copy(target);
    camera.position.copy(target).addScaledVector(dir, dist);
    controls.minDistance = Math.max(4, dist * 0.28);
    controls.maxDistance = Math.max(dist * 2.6, R * 3);
    controls.update();
    // lookAt refreshes matrixWorld before it turns the camera: re-derive it so a pick or projection
    // made before the next render sees this view
    camera.updateMatrixWorld();
  }
  function frame(dir) {
    const f = fitFor(dir);
    placeCamera(f.target, dir, f.dist);
  }
  const viewDir = () => S.camera.position.clone().sub(S.controls.target).normalize();
  function resetView() {
    S.userMoved = false;
    frame(defaultDir());
    request();
  }
  function resize(first) {
    if (!S) return;
    const r = S.view.getBoundingClientRect();
    const w = Math.max(1, Math.round(r.width)), h = Math.max(1, Math.round(r.height));
    if (!first && w === S.w && h === S.h) return;
    S.w = w; S.h = h;
    S.renderer.setSize(w, h, false);
    S.camera.aspect = w / h;
    S.camera.updateProjectionMatrix();
    if (first || !S.userMoved) frame(first ? defaultDir() : viewDir());
    request();
  }

  // ---- floors: both / lifted / ground only / first only, animated ----
  function floorTargets(mode) {
    return S.floors.map((fl, i) => {
      if (i === 0) return { lift: 0, fade: mode === 'first' ? 0.14 : 1 };
      return { lift: mode === 'both' || mode === 'first' ? 0 : FP3D_LIFT, fade: mode === 'ground' ? 0 : 1 };
    });
  }
  function applyFloorMode(animate) {
    const mode = S.floorMode;
    S.wrap.querySelectorAll('[data-floor3]').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.floor3 === mode)));
    const to = floorTargets(mode);
    const from = S.floors.map((fl) => ({ lift: fl.lift, fade: fl.fade }));
    // an untouched camera reframes to the new extent along with the floors
    const dir = viewDir();
    const cam0 = { target: S.controls.target.clone(), dist: S.camera.position.distanceTo(S.controls.target) };
    const cam1 = S.userMoved ? null : fitFor(dir);
    if (!animate || reduced()) {
      S.floors.forEach((fl, i) => setFloor(fl, to[i].lift, to[i].fade));
      refitShadow();
      if (cam1) placeCamera(cam1.target, dir, cam1.dist);
      request();
      return;
    }
    const t0 = performance.now(), dur = 700;
    const ease = (x) => (x < 0.5 ? 4 * x * x * x : 1 - Math.pow(-2 * x + 2, 3) / 2);
    S.anims = [(now) => {
      const x = Math.min(1, (now - t0) / dur), k = ease(x);
      S.floors.forEach((fl, i) => setFloor(fl, from[i].lift + (to[i].lift - from[i].lift) * k, from[i].fade + (to[i].fade - from[i].fade) * k));
      refitShadow();
      if (cam1 && !S.userMoved) placeCamera(cam0.target.clone().lerp(cam1.target, k), dir, cam0.dist + (cam1.dist - cam0.dist) * k);
      return x < 1;
    }];
    request();
  }
  function setFloor(fl, lift, fade) {
    fl.lift = lift;
    fl.fade = fade;
    fl.group.position.y = fl.base + lift;
    fl.group.visible = fade > 0.01;
    for (const e of fl.mats) {
      const m = e.m;
      m.opacity = e.base * fade;
      // `transparent` is baked into the compiled program (an opaque one forces alpha 1), so a flip
      // must recompile; three caches both variants, so later flips reuse them
      const tr = e.transparent || fade < 0.999;
      if (m.transparent !== tr) { m.transparent = tr; m.needsUpdate = true; }
      m.depthWrite = e.depthWrite && fade > 0.6;
    }
    fl.group.traverse((o) => { if (o.isMesh) o.castShadow = fade > 0.6 && !o.material.transparent; });
    showRings(fl);
  }
  // the ring draws through slabs (depthTest off), so it shows only on a floor that is not faded out
  function showRings(fl) { for (const rm of fl.rooms) rm.ring.visible = rm.zone === S.hot && fl.fade > 0.5; }

  // ---- render on demand ----
  // A pulse frame only changes glow intensities, so it skips the shadow pass and the label pass; any
  // other request (camera, resize, data, floors, hover) makes the next frame a full one.
  function request() { if (S) { S.full = true; requestPulse(); } }
  function requestPulse() { if (S && !S.raf) S.raf = requestAnimationFrame(renderFrame); }
  function renderFrame(now) {
    if (!S) return;
    S.raf = 0;
    const full = S.full;
    S.full = false;
    const moving = S.controls.update();
    S.anims = S.anims.filter((a) => a(now));
    pulse(now);
    S.renderer.shadowMap.needsUpdate = full || moving || S.anims.length > 0;
    S.renderer.render(S.scene, S.camera);
    if (S.stats && !S.stats.triangles) {
      S.stats.triangles = S.renderer.info.render.triangles;
      S.stats.firstFrameMs = Math.round(performance.now() - S.t0); // includes compiling the shaders
    }
    if (full || moving || S.anims.length) placeLabels(moving || S.anims.length > 0);
    if (fpState.pinned && fpState.shown && !fpIsSheet() && (full || moving || S.anims.length)) {
      // a pinned zone with no 3D room (an attic chip) keeps the card beside what opened it
      const r = rect(fpState.pinned);
      if (r) fpPlaceFloat(r);
    }
    if (moving || S.anims.length) request();
  }
  // heated rooms breathe slowly and a lit fire flickers (a light timer, never a 60 fps loop; still
  // under reduced motion)
  function heatingRooms() { return S ? S.floors.flatMap((fl) => fl.rooms.filter((r) => r.heating)) : []; }
  function pulse(now) {
    if (S.fireLit) {
      const k = reduced() ? 0.6 : fp3dFlicker(now);
      for (const fl of S.floors) for (const fire of fl.fires || []) if (fire.lit) fire.mat.emissiveIntensity = 0.3 + 0.6 * k;
    }
    const list = heatingRooms();
    if (!list.length) return;
    const k = reduced() ? 0.6 : 0.5 + 0.5 * Math.sin(now / 3600 * 2 * Math.PI);
    const light = S.lastTheme === 'light';
    for (const rm of list) rm.mat.emissiveIntensity = ((light ? 0.05 : 0.06) + (light ? 0.08 : 0.1) * k) * (rm.heatK || 1) + (rm.zone === S.hot ? 0.04 : 0);
  }
  function schedulePulse() {
    if (!S || S.pulseT || reduced() || document.hidden || !S.inView || !(heatingRooms().length || S.fireLit)) return;
    S.pulseT = setTimeout(() => { if (!S) return; S.pulseT = 0; requestPulse(); schedulePulse(); }, 250);
  }

  // ---- labels: HTML over the canvas, projected on every full frame, thinned by the 2D plan's own resolver ----
  // Labels float a metre over their room's floor; one behind a wall or under a slab hides. The
  // occlusion rays run on every settled frame and every third frame while the model moves.
  function placeLabels(moving) {
    const TH = T();
    const items = [], prep = [];
    const rc = new TH.Raycaster();
    const eye = S.camera.position;
    const occluders = S.floors.filter((fl) => fl.fade > 0.6).flatMap((fl) => fl.occluders);
    const test = !moving || (S.tick = (S.tick || 0) + 1) % 3 === 0;
    for (const zone in S.labels) {
      const L = S.labels[zone];
      const fl = L.fl;
      const v = labelPoint(L);
      const q = fl.fade > 0.5 ? toScreen(v) : null;
      let visible = !!q && q[0] > -20 && q[0] < S.w + 20 && q[1] > -10 && q[1] < S.h + 10;
      if (visible && test) {
        const dir = v.clone().sub(eye), dist = dir.length();
        rc.set(eye, dir.normalize());
        rc.far = dist - 0.05;
        L.hidden = rc.intersectObjects(occluders, false).length > 0;
      }
      if (visible) visible = !L.hidden;
      if (!visible) { L.el.classList.add('is-off'); continue; }
      // projected room size decides whether the name fits
      let area = 0;
      const pts = L.rm.room.polygon.map((p) => toScreen(roomPoint(L.rm, fl, p[0], p[1]))).filter(Boolean);
      for (let i = 0, j = pts.length - 1; i < pts.length; j = i++) area += (pts[j][0] + pts[i][0]) * (pts[j][1] - pts[i][1]);
      const side = Math.sqrt(Math.abs(area) / 2);
      const r = fpState.byZone[zone], stt = fpRoomState(r);
      const name = L.rm.room.name || (r ? r.name : zone);
      const hasTemp = stt.temp != null, heating = !!(r && r.heating && r.heating.on);
      const levels = [];
      if (side >= Math.max(40, name.length * 5.6 * 0.82)) levels.push({ name: 'full', ...fpLabelBox(name, hasTemp, true, heating ? 11 : 0) });
      levels.push({ name: 'bare', ...fpLabelBox(name, hasTemp, false, heating ? 11 : 0) });
      levels.push({ name: 'compact', ...fpCompactBox(hasTemp, heating) });
      // the kW row (away from now) stacks under the temperature at every level: 9.5 px + 1 px margin
      if (L.kw.textContent) for (const lv of levels) lv.h += 11;
      items.push({ id: zone, x: q[0], y: q[1], prio: fpLabelPrio(r, L.rm.room.area_m2) + (zone === S.hot ? -5000 : 0), levels });
      prep.push([L, q]);
    }
    const picked = fpResolveLabels(items, 3);
    for (const [L, q] of prep) {
      const lv = picked[L.rm.zone] || 'hidden';
      L.el.classList.toggle('is-off', lv === 'hidden' && L.rm.zone !== S.hot);
      L.el.classList.toggle('is-compact', lv === 'compact' || lv === 'hidden');
      L.el.classList.toggle('has-name', lv === 'full');
      L.el.style.transform = `translate(${q[0].toFixed(1)}px, ${q[1].toFixed(1)}px) translate(-50%, -50%)`;
    }
    // the compass letter on the table, north of the house
    // the compass letters just outside the ring, the hour ticks on the sun path, the sun itself
    const up = fpState.geom.frame ? fpState.geom.frame.up_azimuth_deg : 0;
    const put = (el, v, occlude) => {
      let q = v && toScreen(v);
      if (q && occlude) {
        const dir = v.clone().sub(eye), dist = dir.length();
        rc.set(eye, dir.normalize());
        rc.far = dist - 0.05;
        if (rc.intersectObjects(occluders, false).length) q = null;
      }
      el.style.display = q ? '' : 'none';
      if (q) el.style.transform = `translate(${q[0].toFixed(1)}px, ${q[1].toFixed(1)}px) translate(-50%, -50%)`;
    };
    for (const m of S.compassMarks) {
      const d = fp3dSceneDir(m.az, 0, up), r = ringR() + 0.9;
      put(m.el, tmpV().set(d[0] * r, -FP3D_SLAB, d[2] * r), true);
    }
    for (const hm of S.hourMarks) put(hm.el, hm.p);
    put(S.sunDisc, S.sunPoint);
  }

  // ---- data, theme and light: repaint in place (no geometry is rebuilt) ----
  function update() {
    if (!S) return;
    const TH = T();
    const theme = fpTheme();
    const light = theme === 'light';
    S.lastTheme = theme;
    const col = (name, fb) => token(name, fb);
    const setC = (m, t) => { m.color.set(t.hex); };
    const M = S.mats;
    // per-floor clones carry their own colours: walk the floors' materials by role
    const role = {
      slab: col('--fp-slab', '#141b29'), wall: col('--fp-wall', '#c3ccdc'), edge: col('--fp-wall-side', '#343e54'),
      lip: col('--fp-slab-lip', 'rgba(170,195,240,.32)'), glass: col('--fp-glass', '#a6ecff'), frame: col('--fp-frame', 'rgba(214,238,255,.75)'),
      door: col('--fp-sill', '#8d98ad'), tread: col('--fp-line', 'rgba(195,210,240,.4)'), ring: col('--fp-label', '#f4f7fc'),
    };
    // the plastered chimney: the walls' white, a touch warmer
    role.chimney = { hex: '#' + new TH.Color(role.wall.hex).lerp(new TH.Color('#fff1dc'), 0.22).getHexString(), alpha: 1 };
    S.heatHex = col('--fp-heat', '#ff9440').hex;
    const grid = col('--fp-grid', 'rgba(150,172,215,.075)');
    M.grid.opacity = Math.min(1, grid.alpha * (light ? 2.6 : 3.2));
    M.grid.color.set(grid.hex);
    M.shadow.opacity = light ? 0.2 : 0.42;
    for (const fl of S.floors) {
      fl.group.traverse((o) => {
        const m = o.material;
        if (!m) return;
        const k = m.userData.role;
        if (k && role[k]) setC(m, role[k]);
      });
    }
    updateSun();
    // rooms
    for (const fl of S.floors) {
      for (const rm of fl.rooms) {
        const r = fpState.byZone[rm.zone];
        const stt = fpRoomState(r);
        rm.mat.color.set(fpFill(stt, theme));
        const noData = stt.kind === 'nosensor' || stt.kind === 'noband';
        if (rm.nosensor !== noData || !rm.painted) { rm.mat.map = noData ? S.hatch : null; rm.mat.needsUpdate = true; }
        rm.nosensor = noData;
        rm.painted = true;
        rm.heating = !!(r && r.heating && r.heating.on) && !stt.stale;
        // the glow scales with the power (planned or measured), never below a visible minimum
        rm.heatK = rm.heating ? Math.max(0.35, Math.min(1, (r.heating.kw || 0) / (r.max_heat_kw || r.heating.kw || 1))) : 0;
        paintGlow(rm);
        rm.ring.material.color.set(role.ring.hex);
        const L = S.labels[rm.zone];
        const name = rm.room.name || (r ? r.name : rm.zone);
        L.name.textContent = name;
        const away = fpState.tlKind && fpState.tlKind !== 'now';
        L.temp.textContent = stt.temp != null ? stt.temp.toFixed(1) : away ? 'no data' : stt.failed ? 'read failed' : 'no sensor';
        L.kw.textContent = away && r && r.heating && r.heating.on ? `${r.heating.kw.toFixed(1)} kW` : '';
        L.el.classList.toggle('is-plan', fpState.tlKind === 'plan');
        L.el.classList.toggle('is-none', stt.temp == null);
        L.el.classList.toggle('is-heating', !!(r && r.heating && r.heating.on));
        L.el.classList.toggle('is-stale', !!stt.stale);
        L.el.setAttribute('aria-label', r ? fpAria(r) : `${name}, no data`);
      }
    }
    // the fireplace: no sensor, so a lit fire is the estimator's unexplained heat (see fpFireLikely)
    S.fireLit = false;
    for (const fl of S.floors) {
      for (const fire of fl.fires || []) {
        fire.lit = !!fpFireLikely(fpState.byZone[fire.zone]);
        fire.mat.emissive.set(fire.lit ? '#ff5410' : '#000000');
        fire.mat.emissiveIntensity = fire.lit ? 0.6 : 0;
        S.fireLit = S.fireLit || fire.lit;
      }
    }
    schedulePulse();
    request();
  }
  // a heating room glows warm; the hovered one, when not heating, glows cool
  function paintGlow(rm) {
    const light = S.lastTheme === 'light';
    const hover = rm.zone === S.hot && !rm.heating;
    rm.mat.emissive.set(hover ? (light ? '#ffffff' : '#9fb4e0') : S.heatHex);
    rm.mat.emissiveIntensity = hover ? (light ? 0.12 : 0.16) : rm.heating ? (light ? 0.1 : 0.12) * rm.heatK : 0;
  }

  // ---- the sun: light, shadow frustum, outside faces, window glints, the path, the caption ----
  function updateSun() {
    if (!S) return;
    const TH = T();
    const light = S.lastTheme === 'light';
    const site = fpState.site && isFinite(fpState.site.lat) ? fpState.site : null;
    const t = sunTime();
    const sun = site ? fpSunPosition(t.ms, site.lat, site.lon) : null;
    const day = sun ? fp3dDayFactor(sun.elevation) : 0.85;
    const night = 1 - day;
    const up = fpState.geom.frame ? fpState.geom.frame.up_azimuth_deg : 0;
    const { dir: lightDir, moon } = fp3dLightDir(sun, up);
    const lowSun = sun ? Math.max(0, Math.min(1, sun.elevation / 35)) : 1;
    const sunCol = new TH.Color(light ? '#fff1dc' : '#ffe2bf').lerp(new TH.Color('#fff8ef'), lowSun);
    const moonCol = new TH.Color('#9fb6ff');
    S.sun.color.copy(sunCol).lerp(moonCol, Math.max(night * 0.9, moon));
    S.sun.intensity = (light ? 2.1 : 1.9) * day * (0.4 + 0.6 * Math.max(0.15, lowSun)) + 0.3 * night;
    const D = S.R * 2 + 20;
    S.sun.position.set(lightDir[0] * D, lightDir[1] * D + S.top / 3, lightDir[2] * D);
    S.lightDir = new TH.Vector3(...lightDir);
    fitShadow(S.lightDir);
    // inside the rooms only the sky lights (the caps keep the sun out), so the sky carries the colours
    S.hemi.color.set(light ? '#eef3ff' : '#dde5f7').lerp(new TH.Color('#8c9fd6'), night * 0.75);
    S.hemi.groundColor.set(light ? '#8a8f9a' : '#2a2e38').lerp(new TH.Color('#10131c'), night * 0.7);
    S.hemi.intensity = (light ? 3.1 : 2.8) * (0.6 + 0.4 * day);
    // the outside faces warm with the sun on them
    for (const fl of S.floors) {
      const F = fl.facade;
      if (!F) continue;
      const tints = F.facades.map((x) => fp3dSunTint(fpWallIncidence(x.n, sun, up)));
      const col = F.geo.attributes.color;
      for (let i = 0; i < F.face.length; i++) {
        const k = F.face[i];
        const c = k >= 0 ? tints[k] : null;
        col.setXYZ(i, c ? c[0] : 1, c ? c[1] : 1, c ? c[2] : 1);
      }
      col.needsUpdate = true;
    }
    // windows: a warm glint where the sun shines in, lit from inside at dusk
    const lit = new Set(fpSunlitWindows(fpFloors(fpState.geom), sun, up).map((x) => x.o));
    const glint = new TH.Color('#fff0c4'), warm = new TH.Color('#ffbf73');
    for (const fl of S.floors) {
      for (const pn of fl.panes || []) {
        const inc = lit.has(pn.o) ? fpWallIncidence(pn.o.n, sun, up) : 0;
        pn.mat.emissive.copy(inc > 0 ? glint : warm);
        pn.mat.emissiveIntensity = inc > 0 ? 0.25 + 0.75 * inc : 1.1 * night;
        const e = fl.mats.find((x) => x.m === pn.mat);
        if (e) { e.base = 0.26 + 0.4 * Math.max(night, inc * 0.6); pn.mat.opacity = e.base * fl.fade; }
      }
    }
    // the path and the sun on it
    const dayStart = fpLocalDayStart(t.ms);
    if (site && S.arcDay !== dayStart) buildArc(dayStart, site);
    S.arc.visible = !!site;
    S.sunPoint = sun && sun.elevation > -0.5 ? skyPoint(sun) : null;
    S.sunDisc.classList.toggle('is-low', !!sun && sun.elevation < 6);
    const rp = S.ray.geometry.attributes.position;
    if (S.sunPoint) { rp.setXYZ(0, S.sunPoint.x, S.sunPoint.y, S.sunPoint.z); rp.setXYZ(1, 0, S.top * 0.35, 0); rp.needsUpdate = true; }
    S.ray.visible = !!S.sunPoint && sun.elevation > 0;
    // caption and the rooms with sun on a window now
    const cap = S.wrap.querySelector('#fp3-sun');
    if (sun) {
      if (S.timesDay !== dayStart) { // two minute-scans per day, not per frame while the bar plays
        S.timesDay = dayStart;
        S.times = [fpSunTimes(dayStart, site.lat, site.lon), fpSunTimes(fpLocalDayStart(dayStart + 36 * 3600000), site.lat, site.lon)];
      }
      const [today, next] = S.times;
      const when = t.pinned && fpLocalDayStart(t.ms) !== fpLocalDayStart(Date.now()) ? `${new Date(t.ms).toLocaleDateString([], { month: 'short', day: 'numeric' })}, ` : '';
      cap.textContent = when + fpSunCaption(sun, t.ms, today, next, fpHm);
    }
    cap.hidden = !sun;
    S.mats.arc.opacity = sun && sun.elevation > 0 ? 0.75 : 0.35;
    S.wrap.classList.toggle('is-night', night > 0.6);
    request();
  }
  // Fit the sun's shadow camera tightly around the house and the shadow it throws on the table.
  function fitShadow(dir) {
    const TH = T(), cam = S.sun.shadow.camera;
    const b = fpState.geom.bounds, hx = (b.max[0] - b.min[0]) / 2 + 0.3, hz = (b.max[1] - b.min[1]) / 2 + 0.3;
    const yTop = S.top + Math.max(0, ...S.floors.map((fl) => fl.lift)) + 0.1, y0 = -FP3D_SLAB;
    const pts = [];
    for (const x of [-hx, hx]) for (const z of [-hz, hz]) {
      pts.push(new TH.Vector3(x, y0, z), new TH.Vector3(x, yTop, z));
      if (dir.y > 0.05) { // where the top corner's shadow lands, kept to the table
        const k = Math.min((yTop - y0) / dir.y, S.R * 2.2);
        pts.push(new TH.Vector3(x - dir.x * k, y0, z - dir.z * k));
      }
    }
    S.sun.updateMatrixWorld();
    S.sun.target.updateMatrixWorld();
    cam.position.copy(S.sun.position);
    cam.lookAt(S.sun.target.position);
    cam.updateMatrixWorld();
    let x0 = Infinity, x1 = -Infinity, yy0 = Infinity, yy1 = -Infinity, z0 = Infinity, z1 = -Infinity;
    for (const p of pts) {
      p.applyMatrix4(cam.matrixWorldInverse);
      x0 = Math.min(x0, p.x); x1 = Math.max(x1, p.x); yy0 = Math.min(yy0, p.y); yy1 = Math.max(yy1, p.y); z0 = Math.min(z0, p.z); z1 = Math.max(z1, p.z);
    }
    Object.assign(cam, { left: x0 - 0.2, right: x1 + 0.2, bottom: yy0 - 0.2, top: yy1 + 0.2, near: Math.max(0.1, -z1 - 1), far: -z0 + 1 });
    cam.updateProjectionMatrix();
  }
  // the box follows the upper floor's lift (before the first updateSun there is no light to fit)
  function refitShadow() { if (S.lightDir) fitShadow(S.lightDir); }

  function setHot(zone, onOff) {
    if (!S) return;
    const was = S.hot;
    S.hot = onOff ? zone : (S.hot === zone ? null : S.hot);
    for (const fl of S.floors) {
      showRings(fl);
      for (const rm of fl.rooms) if (rm.zone === was || rm.zone === S.hot) paintGlow(rm);
    }
    for (const z in S.labels) S.labels[z].el.classList.toggle('hot', z === S.hot);
    request();
  }

  function dispose() {
    if (!S) return;
    const s = S;
    S = null;
    cancelAnimationFrame(s.raf);
    clearTimeout(s.pulseT);
    s.offs.forEach((f) => f());
    if (s.ro) s.ro.disconnect();
    if (s.io) s.io.disconnect();
    s.controls.dispose();
    s.scene.traverse((o) => {
      if (o.geometry) o.geometry.dispose();
      if (o.material) (Array.isArray(o.material) ? o.material : [o.material]).forEach((m) => m.dispose());
    });
    Object.values(s.mats).forEach((m) => m.dispose());
    s.hatch.dispose();
    s.renderer.dispose();
    s.renderer.forceContextLoss();
    s.wrap.remove();
  }

  return {
    get active() { return !!S; },
    get stats() { return S ? S.stats : null; },
    // true while a frame or the heating pulse is scheduled (the idle check: false when nothing moves)
    get busy() { return !!(S && (S.raf || S.pulseT)); },
    mount, update, setHot, rect, dispose, anchorPoint, pointOf, aim, pick,
  };
})();
window.fp3d = fp3d;
