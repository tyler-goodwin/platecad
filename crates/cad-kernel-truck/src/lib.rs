use std::collections::{HashMap, HashSet};

use cad_ir::{DEFAULT_COLOR, MeshQuality, ModelSpec, Plate, Rgb, ScenePart, TessellatedScene};
use cad_kernel::{Kernel, KernelError, pack, validate_polygon, validate_primitive};
use truck_meshalgo::tessellation::{MeshableShape, MeshedShape};
use truck_modeling::{
    BSplineCurve, BoundedCurve, Curve, Edge, Face, InnerSpace, KnotVec, Line, ParametricCurve, Point3,
    Rad, Shell, Solid, Vector3, Wire, builder,
};

/// Tessellation tolerance for measuring bounds, whatever the mesh quality, so
/// `arrange` lays parts out the same way at every quality.
const TESSELLATION_TOL: f64 = MeshQuality::Fine.chord_tolerance();
/// Geometric tolerance for truck-shapeops, mm.
const BOOLEAN_TOL: f64 = 0.05;
/// Segments per curved edge when drawing edges and measuring bounds.
const CURVE_SEGMENTS: usize = 64;

/// Points along an edge, start to end: its endpoints for a line, samples
/// along the curve otherwise.
fn edge_points(edge: &Edge) -> Vec<Point3> {
    let curve = edge.oriented_curve();
    if matches!(curve, Curve::Line(_)) {
        return vec![edge.front().point(), edge.back().point()];
    }
    let (t0, t1) = curve.range_tuple();
    (0..=CURVE_SEGMENTS)
        .map(|i| curve.subs(t0 + (t1 - t0) * i as f64 / CURVE_SEGMENTS as f64))
        .collect()
}

/// Points that bound a shell: its sampled edges (exact where a face's
/// extremes lie on its edges, e.g. extrusions) plus its tessellation's
/// vertices (within the tessellation tolerance for surfaces that bulge
/// between their edges, e.g. a torus's outer equator).
fn shell_points(shell: &Shell) -> impl Iterator<Item = Point3> + '_ {
    let mesh = shell.triangulation(TESSELLATION_TOL).to_polygon();
    let surface: Vec<Point3> = mesh.positions().to_vec();
    shell.edge_iter().flat_map(|edge| edge_points(&edge)).chain(surface)
}

fn bounds_of(points: impl Iterator<Item = Point3>) -> Option<(Point3, Point3)> {
    let mut points = points;
    let first = points.next()?;
    Some(points.fold((first, first), |(min, max), p| {
        (
            Point3::new(min.x.min(p.x), min.y.min(p.y), min.z.min(p.z)),
            Point3::new(max.x.max(p.x), max.y.max(p.y), max.z.max(p.z)),
        )
    }))
}

#[derive(Debug, Default)]
pub struct TruckKernel;

impl Kernel for TruckKernel {
    fn name(&self) -> &'static str {
        "truck"
    }

    fn build_with(&mut self, spec: &ModelSpec, quality: MeshQuality) -> Result<TessellatedScene, KernelError> {
        let mut scene = TessellatedScene::default();
        let mut plates = Vec::new();
        for body in build_bodies(spec, &Style::default(), &mut plates)? {
            tessellate_into(&mut scene, &body, quality.chord_tolerance());
        }
        scene.plates = plates;
        Ok(scene)
    }
}

/// Presentation inherited down the spec tree. The innermost `Color`/`Part` wins.
#[derive(Debug, Clone, PartialEq)]
struct Style {
    color: Rgb,
    part: Option<String>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            color: DEFAULT_COLOR,
            part: None,
        }
    }
}

/// One B-rep boundary with its style: a single-face shell for 2D specs, the
/// closed skin for solids.
struct Body {
    shell: Shell,
    style: Style,
}

impl Body {
    fn map(self, f: impl FnOnce(&Shell) -> Shell) -> Self {
        Self {
            shell: f(&self.shell),
            style: self.style,
        }
    }
}

/// `plates` collects build plates from `Arrange` nodes with a bed. They're in
/// the arrange's own frame: an arrange is expected at (or near) the root of a
/// print view, not inside a transform.
fn build_bodies(
    spec: &ModelSpec,
    style: &Style,
    plates: &mut Vec<Plate>,
) -> Result<Vec<Body>, KernelError> {
    match spec {
        ModelSpec::Rect { .. }
        | ModelSpec::Polygon { .. }
        | ModelSpec::Circle { .. }
        | ModelSpec::Cutout { .. } => Ok(vec![Body {
            shell: Shell::from(vec![build_face(spec)?]),
            style: style.clone(),
        }]),
        ModelSpec::Extrude {
            profile,
            distance,
            direction,
        } => {
            if !(*distance > 0.0) {
                return Err(KernelError::Invalid(
                    "extrude distance must be positive".into(),
                ));
            }
            let direction = match direction {
                None => Vector3::new(0.0, 0.0, 1.0),
                Some(d) => {
                    let v = to_vector(*d);
                    let length = v.magnitude();
                    if !(length > 1e-12 && length.is_finite()) {
                        return Err(KernelError::Invalid("extrude direction must be a non-zero vector".into()));
                    }
                    v / length
                }
            };
            let face = facing(build_face(profile)?, direction)?;
            let solid = builder::tsweep(&face, direction * *distance);
            let shell = solid
                .into_boundaries()
                .into_iter()
                .next()
                .ok_or_else(|| KernelError::Message("extrude produced an empty solid".into()))?;
            Ok(vec![Body {
                shell,
                style: profile_style(profile, style.clone()),
            }])
        }
        ModelSpec::Revolve { profile, degrees } => {
            if !(*degrees > 0.0 && *degrees <= 360.0) {
                return Err(KernelError::Invalid("revolve degrees must be in (0, 360]".into()));
            }
            let face = build_face(profile)?;
            let (min, max) = bounds_of(face.boundaries().iter().flat_map(wire_points)).unwrap();
            if min.z.abs() > 1e-9 || max.z.abs() > 1e-9 {
                return Err(KernelError::Invalid("revolve profile must lie in the XY plane".into()));
            }
            if min.x < -1e-9 {
                return Err(KernelError::Invalid(
                    "revolve profile must stay on the x >= 0 side of the axis".into(),
                ));
            }
            // Stand the profile up (its Y becomes Z), then spin it around Z.
            let origin = Point3::new(0.0, 0.0, 0.0);
            let upright = builder::rotated(&face, origin, Vector3::new(1.0, 0.0, 0.0), Rad(std::f64::consts::FRAC_PI_2));
            // Standing up turns the +Z normal into -Y, but on the +X side a
            // positive turn about Z sweeps toward +Y: face the sweep, or the
            // solid comes out inside-out.
            let upright = facing(upright, Vector3::new(0.0, 1.0, 0.0))?;
            let solid: Solid = builder::rsweep(&upright, origin, Vector3::new(0.0, 0.0, 1.0), Rad(degrees.to_radians()));
            Ok(bodies_of(solid, &profile_style(profile, style.clone())))
        }
        ModelSpec::Sphere { radius } => {
            validate_primitive(spec)?;
            // A half circle from pole to pole, spun about Z (`cone` drops the
            // degenerate edges at the poles).
            let top = builder::vertex(Point3::new(0.0, 0.0, *radius));
            let bottom = builder::vertex(Point3::new(0.0, 0.0, -*radius));
            let arc = builder::circle_arc(&top, &bottom, Point3::new(*radius, 0.0, 0.0));
            let shell = builder::cone(&Wire::from(vec![arc]), Vector3::new(0.0, 0.0, 1.0), Rad(std::f64::consts::TAU));
            Ok(vec![Body {
                shell,
                style: style.clone(),
            }])
        }
        ModelSpec::Cylinder { .. } | ModelSpec::Cone { .. } | ModelSpec::Torus { .. } => {
            validate_primitive(spec)?;
            build_bodies(&as_sweep(spec), style, plates)
        }
        ModelSpec::Translate { offset, shape } => Ok(build_bodies(shape, style, plates)?
            .into_iter()
            .map(|body| body.map(|shell| builder::translated(shell, to_vector(*offset))))
            .collect()),
        ModelSpec::Rotate {
            axis,
            degrees,
            origin,
            shape,
        } => {
            let rotation = Rotation::new(*axis, *degrees, *origin)?;
            Ok(build_bodies(shape, style, plates)?
                .into_iter()
                .map(|body| body.map(|shell| rotation.apply(shell)))
                .collect())
        }
        ModelSpec::Group { children } => {
            if children.is_empty() {
                return Err(KernelError::Invalid("group needs at least one child".into()));
            }
            let mut bodies = Vec::new();
            for child in children {
                bodies.extend(build_bodies(child, style, plates)?);
            }
            Ok(bodies)
        }
        ModelSpec::Union { children } => {
            let mut operands = build_operands("union", children, style, plates)?.into_iter();
            let (mut acc, result_style) = operands.next().ok_or_else(|| {
                KernelError::Invalid("union needs at least one shape".into())
            })?;
            for (solid, _) in operands {
                acc = boolean("union", UNION_HINT, || truck_shapeops::or(&acc, &solid, BOOLEAN_TOL))?;
            }
            Ok(bodies_of(acc, &result_style))
        }
        ModelSpec::Intersect { children } => {
            let mut operands = build_operands("intersect", children, style, plates)?.into_iter();
            let (mut acc, result_style) = operands.next().ok_or_else(|| {
                KernelError::Invalid("intersect needs at least one shape".into())
            })?;
            for (solid, _) in operands {
                if !bounds_overlap(&acc, &solid) {
                    return Err(KernelError::Invalid("intersect is empty: the shapes don't overlap".into()));
                }
                acc = boolean("intersect", UNION_HINT, || truck_shapeops::and(&acc, &solid, BOOLEAN_TOL))?;
            }
            Ok(bodies_of(acc, &result_style))
        }
        ModelSpec::Difference { shape, tools } => {
            if tools.is_empty() {
                return Err(KernelError::Invalid("difference needs at least one tool".into()));
            }
            let tools = build_operands("difference", tools, style, plates)?;
            let mut result = Vec::new();
            for (mut acc, base_style) in build_operands("difference", std::slice::from_ref(&**shape), style, plates)? {
                for (tool, _) in &tools {
                    if !bounds_overlap(&acc, tool) {
                        continue; // Nothing to cut; truck would report failure.
                    }
                    let mut inverted = tool.clone();
                    inverted.not();
                    acc = boolean("difference", DIFFERENCE_HINT, || {
                        truck_shapeops::and(&acc, &inverted, BOOLEAN_TOL)
                    })?;
                }
                result.extend(bodies_of(acc, &base_style));
            }
            Ok(result)
        }
        ModelSpec::Fillet { .. } | ModelSpec::Chamfer { .. } => Err(KernelError::Unsupported(
            "fillet and chamfer need the OCC kernel (the default; --kernel occ)".into(),
        )),
        ModelSpec::Color { rgb, shape } => build_bodies(
            shape,
            &Style {
                color: *rgb,
                ..style.clone()
            },
            plates,
        ),
        ModelSpec::Part { name, shape } => build_bodies(
            shape,
            &Style {
                part: Some(validate_part_name(name)?),
                ..style.clone()
            },
            plates,
        ),
        ModelSpec::Arrange { gap, children, bed } => {
            if children.is_empty() {
                return Err(KernelError::Invalid("arrange needs at least one child".into()));
            }
            if !(*gap >= 0.0 && gap.is_finite()) {
                return Err(KernelError::Invalid("arrange gap must be >= 0".into()));
            }
            if let Some(bed) = bed {
                let dims = [bed.width, bed.depth, bed.height];
                if !dims.iter().all(|d| *d > 0.0 && d.is_finite()) {
                    return Err(KernelError::Invalid("bed width, depth and height must be positive".into()));
                }
            }

            let mut built = Vec::new();
            let mut footprints = Vec::new();
            for child in children {
                let bodies = build_bodies(child, style, plates)?;
                let (min, max) = bodies_bounds(&bodies)
                    .ok_or_else(|| KernelError::Invalid("arrange child has no geometry".into()))?;
                footprints.push([max.x - min.x, max.y - min.y]);
                built.push((bodies, min));
            }

            let packing = pack(&footprints, *gap, bed.as_ref());
            plates.extend(packing.plates);
            let mut arranged = Vec::new();
            for ((bodies, min), [x, y]) in built.into_iter().zip(packing.positions) {
                let offset = Vector3::new(x - min.x, y - min.y, -min.z);
                arranged.extend(
                    bodies
                        .into_iter()
                        .map(|body| body.map(|shell| builder::translated(shell, offset))),
                );
            }
            Ok(arranged)
        }
    }
}

const UNION_HINT: &str = "truck can't combine solids that only touch or whose faces line up \
exactly. Make them overlap so their faces cross (e.g. sink one 0.5 mm into the other).";
const DIFFERENCE_HINT: &str = "truck can't cut where a tool's face lines up exactly with a face \
of the shape. Make tools overshoot the faces they cut through (e.g. by 1 mm).";

/// Builds each child into solids (one per closed body) with its style.
fn build_operands(
    op: &str,
    children: &[ModelSpec],
    style: &Style,
    plates: &mut Vec<Plate>,
) -> Result<Vec<(Solid, Style)>, KernelError> {
    let mut operands = Vec::new();
    for child in children {
        for body in build_bodies(child, style, plates)? {
            let solid = Solid::try_new(vec![body.shell]).map_err(|_| {
                KernelError::Invalid(format!("{op} needs solids (e.g. extrusions), not 2D profiles"))
            })?;
            operands.push((solid, body.style));
        }
    }
    Ok(operands)
}

/// Runs a truck-shapeops operation, turning its failures (and panics, which it
/// raises on some coplanar inputs) into an error with a hint. The result's
/// edges are made explicit (see [`explicit_curves`]).
fn boolean(op: &str, hint: &str, run: impl FnOnce() -> Option<Solid>) -> Result<Solid, KernelError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)) {
        Ok(Some(solid)) if !solid.boundaries().is_empty() => Ok(explicit_curves(&solid)),
        _ => Err(KernelError::Invalid(format!("{op} failed: {hint}"))),
    }
}

/// Replaces the implicit intersection curves a boolean leaves on its new edges
/// with explicit ones: a line when the curve is straight, else a fine
/// polyline. truck evaluates intersection curves numerically, and after some
/// transforms (e.g. arrange moving a part to (83, 140.2)) that evaluation
/// never converges and tessellation hangs. Sampling right after the boolean,
/// before anything moves, is reliable.
fn explicit_curves(solid: &Solid) -> Solid {
    solid.mapped(
        |p| *p,
        |curve| match curve {
            Curve::IntersectionCurve(_) => {
                let (t0, t1) = curve.range_tuple();
                let points: Vec<Point3> = (0..=CURVE_SEGMENTS)
                    .map(|i| curve.subs(t0 + (t1 - t0) * i as f64 / CURVE_SEGMENTS as f64))
                    .collect();
                let (first, last) = (points[0], points[CURVE_SEGMENTS]);
                let chord = last - first;
                let straight = points.iter().all(|p| {
                    let d = *p - first;
                    let off_line = d - chord * (d.dot(chord) / chord.magnitude2().max(1e-300));
                    off_line.magnitude() < 1e-7
                });
                if straight && chord.magnitude() > 1e-12 {
                    Curve::Line(Line(first, last))
                } else {
                    Curve::BSplineCurve(BSplineCurve::new(KnotVec::uniform_knot(1, CURVE_SEGMENTS), points))
                }
            }
            other => other.clone(),
        },
        |surface| surface.clone(),
    )
}

fn bodies_of(solid: Solid, style: &Style) -> Vec<Body> {
    solid
        .into_boundaries()
        .into_iter()
        .map(|shell| Body {
            shell,
            style: style.clone(),
        })
        .collect()
}

fn solid_bounds(solid: &Solid) -> Option<(Point3, Point3)> {
    bounds_of(solid.boundaries().iter().flat_map(shell_points))
}

/// Whether two solids' bounding boxes overlap with positive volume.
fn bounds_overlap(a: &Solid, b: &Solid) -> bool {
    match (solid_bounds(a), solid_bounds(b)) {
        (Some((amin, amax)), Some((bmin, bmax))) => {
            amin.x < bmax.x && bmin.x < amax.x && amin.y < bmax.y && bmin.y < amax.y && amin.z < bmax.z && bmin.z < amax.z
        }
        _ => false,
    }
}

fn validate_part_name(name: &str) -> Result<String, KernelError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(KernelError::Invalid("part name must not be empty".into()));
    }
    Ok(trimmed.to_string())
}

/// Bounding box of the bodies' (sampled) edges; see [`shell_points`].
fn bodies_bounds(bodies: &[Body]) -> Option<(Point3, Point3)> {
    bounds_of(bodies.iter().flat_map(|body| shell_points(&body.shell)))
}

/// The same solid as an extrusion or revolve, which truck builds exactly:
/// a cylinder is an extruded circle, a cone a revolved triangle or
/// trapezoid, a torus a revolved circle. Only for `Cylinder`, `Cone`, `Torus`.
fn as_sweep(spec: &ModelSpec) -> ModelSpec {
    let revolve = |profile: ModelSpec| ModelSpec::Revolve {
        profile: Box::new(profile),
        degrees: 360.0,
    };
    match *spec {
        ModelSpec::Cylinder { radius, height } => ModelSpec::Extrude {
            profile: Box::new(ModelSpec::Circle { radius }),
            distance: height,
            direction: None,
        },
        ModelSpec::Cone { radius, top, height } => {
            let mut points = vec![[0.0, 0.0]];
            if radius > 0.0 {
                points.push([radius, 0.0]);
            }
            if top > 0.0 {
                points.push([top, height]);
            }
            points.push([0.0, height]);
            revolve(ModelSpec::Polygon { points })
        }
        ModelSpec::Torus { radius, tube } => revolve(ModelSpec::Translate {
            offset: [radius, 0.0, 0.0],
            shape: Box::new(ModelSpec::Circle { radius: tube }),
        }),
        _ => unreachable!("as_sweep takes a cylinder, cone or torus"),
    }
}

/// Style set inside a profile (innermost wins), which its extrusion inherits
/// over `inherited`.
fn profile_style(spec: &ModelSpec, inherited: Style) -> Style {
    match spec {
        ModelSpec::Color { rgb, shape } => profile_style(
            shape,
            Style {
                color: *rgb,
                ..inherited
            },
        ),
        ModelSpec::Part { name, shape } => profile_style(
            shape,
            Style {
                part: Some(name.trim().to_string()),
                ..inherited
            },
        ),
        ModelSpec::Translate { shape, .. } | ModelSpec::Rotate { shape, .. } => {
            profile_style(shape, inherited)
        }
        ModelSpec::Cutout { outer, .. } => profile_style(outer, inherited),
        _ => inherited,
    }
}

/// A validated `Rotate` node: unit axis, finite angle.
struct Rotation {
    origin: Point3,
    axis: Vector3,
    angle: Rad<f64>,
}

impl Rotation {
    fn new(axis: [f64; 3], degrees: f64, origin: [f64; 3]) -> Result<Self, KernelError> {
        let axis = to_vector(axis);
        let length = axis.magnitude();
        if !(length > 1e-12 && length.is_finite()) {
            return Err(KernelError::Invalid("rotate axis must be a non-zero vector".into()));
        }
        if !degrees.is_finite() || !origin.iter().all(|c| c.is_finite()) {
            return Err(KernelError::Invalid("rotate angle and origin must be finite".into()));
        }
        Ok(Self {
            origin: Point3::new(origin[0], origin[1], origin[2]),
            axis: axis / length,
            angle: Rad(degrees.to_radians()),
        })
    }

    fn apply<T: truck_modeling::Mapped<Point3, truck_modeling::Curve, truck_modeling::Surface>>(
        &self,
        elem: &T,
    ) -> T {
        builder::rotated(elem, self.origin, self.axis, self.angle)
    }
}

/// Planar profile for sweeps. Solids and groups are rejected here.
fn build_face(spec: &ModelSpec) -> Result<Face, KernelError> {
    match spec {
        ModelSpec::Rect { width, height } => {
            if !(*width > 0.0 && *height > 0.0) {
                return Err(KernelError::Invalid(
                    "rect width and height must be positive".into(),
                ));
            }
            let v = builder::vertex(Point3::new(0.0, 0.0, 0.0));
            let e = builder::tsweep(&v, Vector3::new(*width, 0.0, 0.0));
            Ok(builder::tsweep(&e, Vector3::new(0.0, *height, 0.0)))
        }
        ModelSpec::Polygon { points } => {
            let points = validate_polygon(points)?;
            let vertices: Vec<_> = points
                .iter()
                .map(|[x, y]| builder::vertex(Point3::new(*x, *y, 0.0)))
                .collect();
            let edges: Vec<_> = (0..vertices.len())
                .map(|i| builder::line(&vertices[i], &vertices[(i + 1) % vertices.len()]))
                .collect();
            builder::try_attach_plane(&[Wire::from(edges)])
                .map_err(|e| KernelError::Message(format!("polygon face failed: {e}")))
        }
        ModelSpec::Circle { radius } => {
            if !(*radius > 0.0 && radius.is_finite()) {
                return Err(KernelError::Invalid("circle radius must be positive".into()));
            }
            let start = builder::vertex(Point3::new(*radius, 0.0, 0.0));
            let wire: Wire = builder::rsweep(
                &start,
                Point3::new(0.0, 0.0, 0.0),
                Vector3::new(0.0, 0.0, 1.0),
                Rad(std::f64::consts::TAU),
            );
            builder::try_attach_plane(&[wire])
                .map_err(|e| KernelError::Message(format!("circle face failed: {e}")))
        }
        ModelSpec::Cutout { outer, holes } => {
            if holes.is_empty() {
                return Err(KernelError::Invalid("cutout needs at least one hole".into()));
            }
            let outer_face = build_face(outer)?;
            let outer_wires = outer_face.boundaries();
            let Some(outer_wire) = outer_wires.first() else {
                return Err(KernelError::Invalid("cutout outer profile has no outline".into()));
            };
            let normal = wire_normal(outer_wire);
            let (omin, omax) = bounds_of(outer_wires.iter().flat_map(wire_points)).unwrap();
            let mut wires = outer_wires.clone();
            for hole in holes {
                for wire in build_face(hole)?.boundaries() {
                    let (hmin, hmax) = bounds_of(wire_points(&wire)).unwrap();
                    let inside = (0..3).all(|i| hmin[i] >= omin[i] - 1e-9 && hmax[i] <= omax[i] + 1e-9);
                    if !inside {
                        return Err(KernelError::Invalid("cutout hole must lie inside the outer profile".into()));
                    }
                    // Holes run against the outline.
                    wires.push(if wire_normal(&wire).dot(normal) > 0.0 { wire.inverse() } else { wire });
                }
            }
            builder::try_attach_plane(&wires).map_err(|e| {
                KernelError::Invalid(format!(
                    "cutout failed ({e}): holes must be coplanar with the outline, inside it and not touching"
                ))
            })
        }
        ModelSpec::Translate { offset, shape } => {
            Ok(builder::translated(&build_face(shape)?, to_vector(*offset)))
        }
        ModelSpec::Rotate {
            axis,
            degrees,
            origin,
            shape,
        } => Ok(Rotation::new(*axis, *degrees, *origin)?.apply(&build_face(shape)?)),
        // Style is applied by `build_bodies` via `profile_style`.
        ModelSpec::Color { shape, .. } => build_face(shape),
        ModelSpec::Part { name, shape } => {
            validate_part_name(name)?;
            build_face(shape)
        }
        ModelSpec::Extrude { .. }
        | ModelSpec::Revolve { .. }
        | ModelSpec::Sphere { .. }
        | ModelSpec::Cylinder { .. }
        | ModelSpec::Cone { .. }
        | ModelSpec::Torus { .. }
        | ModelSpec::Group { .. }
        | ModelSpec::Arrange { .. }
        | ModelSpec::Union { .. }
        | ModelSpec::Difference { .. }
        | ModelSpec::Intersect { .. }
        | ModelSpec::Fillet { .. }
        | ModelSpec::Chamfer { .. } => {
            Err(KernelError::Invalid(
                "extrude profile must be a single 2D shape (rect, optionally moved, colored or named)"
                    .into(),
            ))
        }
    }
}

/// `face` oriented so its normal points along `direction` (sweeping against
/// the normal would make an inside-out solid). Every profile `build_face`
/// makes has its boundary wound counter-clockwise about +Z.
fn facing(face: Face, direction: Vector3) -> Result<Face, KernelError> {
    let normal = face.boundaries().first().map(wire_normal).unwrap_or(Vector3::new(0.0, 0.0, 1.0));
    let along = normal.dot(direction) / normal.magnitude().max(1e-300);
    if along.abs() < 1e-6 {
        return Err(KernelError::Invalid(
            "extrude direction lies in the profile's plane: it would sweep no volume".into(),
        ));
    }
    Ok(if along < 0.0 { face.inverse() } else { face })
}

fn wire_points(wire: &Wire) -> impl Iterator<Item = Point3> + '_ {
    wire.edge_iter().flat_map(|edge| edge_points(edge))
}

/// Newell's normal of a closed wire (its orientation, scaled by its area).
fn wire_normal(wire: &Wire) -> Vector3 {
    let points: Vec<Point3> = wire_points(wire).collect();
    let mut n = Vector3::new(0.0, 0.0, 0.0);
    for (a, b) in points.iter().zip(points.iter().cycle().skip(1)) {
        n.x += (a.y - b.y) * (a.z + b.z);
        n.y += (a.z - b.z) * (a.x + b.x);
        n.z += (a.x - b.x) * (a.y + b.y);
    }
    n
}

fn to_vector([x, y, z]: [f64; 3]) -> Vector3 {
    Vector3::new(x, y, z)
}

/// Appends `body`'s triangles (meshed to `tolerance`, mm) and edges to
/// `scene`, and records its vertex and index ranges under its part name. A
/// body continuing the previous body's part extends that range instead of
/// starting a new one.
fn tessellate_into(scene: &mut TessellatedScene, body: &Body, tolerance: f64) {
    let vertex_start = scene.positions.len() as u32;
    let index_start = scene.indices.len() as u32;

    let mesh = body.shell.triangulation(tolerance).to_polygon();
    let mesh_positions = mesh.positions();
    let mesh_normals = mesh.normals();

    // truck indexes positions and normals separately; GPUs want one index per
    // (position, normal) pair so faces meeting at a corner keep crisp normals.
    let mut remap: HashMap<(usize, Option<usize>), u32> = HashMap::new();
    for tri in mesh.faces().triangle_iter() {
        let flat = flat_normal(tri.map(|v| mesh_positions[v.pos]));
        for v in tri {
            let index = *remap.entry((v.pos, v.nor)).or_insert_with(|| {
                let p = mesh_positions[v.pos];
                let n = v.nor.map(|i| mesh_normals[i]).unwrap_or(flat);
                scene.positions.push(to_f32(p.x, p.y, p.z));
                scene.normals.push(to_f32(n.x, n.y, n.z));
                scene.colors.push(body.style.color);
                (scene.positions.len() - 1) as u32
            });
            scene.indices.push(index);
        }
    }

    let mut seen = HashSet::new();
    for edge in body.shell.edge_iter() {
        if seen.insert(edge.id()) {
            for pair in edge_points(&edge).windows(2) {
                let (a, b) = (pair[0], pair[1]);
                scene.edges.push(to_f32(a.x, a.y, a.z));
                scene.edges.push(to_f32(b.x, b.y, b.z));
            }
        }
    }

    if let Some(name) = &body.style.part {
        let vertices = vertex_start..scene.positions.len() as u32;
        let indices = index_start..scene.indices.len() as u32;
        match scene.parts.last_mut() {
            Some(last)
                if &last.name == name
                    && last.vertices.end == vertices.start
                    && last.indices.end == indices.start =>
            {
                last.vertices.end = vertices.end;
                last.indices.end = indices.end;
            }
            _ => scene.parts.push(ScenePart {
                name: name.clone(),
                vertices,
                indices,
            }),
        }
    }
}

fn flat_normal([a, b, c]: [Point3; 3]) -> Vector3 {
    let n = (b - a).cross(c - a);
    if n.magnitude2() > 0.0 {
        n.normalize()
    } else {
        Vector3::new(0.0, 0.0, 1.0)
    }
}

fn to_f32(x: f64, y: f64, z: f64) -> [f32; 3] {
    [x as f32, y as f32, z as f32]
}

#[cfg(test)]
mod tests {
    use super::*;
    use cad_ir::Bed;

    fn rect(width: f64, height: f64) -> ModelSpec {
        ModelSpec::Rect { width, height }
    }

    fn extrude(profile: ModelSpec, distance: f64) -> ModelSpec {
        ModelSpec::Extrude {
            profile: Box::new(profile),
            distance,
            direction: None,
        }
    }

    fn paint(rgb: Rgb, shape: ModelSpec) -> ModelSpec {
        ModelSpec::Color {
            rgb,
            shape: Box::new(shape),
        }
    }

    const RED: Rgb = [255, 0, 0];
    const BLUE: Rgb = [0, 0, 255];

    /// Number of vertices painted each color, in first-seen order.
    fn color_counts(scene: &TessellatedScene) -> Vec<(Rgb, usize)> {
        let mut counts: Vec<(Rgb, usize)> = Vec::new();
        for c in &scene.colors {
            match counts.iter_mut().find(|(rgb, _)| rgb == c) {
                Some((_, n)) => *n += 1,
                None => counts.push((*c, 1)),
            }
        }
        counts
    }

    fn assert_aabb(scene: &TessellatedScene, min: [f32; 3], max: [f32; 3]) {
        let aabb = scene.aabb().unwrap();
        for i in 0..3 {
            assert!((aabb.min[i] - min[i]).abs() < 1e-4, "{aabb:?}");
            assert!((aabb.max[i] - max[i]).abs() < 1e-4, "{aabb:?}");
        }
    }

    #[test]
    fn rect_tessellates_to_xy_square() {
        let scene = TruckKernel.build(&rect(10.0, 10.0)).unwrap();

        assert!(scene.indices.len() >= 6);
        assert_eq!(scene.indices.len() % 3, 0);
        assert_eq!(scene.normals.len(), scene.positions.len());
        assert_eq!(scene.colors.len(), scene.positions.len());
        assert_aabb(&scene, [0.0; 3], [10.0, 10.0, 0.0]);
        assert_eq!(scene.edges.len(), 4 * 2);
    }

    #[test]
    fn extruded_rect_is_a_cube() {
        let scene = TruckKernel.build(&extrude(rect(10.0, 10.0), 10.0)).unwrap();

        // 6 faces, at least 2 triangles each.
        assert!(scene.indices.len() >= 6 * 2 * 3);
        assert_aabb(&scene, [0.0; 3], [10.0; 3]);
        assert_eq!(scene.edges.len(), 12 * 2);

        // Every normal is axis-aligned: flat faces, no smoothing across corners.
        for n in &scene.normals {
            let axis_aligned = n.iter().filter(|c| c.abs() > 0.999).count() == 1;
            assert!(axis_aligned, "{n:?}");
        }
    }

    #[test]
    fn extrude_maps_rect_to_xy_and_distance_to_z() {
        let scene = TruckKernel.build(&extrude(rect(1.0, 2.0), 3.0)).unwrap();
        assert_aabb(&scene, [0.0; 3], [1.0, 2.0, 3.0]);
    }

    fn translate(offset: [f64; 3], shape: ModelSpec) -> ModelSpec {
        ModelSpec::Translate {
            offset,
            shape: Box::new(shape),
        }
    }

    fn unit_box() -> ModelSpec {
        extrude(rect(1.0, 1.0), 1.0)
    }

    #[test]
    fn translate_moves_solid() {
        let scene = TruckKernel.build(&translate([5.0, -2.0, 3.0], unit_box())).unwrap();
        assert_aabb(&scene, [5.0, -2.0, 3.0], [6.0, -1.0, 4.0]);
    }

    #[test]
    fn extrude_of_translated_profile_starts_at_offset() {
        let profile = translate([2.0, 3.0, 4.0], rect(1.0, 1.0));
        let scene = TruckKernel.build(&extrude(profile, 2.0)).unwrap();
        assert_aabb(&scene, [2.0, 3.0, 4.0], [3.0, 4.0, 6.0]);
    }

    #[test]
    fn group_merges_children_into_one_scene() {
        let one = TruckKernel.build(&unit_box()).unwrap();
        let group = ModelSpec::Group {
            children: vec![unit_box(), translate([3.0, 0.0, 0.0], unit_box())],
        };
        let scene = TruckKernel.build(&group).unwrap();

        assert_aabb(&scene, [0.0; 3], [4.0, 1.0, 1.0]);
        assert_eq!(scene.indices.len(), one.indices.len() * 2);
        assert_eq!(scene.positions.len(), one.positions.len() * 2);
        assert_eq!(scene.edges.len(), one.edges.len() * 2);
        // Second child's indices point at its own vertices, not the first child's.
        let second = &scene.indices[one.indices.len()..];
        assert!(second.iter().all(|&i| i as usize >= one.positions.len()));
    }

    #[test]
    fn nested_groups_and_translates_compose() {
        let row = ModelSpec::Group {
            children: vec![unit_box(), translate([2.0, 0.0, 0.0], unit_box())],
        };
        let spec = ModelSpec::Group {
            children: vec![row.clone(), translate([0.0, 0.0, 5.0], row)],
        };
        let scene = TruckKernel.build(&spec).unwrap();
        assert_aabb(&scene, [0.0; 3], [3.0, 1.0, 6.0]);
    }

    #[test]
    fn rejects_bad_groups() {
        let empty = ModelSpec::Group { children: vec![] };
        assert!(TruckKernel.build(&empty).is_err());

        let group_profile = ModelSpec::Group {
            children: vec![rect(1.0, 1.0)],
        };
        assert!(TruckKernel.build(&extrude(group_profile, 1.0)).is_err());

        // An invalid child fails the whole build rather than being dropped.
        let bad_child = ModelSpec::Group {
            children: vec![unit_box(), rect(-1.0, 1.0)],
        };
        assert!(TruckKernel.build(&bad_child).is_err());
    }

    #[test]
    fn uncolored_geometry_gets_default_color() {
        let scene = TruckKernel.build(&unit_box()).unwrap();
        assert_eq!(color_counts(&scene), vec![(DEFAULT_COLOR, scene.positions.len())]);
    }

    #[test]
    fn color_paints_every_vertex_of_its_shape() {
        let scene = TruckKernel.build(&paint(RED, unit_box())).unwrap();
        assert_eq!(color_counts(&scene), vec![(RED, scene.positions.len())]);
    }

    #[test]
    fn innermost_color_wins() {
        let spec = paint(
            BLUE,
            ModelSpec::Group {
                children: vec![
                    unit_box(),
                    translate([3.0, 0.0, 0.0], paint(RED, unit_box())),
                ],
            },
        );
        let scene = TruckKernel.build(&spec).unwrap();
        let half = scene.positions.len() / 2;
        assert_eq!(color_counts(&scene), vec![(BLUE, half), (RED, half)]);

        // Colors stay attached to their own geometry: red vertices are the moved box.
        for (p, c) in scene.positions.iter().zip(&scene.colors) {
            assert_eq!(*c == RED, p[0] >= 3.0, "{p:?} painted {c:?}");
        }
    }

    #[test]
    fn colored_profile_colors_its_extrusion() {
        let profile = translate([1.0, 0.0, 0.0], paint(RED, rect(1.0, 1.0)));
        let scene = TruckKernel.build(&paint(BLUE, extrude(profile, 1.0))).unwrap();
        assert_eq!(color_counts(&scene), vec![(RED, scene.positions.len())]);
    }

    #[test]
    fn color_outside_extrude_applies_when_profile_is_plain() {
        let scene = TruckKernel.build(&paint(BLUE, unit_box())).unwrap();
        assert_eq!(color_counts(&scene), vec![(BLUE, scene.positions.len())]);
    }

    #[test]
    fn color_does_not_change_geometry() {
        let plain = TruckKernel.build(&unit_box()).unwrap();
        let painted = TruckKernel.build(&paint(RED, unit_box())).unwrap();
        assert_eq!(plain.positions, painted.positions);
        assert_eq!(plain.normals, painted.normals);
        assert_eq!(plain.indices, painted.indices);
        assert_eq!(plain.edges, painted.edges);
    }

    fn arrange(gap: f64, children: Vec<ModelSpec>) -> ModelSpec {
        ModelSpec::Arrange {
            gap,
            children,
            bed: None,
        }
    }

    fn arrange_on(bed: Bed, gap: f64, children: Vec<ModelSpec>) -> ModelSpec {
        ModelSpec::Arrange {
            gap,
            children,
            bed: Some(bed),
        }
    }

    fn bed(width: f64, depth: f64) -> Bed {
        Bed {
            width,
            depth,
            height: 250.0,
        }
    }

    fn sized_box(x: f64, y: f64, z: f64) -> ModelSpec {
        extrude(rect(x, y), z)
    }

    /// AABBs of consecutive `per_child`-vertex runs (one per arranged box).
    fn child_bounds(scene: &TessellatedScene, per_child: usize) -> Vec<cad_ir::Aabb> {
        scene
            .positions
            .chunks(per_child)
            .map(|chunk| {
                TessellatedScene {
                    positions: chunk.to_vec(),
                    ..Default::default()
                }
                .aabb()
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn arrange_puts_children_on_plate_in_a_row() {
        // Children start floating, below the plate, and overlapping each other.
        // The bed is wide enough that they share one row.
        let spec = arrange_on(
            bed(1000.0, 1000.0),
            5.0,
            vec![
                translate([100.0, 50.0, 20.0], sized_box(10.0, 20.0, 2.0)),
                translate([-7.0, -3.0, -40.0], sized_box(30.0, 5.0, 4.0)),
                sized_box(1.0, 1.0, 1.0),
            ],
        );
        let scene = TruckKernel.build(&spec).unwrap();
        let per_child = TruckKernel.build(&unit_box()).unwrap().positions.len();
        let bounds = child_bounds(&scene, per_child);

        // The 51 x 20 row is centered on the 1000 x 1000 bed: offset (474.5, 490).
        let expected = [
            ([474.5, 490.0, 0.0], [484.5, 510.0, 2.0]),
            ([489.5, 490.0, 0.0], [519.5, 495.0, 4.0]),
            ([524.5, 490.0, 0.0], [525.5, 491.0, 1.0]),
        ];
        assert_eq!(bounds.len(), expected.len());
        for (got, (min, max)) in bounds.iter().zip(expected) {
            for i in 0..3 {
                assert!((got.min[i] - min[i]).abs() < 1e-4, "{got:?}");
                assert!((got.max[i] - max[i]).abs() < 1e-4, "{got:?}");
            }
        }
    }

    #[test]
    fn arrange_with_bed_records_plates_in_the_scene() {
        let coaster = sized_box(90.0, 90.0, 4.0);
        let scene = TruckKernel
            .build(&arrange_on(bed(256.0, 256.0), 10.0, vec![coaster; 5]))
            .unwrap();
        assert_eq!(scene.plates.len(), 2);
        let without_bed = TruckKernel.build(&arrange(10.0, vec![unit_box(); 3])).unwrap();
        assert!(without_bed.plates.is_empty());
    }

    #[test]
    fn single_part_lands_in_the_middle_of_the_bed() {
        let scene = TruckKernel
            .build(&arrange_on(bed(256.0, 200.0), 10.0, vec![sized_box(90.0, 40.0, 4.0)]))
            .unwrap();
        assert_aabb(&scene, [83.0, 80.0, 0.0], [173.0, 120.0, 4.0]);
    }

    #[test]
    fn rejects_bad_beds() {
        for bad in [bed(0.0, 256.0), bed(256.0, f64::NAN), bed(-1.0, 1.0)] {
            assert!(TruckKernel.build(&arrange_on(bad, 1.0, vec![unit_box()])).is_err());
        }
    }

    #[test]
    fn arrange_moves_groups_as_a_unit() {
        let pair = ModelSpec::Group {
            children: vec![unit_box(), translate([0.0, 0.0, 5.0], unit_box())],
        };
        let scene = TruckKernel
            .build(&arrange(0.0, vec![translate([9.0, 9.0, 9.0], pair)]))
            .unwrap();
        // Stack keeps its internal spacing; the whole thing moved onto the plate,
        // centered on the origin.
        assert_aabb(&scene, [-0.5, -0.5, 0.0], [0.5, 0.5, 6.0]);
    }

    #[test]
    fn arrange_keeps_colors_and_geometry() {
        let children = vec![paint(RED, unit_box()), sized_box(2.0, 2.0, 2.0)];
        let plain = TruckKernel.build(&ModelSpec::Group { children: children.clone() }).unwrap();
        let arranged = TruckKernel.build(&arrange(1.0, children)).unwrap();

        assert_eq!(color_counts(&plain), color_counts(&arranged));
        assert_eq!(plain.indices, arranged.indices);
        assert_eq!(plain.edges.len(), arranged.edges.len());
    }

    #[test]
    fn rejects_bad_arranges() {
        assert!(TruckKernel.build(&arrange(1.0, vec![])).is_err());
        assert!(TruckKernel.build(&arrange(-1.0, vec![unit_box()])).is_err());
        assert!(TruckKernel.build(&arrange(f64::NAN, vec![unit_box()])).is_err());
        assert!(TruckKernel.build(&extrude(arrange(1.0, vec![rect(1.0, 1.0)]), 1.0)).is_err());
    }

    fn rotate(axis: [f64; 3], degrees: f64, origin: [f64; 3], shape: ModelSpec) -> ModelSpec {
        ModelSpec::Rotate {
            axis,
            degrees,
            origin,
            shape: Box::new(shape),
        }
    }

    #[test]
    fn rotate_about_z_turns_box_sideways() {
        let spec = rotate([0.0, 0.0, 1.0], 90.0, [0.0; 3], sized_box(2.0, 1.0, 1.0));
        let scene = TruckKernel.build(&spec).unwrap();
        // (x, y) -> (-y, x): the 2-long side now runs along +Y.
        assert_aabb(&scene, [-1.0, 0.0, 0.0], [0.0, 2.0, 1.0]);
    }

    #[test]
    fn rotate_uses_origin_and_normalizes_axis() {
        // About +X through (0, 1, 0): (y, z) -> (1 - z, y - 1). The axis is
        // deliberately not unit length.
        let spec = rotate([5.0, 0.0, 0.0], 90.0, [0.0, 1.0, 0.0], sized_box(1.0, 1.0, 1.0));
        let scene = TruckKernel.build(&spec).unwrap();
        assert_aabb(&scene, [0.0, 0.0, -1.0], [1.0, 1.0, 0.0]);

        // Same rotation pivoting at y = 5: (y, z) -> (5 - z, y - 5).
        let scene2 = TruckKernel
            .build(&rotate([1.0, 0.0, 0.0], 90.0, [0.0, 5.0, 0.0], sized_box(1.0, 1.0, 1.0)))
            .unwrap();
        assert_aabb(&scene2, [0.0, 4.0, -5.0], [1.0, 5.0, -4.0]);
    }

    #[test]
    fn rotated_normals_stay_unit_and_follow_faces() {
        let spec = rotate([0.0, 1.0, 0.0], 90.0, [0.0; 3], unit_box());
        let scene = TruckKernel.build(&spec).unwrap();
        for n in &scene.normals {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-4, "{n:?}");
            assert_eq!(n.iter().filter(|c| c.abs() > 0.999).count(), 1, "{n:?}");
        }
    }

    #[test]
    fn full_turn_is_identity() {
        // Vertex order isn't guaranteed across transforms, so compare sorted sets.
        let sorted = |scene: TessellatedScene| {
            let mut points = scene.positions;
            for p in &mut points {
                *p = p.map(|c| (c * 1e3).round() / 1e3 + 0.0);
            }
            points.sort_by(|a, b| a.partial_cmp(b).unwrap());
            points
        };
        let plain = sorted(TruckKernel.build(&sized_box(3.0, 2.0, 1.0)).unwrap());
        let turned = sorted(
            TruckKernel
                .build(&rotate([1.0, 1.0, 1.0], 360.0, [7.0, -2.0, 4.0], sized_box(3.0, 2.0, 1.0)))
                .unwrap(),
        );
        assert_eq!(plain.len(), turned.len());
        for (a, b) in plain.iter().zip(&turned) {
            for i in 0..3 {
                assert!((a[i] - b[i]).abs() < 1e-4, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn rotated_profile_extrudes_and_keeps_color() {
        // A rect spun 90 degrees in plane, then extruded: still a box, still red.
        let profile = rotate([0.0, 0.0, 1.0], 90.0, [0.0; 3], paint(RED, rect(2.0, 1.0)));
        let scene = TruckKernel.build(&extrude(profile, 3.0)).unwrap();
        assert_aabb(&scene, [-1.0, 0.0, 0.0], [0.0, 2.0, 3.0]);
        assert_eq!(color_counts(&scene), vec![(RED, scene.positions.len())]);
    }

    #[test]
    fn arrange_lands_rotated_parts_on_plate() {
        // A standing board (1 x 10 x 20) laid flat by rotating about Y.
        let standing = sized_box(1.0, 10.0, 20.0);
        let flat = rotate([0.0, 1.0, 0.0], 90.0, [0.0; 3], standing);
        let scene = TruckKernel.build(&arrange(0.0, vec![flat])).unwrap();
        assert_aabb(&scene, [-10.0, -5.0, 0.0], [10.0, 5.0, 1.0]);
    }

    #[test]
    fn rejects_bad_rotations() {
        for spec in [
            rotate([0.0; 3], 90.0, [0.0; 3], unit_box()),
            rotate([0.0, 0.0, 1.0], f64::NAN, [0.0; 3], unit_box()),
            rotate([0.0, 0.0, 1.0], 90.0, [f64::INFINITY, 0.0, 0.0], unit_box()),
            rotate([f64::NAN, 0.0, 1.0], 90.0, [0.0; 3], unit_box()),
        ] {
            assert!(TruckKernel.build(&spec).is_err(), "{spec:?}");
        }
    }

    fn part(name: &str, shape: ModelSpec) -> ModelSpec {
        ModelSpec::Part {
            name: name.into(),
            shape: Box::new(shape),
        }
    }

    fn part_names(scene: &TessellatedScene) -> Vec<&str> {
        scene.parts.iter().map(|p| p.name.as_str()).collect()
    }

    /// Every part's indices stay inside its own vertex range, and ranges don't overlap.
    fn assert_parts_well_formed(scene: &TessellatedScene) {
        let mut previous_end = (0, 0);
        for part in &scene.parts {
            assert!(part.vertices.start >= previous_end.0 && part.indices.start >= previous_end.1);
            assert!(part.indices.len() % 3 == 0, "{part:?}");
            for &i in &scene.indices[part.indices.start as usize..part.indices.end as usize] {
                assert!(part.vertices.contains(&i), "{} index {i} escapes {:?}", part.name, part.vertices);
            }
            previous_end = (part.vertices.end, part.indices.end);
        }
    }

    #[test]
    fn parts_cover_their_own_geometry() {
        let spec = ModelSpec::Group {
            children: vec![
                part("a", unit_box()),
                unit_box(),
                translate([3.0, 0.0, 0.0], part("b", unit_box())),
            ],
        };
        let scene = TruckKernel.build(&spec).unwrap();
        assert_eq!(part_names(&scene), ["a", "b"]);
        assert_parts_well_formed(&scene);

        let one = TruckKernel.build(&unit_box()).unwrap();
        let (a, b) = (&scene.parts[0], &scene.parts[1]);
        assert_eq!(a.vertices, 0..one.positions.len() as u32);
        assert_eq!(a.indices.len(), one.indices.len());
        // The unnamed middle box sits between them, in no part.
        assert_eq!(b.vertices.start as usize, 2 * one.positions.len());
        assert_eq!(b.vertices.end as usize, scene.positions.len());
        // Part "b" is the translated box.
        for v in b.vertices.clone() {
            assert!(scene.positions[v as usize][0] >= 3.0);
        }
    }

    #[test]
    fn unnamed_models_have_no_parts() {
        let scene = TruckKernel.build(&unit_box()).unwrap();
        assert!(scene.parts.is_empty());
    }

    #[test]
    fn multi_body_part_is_one_range() {
        let pair = ModelSpec::Group {
            children: vec![unit_box(), translate([2.0, 0.0, 0.0], unit_box())],
        };
        let scene = TruckKernel.build(&part("pair", pair)).unwrap();
        assert_eq!(part_names(&scene), ["pair"]);
        assert_eq!(scene.parts[0].vertices, 0..scene.positions.len() as u32);
        assert_eq!(scene.parts[0].indices, 0..scene.indices.len() as u32);
    }

    #[test]
    fn innermost_part_name_wins() {
        let spec = part(
            "frame",
            ModelSpec::Group {
                children: vec![
                    unit_box(),
                    translate([2.0, 0.0, 0.0], part("leg", unit_box())),
                    translate([4.0, 0.0, 0.0], unit_box()),
                ],
            },
        );
        let scene = TruckKernel.build(&spec).unwrap();
        // The inner part splits the outer one into two runs.
        assert_eq!(part_names(&scene), ["frame", "leg", "frame"]);
        assert_parts_well_formed(&scene);
    }

    #[test]
    fn parts_survive_transforms_color_and_arrange() {
        let spec = arrange(
            5.0,
            vec![
                paint(RED, part("x", rotate([0.0, 0.0, 1.0], 90.0, [0.0; 3], unit_box()))),
                part("y", translate([9.0, 9.0, 9.0], sized_box(2.0, 2.0, 2.0))),
            ],
        );
        let scene = TruckKernel.build(&spec).unwrap();
        assert_eq!(part_names(&scene), ["x", "y"]);
        assert_parts_well_formed(&scene);
        let x = &scene.parts[0];
        assert!(x.vertices.clone().all(|v| scene.colors[v as usize] == RED));
    }

    #[test]
    fn named_profile_names_its_extrusion() {
        let scene = TruckKernel
            .build(&extrude(part("plate", rect(2.0, 2.0)), 1.0))
            .unwrap();
        assert_eq!(part_names(&scene), ["plate"]);
        assert_eq!(scene.parts[0].vertices, 0..scene.positions.len() as u32);
    }

    #[test]
    fn part_names_are_trimmed_and_must_not_be_blank() {
        let scene = TruckKernel.build(&part("  leg  ", unit_box())).unwrap();
        assert_eq!(part_names(&scene), ["leg"]);
        assert!(TruckKernel.build(&part("   ", unit_box())).is_err());
        assert!(TruckKernel.build(&extrude(part("", rect(1.0, 1.0)), 1.0)).is_err());
    }

    fn polygon(points: &[[f64; 2]]) -> ModelSpec {
        ModelSpec::Polygon {
            points: points.to_vec(),
        }
    }

    /// Area of the triangles whose normals point straight up.
    fn top_area(scene: &TessellatedScene) -> f64 {
        scene
            .indices
            .chunks_exact(3)
            .filter(|t| t.iter().all(|&i| scene.normals[i as usize][2] > 0.999))
            .map(|t| {
                let [a, b, c] = [t[0], t[1], t[2]].map(|i| scene.positions[i as usize].map(f64::from));
                ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).abs() / 2.0
            })
            .sum()
    }

    /// U-shaped outline (an open-front holder seen from above): 3 mm walls
    /// around a 20 x 15 opening.
    const U: [[f64; 2]; 8] = [
        [0.0, 0.0],
        [3.0, 0.0],
        [3.0, 15.0],
        [23.0, 15.0],
        [23.0, 0.0],
        [26.0, 0.0],
        [26.0, 18.0],
        [0.0, 18.0],
    ];

    #[test]
    fn polygon_square_matches_rect() {
        let square = TruckKernel
            .build(&extrude(polygon(&[[0.0, 0.0], [2.0, 0.0], [2.0, 3.0], [0.0, 3.0]]), 1.0))
            .unwrap();
        assert_aabb(&square, [0.0; 3], [2.0, 3.0, 1.0]);
        assert_eq!(square.edges.len(), 12 * 2);
        assert!((top_area(&square) - 6.0).abs() < 1e-6);
    }

    #[test]
    fn non_convex_polygon_tessellates_its_true_area() {
        let scene = TruckKernel.build(&extrude(polygon(&U), 10.0)).unwrap();
        assert_aabb(&scene, [0.0; 3], [26.0, 18.0, 10.0]);
        // 26 x 18 outline minus the 20 x 15 opening.
        assert!((top_area(&scene) - (26.0 * 18.0 - 20.0 * 15.0)).abs() < 1e-6, "{}", top_area(&scene));
        // 8 sides + top + bottom faces: 8 vertical edges, 8 top, 8 bottom.
        assert_eq!(scene.edges.len(), 24 * 2);
    }

    #[test]
    fn polygon_winding_and_closing_point_dont_matter() {
        let ccw = TruckKernel.build(&extrude(polygon(&U), 1.0)).unwrap();
        let mut cw = U.to_vec();
        cw.reverse();
        cw.push(cw[0]);
        let cw = TruckKernel.build(&extrude(polygon(&cw), 1.0)).unwrap();
        assert_eq!(ccw.indices.len(), cw.indices.len());
        assert!((top_area(&ccw) - top_area(&cw)).abs() < 1e-6);
        // Top faces face up either way.
        assert!(top_area(&cw) > 0.0);
    }

    #[test]
    fn rejects_bad_polygons() {
        for (points, why) in [
            (vec![[0.0, 0.0], [1.0, 0.0]], "at least 3"),
            (vec![[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]], "zero area"),
            (vec![[0.0, 0.0], [1.0, 0.0], [1.0, 0.0], [0.0, 1.0]], "repeated"),
            (vec![[0.0, 0.0], [2.0, 2.0], [2.0, 0.0], [0.0, 2.0]], "intersect"),
            (vec![[0.0, 0.0], [f64::NAN, 0.0], [0.0, 1.0]], "finite"),
        ] {
            let err = TruckKernel.build(&extrude(polygon(&points), 1.0)).unwrap_err();
            assert!(err.to_string().contains(why), "{points:?}: {err}");
        }
    }

    fn cuboid(min: [f64; 3], size: [f64; 3]) -> ModelSpec {
        translate(min, sized_box(size[0], size[1], size[2]))
    }

    fn union(children: Vec<ModelSpec>) -> ModelSpec {
        ModelSpec::Union { children }
    }

    fn difference(shape: ModelSpec, tools: Vec<ModelSpec>) -> ModelSpec {
        ModelSpec::Difference {
            shape: Box::new(shape),
            tools,
        }
    }

    fn intersect(children: Vec<ModelSpec>) -> ModelSpec {
        ModelSpec::Intersect { children }
    }

    fn ten() -> ModelSpec {
        cuboid([0.0; 3], [10.0; 3])
    }

    #[test]
    fn union_of_crossing_solids_is_one_body() {
        let spec = union(vec![ten(), cuboid([5.0, 5.5, 6.0], [10.0; 3])]);
        let scene = TruckKernel.build(&spec).unwrap();
        assert_aabb(&scene, [0.0; 3], [15.0, 15.5, 16.0]);
        // Fused: each cube's corner that sits inside the other cube is gone
        // (a plain group would still have both).
        let has = |q: [f32; 3]| scene.positions.iter().any(|p| (0..3).all(|i| (p[i] - q[i]).abs() < 1e-4));
        assert!(!has([10.0, 10.0, 10.0]), "first cube's buried corner survived");
        assert!(!has([5.0, 5.5, 6.0]), "second cube's buried corner survived");
        assert!(has([0.0, 0.0, 0.0]) && has([15.0, 15.5, 16.0]), "outer corners must remain");
    }

    #[test]
    fn difference_cuts_a_through_hole_with_an_overshooting_tool() {
        let spec = difference(ten(), vec![cuboid([3.0, 3.0, -1.0], [4.0, 4.0, 12.0])]);
        let scene = TruckKernel.build(&spec).unwrap();
        assert_aabb(&scene, [0.0; 3], [10.0; 3]);
        // The hole's corners exist on both faces (a plain box has none there).
        let has = |q: [f32; 3]| scene.positions.iter().any(|p| (0..3).all(|i| (p[i] - q[i]).abs() < 1e-4));
        for corner in [[3.0, 3.0, 0.0], [7.0, 7.0, 0.0], [3.0, 7.0, 10.0], [7.0, 3.0, 10.0]] {
            assert!(has(corner), "hole corner {corner:?} missing");
        }
    }

    #[test]
    fn difference_with_several_tools_and_a_miss() {
        let spec = difference(
            ten(),
            vec![
                cuboid([2.0, 2.0, 5.0], [2.0, 2.0, 6.0]),  // pocket from the top
                cuboid([6.0, 6.0, -1.0], [2.0, 2.0, 12.0]), // through-hole
                cuboid([50.0, 50.0, 50.0], [1.0; 3]),      // misses entirely: ignored
            ],
        );
        let scene = TruckKernel.build(&spec).unwrap();
        assert_aabb(&scene, [0.0; 3], [10.0; 3]);
    }

    #[test]
    fn intersect_keeps_the_overlap() {
        let scene = TruckKernel
            .build(&intersect(vec![ten(), cuboid([5.0, 5.5, 6.0], [10.0; 3])]))
            .unwrap();
        assert_aabb(&scene, [5.0, 5.5, 6.0], [10.0; 3]);
        let err = TruckKernel
            .build(&intersect(vec![ten(), cuboid([20.0, 0.0, 0.0], [1.0; 3])]))
            .unwrap_err();
        assert!(err.to_string().contains("don't overlap"), "{err}");
    }

    /// Card holder from models/desk_organizer.ts: a base fused with a leaning
    /// back rest, minus two leaning slots.
    fn card_holder() -> ModelSpec {
        let a = 20f64.to_radians();
        let lean = |p: ModelSpec, d: f64| ModelSpec::Extrude {
            profile: Box::new(p),
            distance: d,
            direction: Some([0.0, a.sin(), a.cos()]),
        };
        let fused = union(vec![
            sized_box(90.0, 50.0, 12.0),
            translate([5.0, 34.0, 2.0], lean(rect(80.0, 4.0), 70.0)),
        ]);
        difference(
            fused,
            vec![
                translate([3.0, 8.0, 6.0], lean(rect(84.0, 3.0), 20.0)),
                translate([3.0, 18.0, 6.0], lean(rect(84.0, 3.0), 20.0)),
            ],
        )
    }

    /// Builds on a thread so a hang fails the test instead of stalling it.
    fn build_within(spec: ModelSpec, seconds: u64) -> TessellatedScene {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(TruckKernel.build(&spec));
        });
        rx.recv_timeout(std::time::Duration::from_secs(seconds))
            .expect("build hung")
            .unwrap()
    }

    #[test]
    fn moved_boolean_results_tessellate_quickly_and_consistently() {
        // (83, 140.2) used to hang forever on truck's intersection curves.
        let counts: Vec<usize> = [[0.0; 3], [90.0, 10.0, 5.0], [83.0, 140.2, 0.0], [12.345, 6.789, 0.0]]
            .into_iter()
            .map(|offset| build_within(translate(offset, card_holder()), 10).indices.len())
            .collect();
        assert!(counts.iter().all(|c| *c == counts[0]), "same shape, different meshes: {counts:?}");
    }

    #[test]
    fn booleans_keep_the_first_operands_style() {
        let spec = paint(
            RED,
            part(
                "plate",
                difference(ten(), vec![paint(BLUE, cuboid([3.0, 3.0, -1.0], [4.0, 4.0, 12.0]))]),
            ),
        );
        let scene = TruckKernel.build(&spec).unwrap();
        assert_eq!(color_counts(&scene), vec![(RED, scene.positions.len())]);
        assert_eq!(part_names(&scene), ["plate"]);
    }

    /// truck-shapeops can't handle faces that line up exactly. These pin that
    /// down: each must fail with a hint, never panic. If a truck upgrade makes
    /// one succeed, this test tells us the limitation is gone.
    #[test]
    fn coplanar_booleans_fail_with_a_hint_instead_of_panicking() {
        let floor = || cuboid([0.0; 3], [20.0, 20.0, 2.0]);
        let unions = [
            ("shared bottom plane", union(vec![floor(), cuboid([0.0; 3], [3.0, 20.0, 15.0])])),
            ("touching faces", union(vec![floor(), cuboid([0.0, 0.0, 2.0], [3.0, 20.0, 15.0])])),
        ];
        for (case, spec) in unions {
            let err = TruckKernel.build(&spec).map(|_| ()).unwrap_err().to_string();
            assert!(err.contains("union failed") && err.contains("overlap"), "{case}: {err}");
        }
        let flush_hole = difference(ten(), vec![cuboid([3.0, 3.0, 0.0], [4.0, 4.0, 10.0])]);
        let err = TruckKernel.build(&flush_hole).map(|_| ()).unwrap_err().to_string();
        assert!(err.contains("difference failed") && err.contains("overshoot"), "{err}");
    }

    #[test]
    fn booleans_reject_profiles_and_empty_input() {
        let err = TruckKernel
            .build(&union(vec![ten(), rect(1.0, 1.0)]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not 2D profiles"), "{err}");
        assert!(TruckKernel.build(&union(vec![])).is_err());
        assert!(TruckKernel.build(&difference(ten(), vec![])).is_err());
        assert!(TruckKernel.build(&extrude(union(vec![ten()]), 1.0)).is_err());
    }

    fn circle(radius: f64) -> ModelSpec {
        ModelSpec::Circle { radius }
    }

    /// Mesh bounds of curved shapes: tessellated circles are inscribed
    /// polygons, so allow the tessellation tolerance.
    fn assert_mesh_bounds(scene: &TessellatedScene, min: [f32; 3], max: [f32; 3]) {
        let aabb = scene.aabb().unwrap();
        let tol = TESSELLATION_TOL as f32 + 1e-4;
        for i in 0..3 {
            assert!((aabb.min[i] - min[i]).abs() <= tol, "{aabb:?}");
            assert!((aabb.max[i] - max[i]).abs() <= tol, "{aabb:?}");
        }
    }

    fn cutout(outer: ModelSpec, holes: Vec<ModelSpec>) -> ModelSpec {
        ModelSpec::Cutout {
            outer: Box::new(outer),
            holes,
        }
    }

    #[test]
    fn coarser_quality_meshes_curves_with_fewer_triangles_within_tolerance() {
        let disc = extrude(circle(40.0), 5.0);
        let triangles: Vec<usize> = MeshQuality::ALL
            .iter()
            .map(|&q| {
                let scene = TruckKernel.build_with(&disc, q).unwrap();
                let aabb = scene.aabb().unwrap();
                let tol = q.chord_tolerance() as f32 + 1e-4;
                assert!(aabb.max[0] <= 40.0 + tol && aabb.max[0] >= 40.0 - tol, "{q:?}: {aabb:?}");
                scene.indices.len() / 3
            })
            .collect();
        assert!(triangles.windows(2).all(|w| w[0] < w[1]), "{triangles:?}");
    }

    #[test]
    fn cylinder_has_round_bounds_and_smooth_sides() {
        let scene = TruckKernel.build(&extrude(circle(10.0), 5.0)).unwrap();
        assert_mesh_bounds(&scene, [-10.0, -10.0, 0.0], [10.0, 10.0, 5.0]);
        // Side normals are horizontal and point outward (smooth, not faceted
        // per triangle): each side vertex's normal is its radial direction.
        let mut side = 0;
        for (p, n) in scene.positions.iter().zip(&scene.normals) {
            if n[2].abs() < 1e-3 {
                side += 1;
                let r = (p[0] * p[0] + p[1] * p[1]).sqrt();
                assert!((n[0] - p[0] / r).abs() < 1e-2 && (n[1] - p[1] / r).abs() < 1e-2, "{p:?} {n:?}");
            }
        }
        assert!(side > 32, "too few side vertices: {side}");
    }

    #[test]
    fn circle_edges_are_drawn_as_round_polylines() {
        let scene = TruckKernel.build(&extrude(circle(10.0), 5.0)).unwrap();
        let segments: Vec<_> = scene.edges.chunks_exact(2).collect();
        let on_rim = |p: [f32; 3]| ((p[0] * p[0] + p[1] * p[1]).sqrt() - 10.0).abs() < 1e-3;
        let rim = segments.iter().filter(|s| on_rim(s[0]) && on_rim(s[1]) && s[0][2] == s[1][2]).count();
        // Both rims, finely sampled.
        assert!(rim >= 2 * 32, "{rim} rim segments");
    }

    #[test]
    fn arrange_measures_curved_parts_correctly() {
        let scene = TruckKernel
            .build(&arrange(5.0, vec![extrude(circle(10.0), 2.0), extrude(circle(10.0), 2.0)]))
            .unwrap();
        // Two 20 mm discs and a 5 mm gap, centered: no overlap. The B-rep
        // bounds arrange uses are exact (x lands on +-22.5); the mesh is within
        // tessellation tolerance.
        assert_mesh_bounds(&scene, [-22.5, -10.0, 0.0], [22.5, 10.0, 2.0]);
        let xs = scene.positions.iter().map(|p| p[0]);
        let (lo, hi) = xs.fold((f32::MAX, f32::MIN), |(lo, hi), x| (lo.min(x), hi.max(x)));
        assert!((lo + 22.5).abs() < 1e-4 && (hi - 22.5).abs() < 1e-4, "{lo} {hi}");
    }

    #[test]
    fn cutout_profiles_extrude_to_holed_solids() {
        let washer = TruckKernel.build(&extrude(cutout(circle(10.0), vec![circle(4.0)]), 2.0)).unwrap();
        assert_mesh_bounds(&washer, [-10.0, -10.0, 0.0], [10.0, 10.0, 2.0]);
        // Inner wall exists: vertices at radius 4.
        assert!(washer
            .positions
            .iter()
            .any(|p| ((p[0] * p[0] + p[1] * p[1]).sqrt() - 4.0).abs() < 1e-3));

        let plate = cutout(
            rect(40.0, 20.0),
            vec![
                translate([10.0, 10.0, 0.0], circle(3.0)),
                polygon(&[[25.0, 5.0], [35.0, 5.0], [30.0, 15.0]]),
            ],
        );
        let scene = TruckKernel.build(&extrude(plate, 3.0)).unwrap();
        assert_aabb(&scene, [0.0; 3], [40.0, 20.0, 3.0]);
    }

    #[test]
    fn cutout_hole_winding_doesnt_matter() {
        let ccw = polygon(&[[5.0, 5.0], [10.0, 5.0], [10.0, 10.0], [5.0, 10.0]]);
        let cw = polygon(&[[5.0, 5.0], [5.0, 10.0], [10.0, 10.0], [10.0, 5.0]]);
        let a = TruckKernel.build(&extrude(cutout(rect(20.0, 20.0), vec![ccw]), 1.0)).unwrap();
        let b = TruckKernel.build(&extrude(cutout(rect(20.0, 20.0), vec![cw]), 1.0)).unwrap();
        assert_eq!(a.indices.len(), b.indices.len());
    }

    #[test]
    fn rejects_bad_circles_and_cutouts() {
        assert!(TruckKernel.build(&extrude(circle(0.0), 1.0)).is_err());
        assert!(TruckKernel.build(&extrude(circle(f64::NAN), 1.0)).is_err());
        assert!(TruckKernel.build(&extrude(cutout(rect(1.0, 1.0), vec![]), 1.0)).is_err());
        let outside = cutout(rect(10.0, 10.0), vec![translate([20.0, 0.0, 0.0], circle(1.0))]);
        let err = TruckKernel.build(&extrude(outside, 1.0)).unwrap_err().to_string();
        assert!(err.contains("inside the outer profile"), "{err}");
        // A cutout is a profile, not a solid.
        assert!(TruckKernel.build(&cutout(extrude(rect(1.0, 1.0), 1.0), vec![circle(0.1)])).is_err());
    }

    fn extrude_along(profile: ModelSpec, distance: f64, direction: [f64; 3]) -> ModelSpec {
        ModelSpec::Extrude {
            profile: Box::new(profile),
            distance,
            direction: Some(direction),
        }
    }

    fn revolve(profile: ModelSpec, degrees: f64) -> ModelSpec {
        ModelSpec::Revolve {
            profile: Box::new(profile),
            degrees,
        }
    }

    #[test]
    fn extrude_follows_its_direction() {
        let down = TruckKernel.build(&extrude_along(rect(2.0, 3.0), 4.0, [0.0, 0.0, -1.0])).unwrap();
        assert_aabb(&down, [0.0, 0.0, -4.0], [2.0, 3.0, 0.0]);
        // Normalized: length 5 or 1, same result.
        let slanted = TruckKernel.build(&extrude_along(rect(2.0, 3.0), 4.0, [0.0, 3.0, 4.0])).unwrap();
        assert_aabb(&slanted, [0.0, 0.0, 0.0], [2.0, 3.0 + 2.4, 3.2]);
        // A profile stood up (rotated into XZ) extruded along +Y.
        let standing = rotate([1.0, 0.0, 0.0], 90.0, [0.0; 3], rect(2.0, 3.0));
        let wall = TruckKernel.build(&extrude_along(standing, 5.0, [0.0, 1.0, 0.0])).unwrap();
        assert_aabb(&wall, [0.0, 0.0, 0.0], [2.0, 5.0, 3.0]);
    }

    #[test]
    fn extrude_rejects_bad_directions() {
        for (direction, why) in [([0.0; 3], "non-zero"), ([1.0, 0.0, 0.0], "lies in the profile's plane")] {
            let err = TruckKernel.build(&extrude_along(rect(1.0, 1.0), 1.0, direction)).unwrap_err();
            assert!(err.to_string().contains(why), "{err}");
        }
    }

    #[test]
    fn revolve_turns_xy_profiles_around_z() {
        // A 5..10 x 0..20 rect makes a tube standing on the XY plane.
        let tube = TruckKernel
            .build(&revolve(translate([5.0, 0.0, 0.0], rect(5.0, 20.0)), 360.0))
            .unwrap();
        assert_mesh_bounds(&tube, [-10.0, -10.0, 0.0], [10.0, 10.0, 20.0]);
        // Inner wall at radius 5.
        assert!(tube
            .positions
            .iter()
            .any(|p| ((p[0] * p[0] + p[1] * p[1]).sqrt() - 5.0).abs() < 1e-3));

        let quarter = TruckKernel
            .build(&revolve(translate([5.0, 0.0, 0.0], rect(5.0, 20.0)), 90.0))
            .unwrap();
        assert_mesh_bounds(&quarter, [0.0, 0.0, 0.0], [10.0, 10.0, 20.0]);
    }

    #[test]
    fn bounds_include_surfaces_that_bulge_between_edges() {
        // Torus: the outer equator isn't an edge, but arrange must still see it.
        let torus = revolve(translate([20.0, 0.0, 0.0], circle(5.0)), 360.0);
        let scene = TruckKernel.build(&arrange(0.0, vec![torus])).unwrap();
        assert_mesh_bounds(&scene, [-25.0, -25.0, 0.0], [25.0, 25.0, 10.0]);
    }

    #[test]
    fn revolve_rejects_bad_profiles() {
        let crosses = revolve(translate([-1.0, 0.0, 0.0], rect(2.0, 1.0)), 360.0);
        assert!(TruckKernel.build(&crosses).unwrap_err().to_string().contains("x >= 0"));
        let tilted = revolve(rotate([1.0, 0.0, 0.0], 30.0, [0.0; 3], translate([1.0, 0.0, 0.0], rect(1.0, 1.0))), 360.0);
        assert!(TruckKernel.build(&tilted).unwrap_err().to_string().contains("XY plane"));
        for degrees in [0.0, -10.0, 361.0, f64::NAN] {
            assert!(TruckKernel.build(&revolve(translate([1.0, 0.0, 0.0], rect(1.0, 1.0)), degrees)).is_err());
        }
    }

    #[test]
    fn fillet_and_chamfer_need_occ() {
        for spec in [
            ModelSpec::Fillet { shape: Box::new(unit_box()), radius: 0.1, edges: vec![] },
            ModelSpec::Chamfer { shape: Box::new(unit_box()), distance: 0.1, edges: vec![] },
        ] {
            let err = TruckKernel.build(&spec).unwrap_err();
            assert!(matches!(err, KernelError::Unsupported(_)), "{err}");
            assert!(err.to_string().contains("OCC kernel"), "{err}");
        }
    }

    #[test]
    fn rejects_non_positive_rect() {
        assert!(TruckKernel.build(&rect(0.0, 10.0)).is_err());
    }

    #[test]
    fn rejects_bad_extrudes() {
        assert!(TruckKernel.build(&extrude(rect(10.0, 10.0), 0.0)).is_err());
        let nested = extrude(extrude(rect(1.0, 1.0), 1.0), 1.0);
        assert!(TruckKernel.build(&nested).is_err());
    }
}
