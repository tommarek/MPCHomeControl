#!/usr/bin/env python3
"""Derive the dashboard floor plan (`src/dashboard/floorplan.json`) from the architect's DXF.

Regenerate (from the repo root; needs `pip install --user ezdxf shapely`):

    python3 tools/floorplan_from_dxf.py [--dxf podlazi.dxf] [--out src/dashboard/floorplan.json]

The output is deterministic (same bytes on every run). The script prints an AREA CHECK table
(room polygon area vs the model's `volume / height`, read from `model.json5`) and exits non-zero
when a zone is missing or deviates by more than 15 % — except the zones listed in
`ACCEPTED_DEVIATIONS`, whose (documented) mismatch is printed but does not fail the run. The check
runs before the output is written, so a failing run leaves the committed JSON untouched. A zone whose
faces do not merge into one contiguous room aborts the run.

How the geometry is recovered
-----------------------------
`podlazi.dxf` is in millimetres with two layers sharing one origin: `1NP` (ground floor, incl. the
garage) and `2NP` (first floor). There are no TEXT entities, so rooms cannot be read off labels.
Instead every LINE/LWPOLYLINE segment of a layer is noded together and `shapely.polygonize` turns
the wall faces into closed *faces*. Each zone is then a face (or union of faces) picked by a hard
-coded seed point / region of interest in DXF millimetres (the `GROUND_ZONES` / `FIRST_ZONES` tables below, each with
a comment saying what it is). Room polygons are therefore the INNER (clear floor) faces of the walls.

Where the DXF cannot give a room directly this is spelled out in the tables and in `source`:
  "dxf"        a polygonize face (or union of faces), used as-is
  "dxf+split"  one face cut by an explicit documented line (kitchen / living room, open plan)
  "dxf+inset"  the first-floor closet face cut back to the area of `CLOSET_INSET`
  "hand"       rectangle/void whose corner coordinates were read off DXF vertices by hand
A room whose `area_m2` is overridden by a tape measurement (`MEASURED_AREAS`) also carries `area_source`
and `polygon_area_m2`, the drawn polygon's own area; the AREA CHECK tests that, never the override.

Frame decision
--------------
The DXF-native frame has +y (up) facing azimuth 140 (SE), +x facing 230 (SW), -y facing 320 (NW) and
-x facing 50 (NE). The display frame is the architect's ground-floor drawing orientation, i.e. a
proper 90 degree clockwise rotation of the DXF (not a mirror): screen-up faces azimuth 50 (NE),
screen-right 140 (SE), screen-down 230 (SW), screen-left 320 (NW). Metres, SVG convention
(x right, y DOWN):

    X = (y_dxf - Y0) / 1000        Y = (x_dxf - X0) / 1000

with Y0 = the ground floor's most negative y (the az-320 outer face) and X0 = 0. The SAME transform is
used for both floors. North points 50 degrees counter-clockwise from screen-up. Result: kitchen at
the left, garage at the bottom right, first-floor guest room directly above the kitchen.

Notes on the output
-------------------
* `walls` are hole-free closed rings: the wall region (outline minus rooms and voids) is cut into
  vertical slabs so no ring encloses another. Adjacent slabs share an edge; paint them with a
  hairline stroke in the fill colour (or `shape-rendering: crispEdges`) to hide anti-alias seams.
  Every door and window is cut out of the walls (see `openings`), so a wall ring ends at each jamb.
* `features` lists fixtures that are not rooms, walls or openings (display metres, heights above the
  floor): kind "fireplace" (ground floor: the chimney block `polygon`, its DXF `flue` square and
  circle, the corner `firebox` with its glass `faces` {a, b, outward n} and `sill_m`/`head_m`, the
  stone `plinth` {polygon, top_m, thickness_m}, and `details` on the block's faces: hatch / vent
  {a, b, n, y0, y1}) and kind "chimney" (first floor: the flue square rising through it). See
  `FIREPLACE` for which parts are DXF and which are hand-placed from photos.
* `openings` lists every door and window of the floor, cut through the wall:
    kind       "door" | "window"
    type       door: "swing" (DXF arc) | "sliding" | "sectional" (garage vehicle door) | "glazed"
               (full-height terrace glazing); window: "window" | "clerestory" (a 20 cm strip
               under the ceiling, sill >= CLERESTORY_MIN_SILL_M)
    a, b       the two jamb points on the wall centre line (display metres)
    width_m    |b - a|;  depth_m  the wall thickness;  n  unit normal across the wall (for an
               exterior opening it points outside, and `azimuth_deg` is that outward direction)
    rooms      the zones on the two sides of the wall ("outside" beyond the outline), sorted
    sill_m, head_m   height of the sill and head above the floor (windows and glazed doors; from
               the architect's 1.NP / 2.NP drawings, `OPENING_HEIGHTS`)
    swing      swing doors only: the DXF arc. `hinge` is the arc centre, `r` the leaf, angles are
               SVG degrees from +x, clockwise positive, `a1_deg > a0_deg`; `leaf` is the open leaf's
               free end and `closed` the leaf's free end when shut (on the far jamb)
    source     "dxf" (derived) or "hand" (DXF vertices read off by hand, see `HAND_OPENINGS`)
  Where openings come from: swing doors from the DXF ARCs; garage vehicle doors from the sectional
  leaf line the DXF draws 50 mm inside the garage's inner face (`SECTIONAL_SIDES`); windows from the
  glazing line 60-110 mm inside the outer face (a span lying over a swing door is the door's own
  glazing and is dropped); hand: the hall <-> living-room sliding opening (its two leaf boxes sit
  inside the wall, no arc).
"""

import argparse
import json
import math
import re
import sys
from pathlib import Path

import ezdxf
from shapely.geometry import LineString, MultiPolygon, Point, Polygon, box
from shapely.geometry.polygon import orient
from shapely.ops import polygonize, polylabel, unary_union

REPO = Path(__file__).resolve().parent.parent

# --- model comparison ------------------------------------------------------------------------

GROUND_HEIGHT_M = 2.55
FIRST_HEIGHT_M = 2.30
MAX_DEVIATION = 0.15
NEGLIGIBLE_PART_MM2 = 50_000  # 0.05 m2: a stray sliver of a zone union that tidy() may drop

# Zones whose DXF area disagrees with the model's tape-measured volume by more than 15 %.
# The polygon is still the DXF truth (owner-confirmed room assignment); the model number is the
# thing to re-check. Printed as ACCEPTED instead of failing the run.
ACCEPTED_DEVIATIONS: dict[str, str] = {}

# Owner tape measurements (width x depth, m) that override the drawn polygon's reported area. The
# first-floor bathroom (the top-left corner room, architect's 202 KOUPELNA) measured 3.436 x 2.768 m
# on 2026-10-10, confirming model.json5's 9.501 m2; its drawn (inner-face) rectangle is a little larger.
MEASURED_AREAS = {
    "first_floor_bathroom": (3.436, 2.768),
}

# --- zone tables (all coordinates are DXF-native millimetres) --------------------------------

# Ground floor, layer 1NP. `seeds`: points inside one polygonize face. `rois`: boxes (xmin, ymin,
# xmax, ymax); every face whose representative point lies in a box is merged in (used where
# stair treads cut one room into many small cells).
GROUND_ZONES = [
    # zone, name, source, seeds, rois
    ("entrance", "Entrance", "dxf", [(5445, -1132)], []),  # ENTRY 1973x4415
    ("ground_closet", "Closet", "dxf", [(1707, -2262)], []),  # CLOSET 3669x2554
    ("technical_room", "Utility", "dxf", [(6232, -4412)], []),  # UTILITY 3449x2974
    # HALL incl. the stair run: the big hall/vestibule face plus every small tread/landing cell in
    # the stair block x 432..4432, y -9694..-5704.
    ("ground_hall", "Hall", "dxf", [(3497, -5966)], [(432, -9694, 4432, -5704)]),
    ("toilet", "WC", "dxf", [(1809, -4907)], []),  # WC 1224x2554
    ("ground_bathroom", "Bathroom", "dxf", [(6232, -7412)], []),  # KOUPELNA PRIZEMI
    ("storage", "Storage", "dxf", [(1932, -16632)], []),  # STORAGE 1974x2902
    ("office", "Office", "dxf", [(1932, -19502)], []),  # OFFICE 3830x2902
    ("garrage", "Garage", "dxf", [(11485, -3029)], []),  # GARAGE 6024x6487 (holes = car outlines)
]
# Kitchen + living room are ONE open space with no wall between (one polygonize face). Explicit
# split: the kitchen is the strip at the -y end, cut along the storage room's far wall face
# (y = -15294, the end of the office/storage block); this gives ~24.5 m2 (model 24.3).
OPEN_PLAN_SEED = (5732, -15144)
OPEN_PLAN_SPLIT_Y = -15294

# First floor, layer 2NP.
# Room assignment per the architect's 2NP plan (owner, 2026-10-10): 202 KOUPELNA is the top-left
# corner room, 203 SATNA (closet) the room beside it on the SW side, 204 LOZNICE (bedroom) the next.
FIRST_ZONES = [
    ("first_floor_closet", "Closet", "dxf", [(5800, -2400)], []),
    ("bedroom", "Bedroom", "dxf", [(5800, -6500)], []),
    ("room_1", "Room 1", "dxf", [(5800, -10800)], []),
    ("room_2", "Room 2", "dxf", [(5800, -15200)], []),
    # Guest room = the two faces across the whole -y end (an L: the wide end room plus the strip
    # beside room_2 on the hall side).
    ("guestroom", "Guest room", "dxf", [(5800, -19500), (1500, -18000)], []),
    # Hall WITHOUT the stair void (model: 19.05 m2 "solid floor"): the strip east of the stairs plus
    # the strip below them. The knee-wall zone x 445..930 is not counted.
    ("first_floor_hall", "Hall", "dxf", [(3200, -10000), (1500, -12000)], []),
]
# Hand rectangles (xmin, ymin, xmax, ymax) read off DXF vertices.
#   bathroom: top-left room, the finish-layer box whose inner faces are x=990 / y=-1810 and, on the
#   far sides, the wall faces x=3770 / y=-5525 (the 815 / -1635 solid corner includes the wall lining).
FIRST_HAND_RECTS = [
    ("first_floor_bathroom", "Bathroom", (990, -5525, 3770, -1810)),
]
# The walk-in closet is slightly smaller than its drawn slot (owner, 2026-10-10): it runs the full
# length of the bedroom wall (the slot's -y side, from the bathroom wall to the SW knee wall) but is
# shorter along the bathroom wall (-x side), so the gable end of the slot (+y) is not part of the
# room. Inset that side alone so the room has model.json5's 12.905 m2.
CLOSET_INSET = {"zone": "first_floor_closet", "area_m2": 12.905}
# Stair void on the first floor (no floor): the stair run's bounding rectangle.
FIRST_VOID_RECT = (445, -9690, 2575, -5750)

# Stair boxes (xmin, xmax, ymin, ymax); tread lines are the 0.95-1.1 m LINEs inside.
STAIR_BOX = {"1NP": (432, 2800, -9700, -5690), "2NP": (445, 2600, -9700, -5690)}
TREAD_LEN_MM = (950, 1100)

# Exterior sides for window detection: (name, axis of the wall line, outer coordinate, inward
# sign, other-axis range, outward azimuth). The DXF draws a window as parallel lines 80 and 160
# mm inside the outer face; only the 80 mm one is used.
EXTERIOR_SIDES = {
    "1NP": [
        ("top", "y", -4, -1, (-8, 8172), 140),
        ("left", "x", -8, +1, (-21859, -4), 50),
        ("bottom", "y", -21859, +1, (-8, 8172), 320),
        ("right", "x", 8172, -1, (-21859, -6604), 230),
    ],
    "2NP": [
        ("top", "y", 0, -1, (0, 8185), 140),
        ("left", "x", 0, +1, (-21855, 0), 50),
        ("bottom", "y", -21855, +1, (0, 8185), 320),
        ("right", "x", 8185, -1, (-21855, 0), 230),
    ],
}
WINDOW_OFFSET_MM = (60, 110)
WINDOW_MIN_LEN_MM = 500

# Garage vehicle doors: the DXF draws a sectional door as a box in the wall plus a leaf line 50 mm
# inside the inner face. (axis, outer face, inner face, other-axis range, outward azimuth)
SECTIONAL_SIDES = {
    "1NP": [
        ("y", -4, -304, (8172, 15119), 140),  # garage SE side: main door
        ("y", -6604, -6304, (8172, 15119), 320),  # garage NW side: small door
    ],
}
SECTIONAL_LEAF_OFFSET_MM = 50
SECTIONAL_MIN_LEN_MM = 2000

# Openings with no arc and no leaf line, as DXF vertices: (layer, type, jamb a, jamb b, wall depth
# direction (unit, mm frame) from the a-b face across the wall, depth mm).
HAND_OPENINGS = [
    # hall <-> living room: the hall-side face y=-9694 breaks at x=2682 and x=4182 (1.5 m); two
    # 750 mm sliding leaves are drawn inside the 300 mm wall.
    ("1NP", "sliding", (2682, -9694), (4182, -9694), (0, -1), 300),
]

# The living-room fireplace. The DXF draws only its flue: a CIRCLE (r ~89 mm) inside a closed
# 400 x 400 mm LWPOLYLINE, on 1NP and again on 2NP (the chimney rises through the first floor, in
# the corner of room_1). Everything else is HAND-PLACED from the owner's photos (2026-10-10):
#   * the plastered chimney block stands in the corner the flue square marks (the wall faces along
#     the flue square's -y and +x edges), floor to ceiling, its -x face flush with the hall wall
#     beside the hall <-> living doorway (2.5 cm proud of it), its +y face meeting the living
#     room's east wall: BLOCK_W along x, BLOCK_D along y (display metres);
#   * a corner firebox at the block's free corner, glass on both exposed faces (FRONT along the -x
#     face, SIDE along the +y face), sill (top of the black stone plinth) and head heights;
#   * the plinth sticks out PLINTH_LIP past the faces and the glass ends;
#   * an inspection hatch and a vent grille on the +y face (offsets along it from the corner).
# Sizes were estimated against the doorway, the 2.55 m ceiling and the switch plates.
FLUE_RADIUS_MM = (80, 100)
FIREPLACE = {
    "zone": "livingroom",
    "block_w": 1.275,
    "block_d": 0.90,
    "front_glass": 0.68,
    "side_glass": 0.58,
    "sill_m": 0.45,
    "head_m": 0.88,
    "plinth_lip": 0.04,
    "plinth_thickness_m": 0.06,
    "hatch": (0.52, 0.94, 1.70, 2.02),  # along the +y face from the corner (m), bottom, top (m)
    "vent": (0.93, 1.20, 2.40, 2.49),
}

# Sill and head heights (m above the floor) by (layer, outward azimuth, span mm), read off the
# architect's drawings ("1400(900)" = 1400 high on a 900 sill; "2300" alone = full-height glazing).
# A value of None for the sill marks full-height glazing (a terrace door).
OPENING_HEIGHTS = {
    ("1NP", 320, 1250): (0.9, 2.3),  # office, kitchen
    ("1NP", 140, 1250): (0.9, 2.3),  # closet, entrance
    ("1NP", 230, 1500): (0.9, 2.3),  # kitchen
    ("1NP", 230, 1000): (0.9, 2.3),  # bathroom
    ("1NP", 230, 2000): (None, 2.3),  # kitchen terrace door
    ("1NP", 230, 3500): (None, 2.3),  # living-room lift-and-slide portal
    ("1NP", 50, 3500): (2.35, 2.55),  # living room, 20 cm clerestory under the ceiling (owner)
    ("1NP", 50, 3000): (2.35, 2.55),  # stair hall, 20 cm clerestory (owner)
    ("1NP", 50, 750): (2.35, 2.55),  # WC, 20 cm clerestory (owner; not in the thermal model)
    ("2NP", 230, 1500): (0.94, 1.94),  # "1000(938)"
    ("2NP", 320, 1250): (0.94, 1.94),  # guest-room gable; owner: every first-floor window is 1.0 m tall (architect drew "1500(800)")
}
# A window whose sill is at least this high is a clerestory: a 20 cm glass strip under the ceiling
# (owner, 2026-10-10: the WC, the stair hall and the living room's NE window).
CLERESTORY_MIN_SILL_M = 2.0

# --- DXF reading -----------------------------------------------------------------------------


def layer_segments(msp, layer):
    """All LINE / LWPOLYLINE segments of a layer as ((x0, y0), (x1, y1)) in mm."""
    out = []
    for e in msp:
        if e.dxf.layer != layer:
            continue
        kind = e.dxftype()
        if kind == "LINE":
            out.append(((e.dxf.start.x, e.dxf.start.y), (e.dxf.end.x, e.dxf.end.y)))
        elif kind == "LWPOLYLINE":
            pts = list(e.get_points("xy"))
            if e.closed:
                pts.append(pts[0])
            out.extend(zip(pts, pts[1:]))
    return out


def layer_faces(segments, extend=15.0):
    """Polygonize the wall faces. Axis-aligned segments are extended by `extend` mm at both ends
    so that corners the draughtsman left 5-10 mm open still close."""
    lines = []
    for a, b in segments:
        dx, dy = b[0] - a[0], b[1] - a[1]
        n = math.hypot(dx, dy)
        if n == 0:
            continue
        ux, uy = dx / n, dy / n
        if min(abs(ux), abs(uy)) > 1e-3:
            lines.append(LineString([a, b]))
        else:
            lines.append(
                LineString(
                    [
                        (a[0] - ux * extend, a[1] - uy * extend),
                        (b[0] + ux * extend, b[1] + uy * extend),
                    ]
                )
            )
    return sorted(polygonize(unary_union(lines)), key=lambda f: (f.bounds, f.area))


def face_at(faces, pt):
    hits = [f for f in faces if f.contains(Point(pt))]
    if len(hits) != 1:
        raise SystemExit(f"seed {pt} lies in {len(hits)} faces (expected 1)")
    return hits[0]


def tidy(geom):
    """Fill holes, close hairline gaps between merged faces, drop door-jamb noise."""
    if isinstance(geom, MultiPolygon):
        geom = max(geom.geoms, key=lambda g: g.area)
    geom = Polygon(geom.exterior)
    geom = geom.buffer(30, join_style=2).buffer(-30, join_style=2)
    if isinstance(geom, MultiPolygon):
        geom = max(geom.geoms, key=lambda g: g.area)
    return geom.simplify(12)


def zone_polygon(zone, faces, seeds, rois):
    picked = [face_at(faces, s) for s in seeds]
    for xmin, ymin, xmax, ymax in rois:
        region = box(xmin, ymin, xmax, ymax)
        picked += [f for f in faces if region.contains(f.representative_point())]
    union = unary_union(picked)
    if isinstance(union, MultiPolygon):
        union = union.buffer(30, join_style=2).buffer(-30, join_style=2)
    if isinstance(union, MultiPolygon):
        areas = sorted((g.area for g in union.geoms), reverse=True)
        if areas[1] >= NEGLIGIBLE_PART_MM2:
            parts = ", ".join(f"{a / 1e6:.2f}" for a in areas)
            raise SystemExit(f"zone {zone}: faces do not form one contiguous room, part areas (m2): {parts}")
    return tidy(union)


# --- output frame ----------------------------------------------------------------------------


def clean_ring(pts):
    """Round to 0.01 m, drop repeated and collinear vertices, return [[x, y], ...] (not repeated)."""
    out = []
    for x, y in pts:
        p = (round(x, 2) + 0.0, round(y, 2) + 0.0)
        if not out or out[-1] != p:
            out.append(p)
    if len(out) > 1 and out[0] == out[-1]:
        out.pop()
    changed = True
    while changed and len(out) > 3:
        changed = False
        for i in range(len(out)):
            a, b, c = out[i - 1], out[i], out[(i + 1) % len(out)]
            if abs((b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0])) < 1e-9:
                del out[i]
                changed = True
                break
    return [list(p) for p in out]


def r2(v):
    return round(v, 2) + 0.0


class Frame:
    """DXF-native mm -> display metres (90 degrees clockwise, y down)."""

    def __init__(self, ry0mm, x0_mm=0.0):
        self.y0, self.x0 = ry0mm, x0_mm

    def pt(self, x, y):
        return ((y - self.y0) / 1000.0, (x - self.x0) / 1000.0)

    def ring(self, coords):
        return clean_ring([self.pt(x, y) for x, y in coords])

    def polygon(self, poly):
        """Display-frame polygon (shapely) for area / label computations."""
        return orient(Polygon(self.ring(poly.exterior.coords)), 1.0)


def ring_area(ring):
    return abs(Polygon(ring).area)


def room_entry(frame, zone, name, poly, source):
    ring = frame.ring(poly.exterior.coords)
    disp = Polygon(ring)
    label = polylabel(disp, 0.05)
    entry = {
        "zone": zone,
        "name": name,
        "polygon": ring,
        "label": [round(label.x, 2), round(label.y, 2)],
        "area_m2": round(disp.area, 2),
        "source": source,
    }
    if zone in MEASURED_AREAS:
        w, d = MEASURED_AREAS[zone]
        entry["polygon_area_m2"] = entry["area_m2"]
        entry["area_m2"] = round(w * d, 2)
        entry["area_source"] = f"tape {w:.3f} x {d:.3f} m"
    return entry


# --- details ---------------------------------------------------------------------------------


def door_arcs(msp, layer, frame):
    doors = []
    for e in msp:
        if e.dxf.layer != layer or e.dxftype() != "ARC":
            continue
        hx, hy = frame.pt(e.dxf.center.x, e.dxf.center.y)
        # native angle th (CCW, y up): direction (cos th, sin th) -> display (dX, dY) = (sin th, cos th)
        # -> SVG angle phi (clockwise from +x) = 90 - th; the CCW native sweep start..end becomes
        # the increasing SVG range [90 - end, 90 - start].
        start, end = e.dxf.start_angle, e.dxf.end_angle
        if end < start:
            end += 360.0
        a0 = (90.0 - end) % 360.0
        doors.append(
            {
                "hinge": [round(hx, 2), round(hy, 2)],
                "r": round(e.dxf.radius / 1000.0, 2),
                "a0_deg": round(a0, 1),
                "a1_deg": round(a0 + (end - start), 1),
            }
        )
    doors.sort(key=lambda d: (d["hinge"], d["a0_deg"]))
    return doors


def stair_treads(segments, layer, frame):
    xmin, xmax, ymin, ymax = STAIR_BOX[layer]
    out = set()
    for (x0, y0), (x1, y1) in segments:
        if not (xmin - 5 <= min(x0, x1) and max(x0, x1) <= xmax + 5):
            continue
        if not (ymin - 5 <= min(y0, y1) and max(y0, y1) <= ymax + 5):
            continue
        n = math.hypot(x1 - x0, y1 - y0)
        if TREAD_LEN_MM[0] <= n <= TREAD_LEN_MM[1]:
            a, b = frame.pt(x0, y0), frame.pt(x1, y1)
            a = (round(a[0], 2) + 0.0, round(a[1], 2) + 0.0)
            b = (round(b[0], 2) + 0.0, round(b[1], 2) + 0.0)
            out.add(tuple(sorted([a, b])))
    return [[list(a), list(b)] for a, b in sorted(out)]


def flue(msp, layer, frame):
    """The chimney flue on a layer: (display square ring, centre, radius m), or None."""
    squares = []
    for e in msp.query(f'LWPOLYLINE[layer=="{layer}"]'):
        pts = [(p[0], p[1]) for p in e.get_points()]
        if len(pts) == 4:
            xs, ys = [p[0] for p in pts], [p[1] for p in pts]
            if 350 <= max(xs) - min(xs) <= 450 and 350 <= max(ys) - min(ys) <= 450:
                squares.append(box(min(xs), min(ys), max(xs), max(ys)))
    for c in msp.query(f'CIRCLE[layer=="{layer}"]'):
        if not FLUE_RADIUS_MM[0] <= c.dxf.radius <= FLUE_RADIUS_MM[1]:
            continue
        centre = Point(c.dxf.center.x, c.dxf.center.y)
        for sq in squares:
            if sq.contains(centre):
                cx, cy = frame.pt(centre.x, centre.y)
                return frame.ring(sq.exterior.coords), [r2(cx), r2(cy)], round(c.dxf.radius / 1000.0, 3)
    return None


def fireplace_features(msp, frame):
    """`features` per floor id: the ground-floor fireplace (block, firebox, plinth, hatch, vent) and
    the chimney continuing through the first floor. See FIREPLACE for what is DXF and what is hand."""
    out = {"ground": [], "first": []}
    g, f1 = flue(msp, "1NP", frame), flue(msp, "2NP", frame)
    for layer, found in (("1NP", g), ("2NP", f1)):
        if found is None:
            raise SystemExit(f"fireplace: no flue circle + square found on {layer}")
    if g:
        F = FIREPLACE
        sq, centre, r = g
        x1, y0 = max(p[0] for p in sq), min(p[1] for p in sq)  # the corner's two wall faces
        x0, y1 = r2(x1 - F["block_w"]), r2(y0 + F["block_d"])
        lip = F["plinth_lip"]
        on_side = lambda u0, u1, z0, z1: {"a": [r2(x0 + u0), y1], "b": [r2(x0 + u1), y1], "n": [0.0, 1.0], "y0": z0, "y1": z1}
        out["ground"].append({
            "kind": "fireplace",
            "zone": F["zone"],
            "polygon": [[x0, y0], [x1, y0], [x1, y1], [x0, y1]],
            "flue": {"polygon": sq, "center": centre, "r": r},
            "firebox": {
                "corner": [x0, y1],
                "faces": [
                    {"a": [x0, r2(y1 - F["front_glass"])], "b": [x0, y1], "n": [-1.0, 0.0]},
                    {"a": [x0, y1], "b": [r2(x0 + F["side_glass"]), y1], "n": [0.0, 1.0]},
                ],
                "sill_m": F["sill_m"],
                "head_m": F["head_m"],
            },
            "plinth": {
                "polygon": [[r2(x0 - lip), r2(y1 - F["front_glass"] - lip)], [r2(x0 + F["side_glass"] + lip), r2(y1 - F["front_glass"] - lip)],
                            [r2(x0 + F["side_glass"] + lip), r2(y1 + lip)], [r2(x0 - lip), r2(y1 + lip)]],
                "top_m": F["sill_m"],
                "thickness_m": F["plinth_thickness_m"],
            },
            "details": [
                {"kind": "hatch", **on_side(*F["hatch"])},
                {"kind": "vent", **on_side(*F["vent"])},
            ],
            "source": "flue: dxf (1NP circle + square); block, firebox, plinth, hatch, vent: hand, from photos",
        })
    if f1:
        sq, centre, r = f1
        out["first"].append({"kind": "chimney", "polygon": sq, "flue": {"center": centre, "r": r}, "source": "dxf (2NP circle + square)"})
    return out


def window_spans(segments, layer):
    """Glazing spans in DXF mm: (axis, outer, inward, start, end, azimuth)."""
    out = set()
    for _name, axis, outer, inward, (lo, hi), azimuth in EXTERIOR_SIDES[layer]:
        for (x0, y0), (x1, y1) in segments:
            if axis == "y":
                if abs(y0 - y1) > 1:
                    continue
                off = (y0 - outer) * inward
                s, e = sorted((x0, x1))
            else:
                if abs(x0 - x1) > 1:
                    continue
                off = (x0 - outer) * inward
                s, e = sorted((y0, y1))
            if WINDOW_OFFSET_MM[0] <= off <= WINDOW_OFFSET_MM[1] and e - s >= WINDOW_MIN_LEN_MM:
                if s >= lo - 5 and e <= hi + 5:
                    out.add((axis, outer, inward, round(s), round(e), azimuth))
    return sorted(out)


def sectional_spans(segments, layer):
    """Garage vehicle doors in DXF mm: (axis, outer, inner, start, end, azimuth)."""
    out = set()
    for axis, outer, inner, (lo, hi), azimuth in SECTIONAL_SIDES.get(layer, []):
        inward = 1 if inner > outer else -1
        leaf = inner + inward * SECTIONAL_LEAF_OFFSET_MM
        for (x0, y0), (x1, y1) in segments:
            if axis == "y" and abs(y0 - y1) <= 1 and abs(y0 - leaf) <= 5:
                s, e = sorted((x0, x1))
            elif axis == "x" and abs(x0 - x1) <= 1 and abs(x0 - leaf) <= 5:
                s, e = sorted((y0, y1))
            else:
                continue
            if e - s >= SECTIONAL_MIN_LEN_MM and s >= lo - 5 and e <= hi + 5:
                out.add((axis, outer, inner, round(s), round(e), azimuth))
    return sorted(out)


def wall_rings(outline_disp, solids_disp, cut_xs, holes=()):
    """Wall region = outline - rooms - voids - openings, cut at the given display-x values so that
    no resulting ring has a hole."""
    rooms = unary_union(list(solids_disp) + list(holes))
    region = outline_disp.difference(rooms)
    xmin, ymin, xmax, ymax = outline_disp.bounds
    cuts = sorted(set([xmin - 1] + [round(x, 2) for x in cut_xs] + [xmax + 1]))
    rings = []
    for lo, hi in zip(cuts, cuts[1:]):
        piece = region.intersection(box(lo, ymin - 1, hi, ymax + 1))
        geoms = piece.geoms if hasattr(piece, "geoms") else [piece]
        for g in geoms:
            if not isinstance(g, Polygon) or g.area < 0.02:
                continue
            assert not g.interiors, "wall piece with a hole - add a cut"
            ring = clean_ring(g.exterior.coords[:-1])
            if len(ring) >= 3:
                rings.append(ring)
    rings.sort(key=lambda r: (min(p[0] for p in r), min(p[1] for p in r), len(r)))
    return rings


# --- openings (display frame, metres) -------------------------------------------------------


def _r2(v):
    return round(v, 2) + 0.0


def _pt(p):
    return [_r2(p[0]), _r2(p[1])]


def azimuth_of(n):
    """Outward display unit normal -> compass azimuth (screen-up faces 50)."""
    return round((50 + math.degrees(math.atan2(n[0], -n[1]))) % 360)


def march(region, p, d, step=0.005, limit=0.9):
    """Walk from p along d: the distances (t_in, t_out) where the walk enters and leaves `region`."""
    t, t_in = 0.0, None
    while t <= limit:
        inside = region.contains(Point(p[0] + d[0] * t, p[1] + d[1] * t))
        if t_in is None and inside:
            t_in = t
        elif t_in is not None and not inside:
            return t_in, t
        t += step
    raise SystemExit(f"no wall crossed from {p} along {d}")


def side_zone(pt, entries, voids, outline):
    p = Point(pt)
    for e in entries:
        if Polygon(e["polygon"]).contains(p):
            return e["zone"]
    for kind, ring in voids:
        if Polygon(ring).contains(p):
            return kind
    return "outside" if not outline.contains(p) else "wall"


def opening(kind, typ, centre, d, n, width, depth, source):
    """An opening centred on `centre` (wall centre line), running along unit d, across unit n."""
    a = (centre[0] - d[0] * width / 2, centre[1] - d[1] * width / 2)
    b = (centre[0] + d[0] * width / 2, centre[1] + d[1] * width / 2)
    if (round(a[0], 2), round(a[1], 2)) > (round(b[0], 2), round(b[1], 2)):
        a, b = b, a
    return {"kind": kind, "type": typ, "a": a, "b": b, "n": n, "width_m": width, "depth_m": depth, "source": source}


def floor_openings(layer, frame, msp, seg, region, entries, voids, outline):
    """Every door and window of a floor (see the module docstring), unrounded until `finish`."""
    out = []
    # swing doors: the DXF arcs
    for dr in door_arcs(msp, layer, frame):
        h, r = dr["hinge"], dr["r"]
        e0 = (math.cos(math.radians(dr["a0_deg"])), math.sin(math.radians(dr["a0_deg"])))
        e1 = (math.cos(math.radians(dr["a1_deg"])), math.sin(math.radians(dr["a1_deg"])))
        cands = []
        for d, w in ((e0, e1), (e1, e0)):
            probe = (h[0] + d[0] * r / 2 - w[0] * 0.04, h[1] + d[1] * r / 2 - w[1] * 0.04)
            if region.contains(Point(probe)):
                cands.append((d, w))
        if len(cands) != 1:
            raise SystemExit(f"door at {h}: cannot tell the wall side ({len(cands)} candidates)")
        d, w = cands[0]
        start = (h[0] + d[0] * r / 2 + w[0] * 0.15, h[1] + d[1] * r / 2 + w[1] * 0.15)
        t_in, t_out = march(region, start, (-w[0], -w[1]))
        mid = (t_in + t_out) / 2
        centre = (start[0] - w[0] * mid, start[1] - w[1] * mid)
        o = opening("door", "swing", centre, d, (-w[0], -w[1]), r, t_out - t_in, "dxf")
        o["swing"] = {
            "hinge": h,
            "r": r,
            "a0_deg": dr["a0_deg"],
            "a1_deg": dr["a1_deg"],
            "leaf": _pt((h[0] + w[0] * r, h[1] + w[1] * r)),
            "closed": _pt((h[0] + d[0] * r, h[1] + d[1] * r)),
        }
        out.append(o)
    doors = [o for o in out]

    def along_face(axis, outer, s, e):
        if axis == "y":
            return frame.pt(s, outer), frame.pt(e, outer)
        return frame.pt(outer, s), frame.pt(outer, e)

    def disp_vec(axis, sign):
        # a DXF mm direction along the wall normal -> display (dx, dy) = (dy_dxf, dx_dxf)
        return (float(sign), 0.0) if axis == "y" else (0.0, float(sign))

    # garage vehicle doors: the sectional leaf line
    for axis, outer, inner, s0, e0_, _az in sectional_spans(seg, layer):
        a, b = along_face(axis, outer, s0, e0_)
        inward = disp_vec(axis, 1 if inner > outer else -1)
        depth = abs(inner - outer) / 1000.0
        centre = ((a[0] + b[0]) / 2 + inward[0] * depth / 2, (a[1] + b[1]) / 2 + inward[1] * depth / 2)
        d = ((b[0] - a[0]) / math.dist(a, b), (b[1] - a[1]) / math.dist(a, b))
        out.append(opening("door", "sectional", centre, d, (-inward[0], -inward[1]), math.dist(a, b), depth, "dxf"))
    # hand-placed openings
    for lay, typ, ja, jb, dirn, depth_mm in HAND_OPENINGS:
        if lay != layer:
            continue
        a, b = frame.pt(*ja), frame.pt(*jb)
        nn = (float(dirn[1]), float(dirn[0]))
        depth = depth_mm / 1000.0
        centre = ((a[0] + b[0]) / 2 + nn[0] * depth / 2, (a[1] + b[1]) / 2 + nn[1] * depth / 2)
        d = ((b[0] - a[0]) / math.dist(a, b), (b[1] - a[1]) / math.dist(a, b))
        out.append(opening("door", typ, centre, d, nn, math.dist(a, b), depth, "hand"))
    # windows (and full-height glazing): the glazing line inside the outer face
    for axis, outer, inward_mm, s0, e0_, az in window_spans(seg, layer):
        a, b = along_face(axis, outer, s0, e0_)
        inward = disp_vec(axis, inward_mm)
        d = ((b[0] - a[0]) / math.dist(a, b), (b[1] - a[1]) / math.dist(a, b))
        span = LineString([a, b])
        if any(span.distance(LineString([o["a"], o["b"]])) < 0.4 and abs(d[0] * o["n"][0] + d[1] * o["n"][1]) < 1e-6
               and span.buffer(0.4).intersection(LineString([o["a"], o["b"]])).length > 0.3 for o in doors):
            continue  # the glazed panel of a swing door
        start = ((a[0] + b[0]) / 2 - inward[0] * 0.05, (a[1] + b[1]) / 2 - inward[1] * 0.05)
        t_in, t_out = march(region, start, inward)
        mid = (t_in + t_out) / 2
        centre = (start[0] + inward[0] * mid, start[1] + inward[1] * mid)
        key = (layer, az, int(round((e0_ - s0) / 10.0)) * 10)
        if key not in OPENING_HEIGHTS:
            raise SystemExit(f"no sill/head height for window {key}: add it to OPENING_HEIGHTS")
        sill, head = OPENING_HEIGHTS[key]
        typ = "glazed" if sill is None else "clerestory" if sill >= CLERESTORY_MIN_SILL_M else "window"
        o = opening("door" if sill is None else "window", typ,
                    centre, d, (-inward[0], -inward[1]), math.dist(a, b), t_out - t_in, "dxf")
        o["sill_m"], o["head_m"] = (0.0 if sill is None else sill), head
        out.append(o)

    # sides, exterior direction, rounding
    final = []
    for o in out:
        c = ((o["a"][0] + o["b"][0]) / 2, (o["a"][1] + o["b"][1]) / 2)
        n, reach = o["n"], o["depth_m"] / 2 + 0.2
        plus = side_zone((c[0] + n[0] * reach, c[1] + n[1] * reach), entries, voids, outline)
        minus = side_zone((c[0] - n[0] * reach, c[1] - n[1] * reach), entries, voids, outline)
        if minus == "outside":
            n, plus, minus = (-n[0], -n[1]), minus, plus
        rec = {
            "kind": o["kind"],
            "type": o["type"],
            "floor": None,
            "a": _pt(o["a"]),
            "b": _pt(o["b"]),
            "width_m": _r2(o["width_m"]),
            "depth_m": _r2(o["depth_m"]),
            "n": [_r2(n[0]), _r2(n[1])],
            "rooms": sorted([plus, minus]),
        }
        if plus == "outside":
            rec["azimuth_deg"] = azimuth_of(n)
        if "sill_m" in o:
            rec["sill_m"], rec["head_m"] = o["sill_m"], o["head_m"]
        if "swing" in o:
            rec["swing"] = o["swing"]
        rec["source"] = o["source"]
        final.append(rec)
    final.sort(key=lambda r: (r["kind"], r["a"], r["b"]))
    return final, [opening_rect(o) for o in out]


def opening_rect(o, extra=0.03):
    """The cut through the wall: the opening's footprint, a little past both faces."""
    a, b, n, h = o["a"], o["b"], o["n"], o["depth_m"] / 2 + extra
    return Polygon([
        (a[0] - n[0] * h, a[1] - n[1] * h),
        (b[0] - n[0] * h, b[1] - n[1] * h),
        (b[0] + n[0] * h, b[1] + n[1] * h),
        (a[0] + n[0] * h, a[1] + n[1] * h),
    ])


def inset_closet(room):
    """Cut the closet's slot back from the gable (+y) to `CLOSET_INSET`'s area, keeping its full
    width along the bedroom wall (DXF mm)."""
    zone, name, _source, poly = room
    x0, y0, x1, y1 = poly.bounds
    d = (y1 - y0) - CLOSET_INSET["area_m2"] * 1e6 / (x1 - x0)
    if not 0 <= d < (y1 - y0) / 2:
        raise SystemExit(f"closet inset: {d:.0f} mm does not fit the {x1 - x0:.0f} x {y1 - y0:.0f} mm slot")
    return (zone, name, "dxf+inset", box(x0, y0, x1, y1 - round(d)))


# --- model areas -----------------------------------------------------------------------------


def model_volumes(path):
    text = path.read_text()
    vols = dict(re.findall(r'"(\w+)":\s*\{\s*"volume":\s*([0-9.]+)', text))
    return {k: float(v) for k, v in vols.items()}


# --- main ------------------------------------------------------------------------------------


def build(dxf_path, model_path):
    doc = ezdxf.readfile(dxf_path)
    msp = doc.modelspace()
    seg = {l: layer_segments(msp, l) for l in ("1NP", "2NP")}
    faces = {l: layer_faces(seg[l]) for l in seg}

    # ground-floor outline = everything enclosed by closed wall faces
    ground_outline = tidy(unary_union([f for f in faces["1NP"] if f.area > 1e3]))
    y0 = ground_outline.bounds[1]
    frame = Frame(y0)

    # first-floor outline = the layer's largest polyline's bounding box (outer wall faces)
    def bbox_area(e):
        p = list(e.get_points("xy"))
        xs, ys = [q[0] for q in p], [q[1] for q in p]
        return (max(xs) - min(xs)) * (max(ys) - min(ys))

    big = max((e for e in msp if e.dxf.layer == "2NP" and e.dxftype() == "LWPOLYLINE"), key=bbox_area)
    xs, ys = zip(*big.get_points("xy"))
    first_outline = box(min(xs), min(ys), max(xs), max(ys))

    # --- ground rooms
    ground_rooms = []
    for zone, name, source, seeds, rois in GROUND_ZONES:
        ground_rooms.append((zone, name, source, zone_polygon(zone, faces["1NP"], seeds, rois)))
    open_plan = tidy(face_at(faces["1NP"], OPEN_PLAN_SEED))
    xmin, ymin, xmax, ymax = open_plan.bounds
    kitchen = tidy(open_plan.intersection(box(xmin - 1, ymin - 1, xmax + 1, OPEN_PLAN_SPLIT_Y)))
    living = tidy(open_plan.intersection(box(xmin - 1, OPEN_PLAN_SPLIT_Y, xmax + 1, ymax + 1)))
    ground_rooms += [
        ("livingroom", "Living room", "dxf+split", living),
        ("kitchen", "Kitchen", "dxf+split", kitchen),
    ]

    # --- first-floor rooms
    first_rooms = []
    for zone, name, source, seeds, rois in FIRST_ZONES:
        first_rooms.append((zone, name, source, zone_polygon(zone, faces["2NP"], seeds, rois)))
    for zone, name, (x0, ry0, x1, y1) in FIRST_HAND_RECTS:
        first_rooms.append((zone, name, "hand", box(x0, ry0, x1, y1)))
    first_rooms = [inset_closet(r) if r[0] == CLOSET_INSET["zone"] else r for r in first_rooms]
    void = box(*FIRST_VOID_RECT)

    # --- roof: attic over the first floor, garage attic over the garage
    garage_outline = tidy(ground_outline.intersection(box(first_outline.bounds[2], -6604 - 5, 15119, 5)))
    roof_outline = tidy(unary_union([first_outline, garage_outline]))

    floors = []

    features = fireplace_features(msp, frame)

    def floor_entry(fid, fname, level, height, outline, rooms, layer, voids):
        disp_outline = frame.polygon(outline)
        entries = [room_entry(frame, z, n, p, s) for z, n, s, p in rooms]
        solids = [Polygon(e["polygon"]) for e in entries] + [Polygon(frame.ring(v.exterior.coords)) for _k, v in voids]
        cuts = [Polygon(e["polygon"]).centroid.x for e in entries]
        cuts += [Polygon(frame.ring(v.exterior.coords)).centroid.x for _k, v in voids]
        void_rings = [(k, frame.ring(v.exterior.coords)) for k, v in voids]
        region = disp_outline.difference(unary_union(solids))
        openings, holes = floor_openings(layer, frame, msp, seg[layer], region, entries, void_rings, disp_outline)
        for o in openings:
            o["floor"] = fid
        return {
            "id": fid,
            "name": fname,
            "level": level,
            "height_m": height,
            "outline": frame.ring(outline.exterior.coords),
            "walls": wall_rings(disp_outline, solids, cuts, holes),
            "details": {"stairs": stair_treads(seg[layer], layer, frame)},
            "openings": openings,
            "rooms": entries,
            "voids": [
                {"kind": k, "polygon": frame.ring(v.exterior.coords)} for k, v in voids
            ],
            "features": features.get(fid, []),
        }

    floors.append(floor_entry("ground", "Ground floor", 0, GROUND_HEIGHT_M, ground_outline, ground_rooms, "1NP", []))
    floors.append(floor_entry("first", "First floor", 1, FIRST_HEIGHT_M, first_outline, first_rooms, "2NP", [("stairwell", void)]))
    roof_rooms = [
        room_entry(frame, "attic", "Attic", first_outline, "dxf"),
        room_entry(frame, "garrage_attic", "Garage attic", garage_outline, "dxf"),
    ]
    floors.append(
        {
            "id": "roof",
            "name": "Roof",
            "level": 2,
            "height_m": 0,
            "outline": frame.ring(roof_outline.exterior.coords),
            "walls": [],
            "details": {"stairs": []},
            "openings": [],
            "rooms": roof_rooms,
            "voids": [],
            "features": [],
        }
    )

    allpts = [p for f in floors for p in f["outline"]]
    bounds = {
        "min": [min(p[0] for p in allpts), min(p[1] for p in allpts)],
        "max": [max(p[0] for p in allpts), max(p[1] for p in allpts)],
    }
    data = {
        "version": 1,
        "generated_by": "tools/floorplan_from_dxf.py",
        "source": "podlazi.dxf layers 1NP/2NP",
        "units": "m",
        "frame": {"up_azimuth_deg": 50, "right_azimuth_deg": 140, "north_from_up_deg": -50, "y_down": True},
        "bounds": bounds,
        "floors": floors,
    }
    return data, model_volumes(model_path)


def area_check(data, volumes):
    heights = {f["id"]: f["height_m"] for f in data["floors"]}
    print("AREA CHECK  (polygon area vs model volume / height)")
    print(f"{'zone':<22}{'floor':<8}{'poly m2':>9}{'model m2':>10}{'dev %':>8}  note")
    failures = []
    seen = set()
    for f in data["floors"]:
        for r in f["rooms"]:
            seen.add(r["zone"])
            if f["id"] == "roof":
                print(f"{r['zone']:<22}{f['id']:<8}{r['area_m2']:>9.2f}{'-':>10}{'-':>8}  skipped")
                continue
            vol = volumes.get(r["zone"])
            if vol is None:
                failures.append(f"{r['zone']}: no model volume")
                continue
            model = vol / (heights[f["id"]] if r["zone"] != "garrage" else GROUND_HEIGHT_M)
            poly_area = r.get("polygon_area_m2", r["area_m2"])
            dev = (poly_area - model) / model
            note = ""
            if abs(dev) > MAX_DEVIATION:
                if r["zone"] in ACCEPTED_DEVIATIONS:
                    note = "ACCEPTED: " + ACCEPTED_DEVIATIONS[r["zone"]]
                else:
                    note = "FAIL > 15 %"
                    failures.append(f"{r['zone']}: {dev:+.1%}")
            if "area_source" in r:
                note = (note + " " if note else "") + f"reported {r['area_m2']:.2f} ({r['area_source']})"
            print(f"{r['zone']:<22}{f['id']:<8}{poly_area:>9.2f}{model:>10.2f}{dev * 100:>+8.1f}  {note}")
    expected = {z for z, *_ in GROUND_ZONES} | {z for z, *_ in FIRST_ZONES} | {z for z, *_ in FIRST_HAND_RECTS}
    expected |= {"livingroom", "kitchen", "attic", "garrage_attic"}
    missing = expected - seen
    if missing:
        failures.append("missing zones: " + ", ".join(sorted(missing)))
    return failures


def dump(data):
    """Compact JSON: one room / detail list per line group, no per-coordinate newlines."""
    def j(v):
        return json.dumps(v, separators=(",", ":"))

    lines = ["{"]
    for k in ("version", "generated_by", "source", "units", "frame", "bounds"):
        lines.append(f'"{k}":{j(data[k])},')
    lines.append('"floors":[')
    floor_chunks = []
    for f in data["floors"]:
        parts = [f'"{k}":{j(f[k])}' for k in ("id", "name", "level", "height_m", "outline")]
        parts.append('"walls":[\n' + ",\n".join(j(w) for w in f["walls"]) + "\n]")
        parts.append(f'"details":{j(f["details"])}')
        parts.append('"openings":[' + ("\n" + ",\n".join(j(o) for o in f["openings"]) + "\n" if f["openings"] else "") + "]")
        parts.append('"rooms":[\n' + ",\n".join(j(r) for r in f["rooms"]) + "\n]")
        parts.append(f'"voids":{j(f["voids"])}')
        parts.append('"features":[' + ("\n" + ",\n".join(j(x) for x in f["features"]) + "\n" if f["features"] else "") + "]")
        floor_chunks.append("{\n" + ",\n".join(parts) + "\n}")
    lines.append(",\n".join(floor_chunks))
    lines.append("]")
    lines.append("}")
    return "\n".join(lines) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--dxf", default=str(REPO / "podlazi.dxf"))
    ap.add_argument("--out", default=str(REPO / "src/dashboard/floorplan.json"))
    ap.add_argument("--model", default=str(REPO / "model.json5"))
    args = ap.parse_args()

    data, volumes = build(args.dxf, Path(args.model))
    text = dump(data)
    json.loads(text)  # must parse
    failures = area_check(data, volumes)
    if failures:
        print(f"FAILED (nothing written to {args.out}): " + "; ".join(failures), file=sys.stderr)
        sys.exit(1)
    Path(args.out).write_text(text)
    print(f"wrote {args.out} ({len(text.encode()) / 1024:.1f} KiB)")


if __name__ == "__main__":
    main()
