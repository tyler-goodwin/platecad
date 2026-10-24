//! OpenCascade backend, through cadrum (statically linked OCCT 8).
//!
//! Same IR and output as the truck kernel. OCCT's booleans handle faces that
//! line up exactly, which truck can't. Spike limitation: a bare 2D profile at
//! the top of a view (e.g. `models/square.ts`) is unsupported; OCC builds and
//! meshes solids.

use cad_ir::{DEFAULT_COLOR, ModelSpec, Plate, Rgb, ScenePart, TessellatedScene};
use cad_kernel::{Kernel, KernelError, pack, validate_polygon};
use cadrum::{Boolean, DVec3, Edge, Solid, Tessellation};

/// Absolute chord tolerance (mm), matching the truck kernel's tessellation.
const TESSELLATION: Tessellation = Tessellation {
    deflection_linear: 0.01,
    deflection_angular: 0.5,
    relative_linear: false,
};

#[derive(Debug, Default)]
pub struct OccKernel;

impl Kernel for OccKernel {
    fn name(&self) -> &'static str {
        "occ"
    }

    fn build(&mut self, spec: &ModelSpec) -> Result<TessellatedScene, KernelError> {
        let mut plates = Vec::new();
        let bodies = build_solids(spec, &Style::default(), &mut plates)?;
        let mut scene = TessellatedScene::default();
        for (solid, style) in &bodies {
            tessellate_into(&mut scene, solid, style)?;
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

fn occ(op: &str) -> impl Fn(cadrum::Error) -> KernelError + '_ {
    move |e| KernelError::Message(format!("{op} failed in OpenCascade: {e}"))
}

fn invalid(message: impl Into<String>) -> KernelError {
    KernelError::Invalid(message.into())
}

fn vec3([x, y, z]: [f64; 3]) -> DVec3 {
    DVec3::new(x, y, z)
}

fn unit(axis: [f64; 3], what: &str) -> Result<DVec3, KernelError> {
    let v = vec3(axis);
    let length = v.length();
    if !(length > 1e-12 && length.is_finite()) {
        return Err(invalid(format!("{what} must be a non-zero vector")));
    }
    Ok(v / length)
}

fn part_name(name: &str) -> Result<String, KernelError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(invalid("part name must not be empty"));
    }
    Ok(trimmed.to_string())
}

/// A 2D profile as closed edge loops (outline first, then holes), plus the
/// style set inside it, which its sweep inherits.
fn build_profile(spec: &ModelSpec, style: Style) -> Result<(Vec<Vec<Edge>>, Style), KernelError> {
    match spec {
        ModelSpec::Rect { width, height } => {
            if !(*width > 0.0 && *height > 0.0) {
                return Err(invalid("rect width and height must be positive"));
            }
            let corners = [[0.0, 0.0], [*width, 0.0], [*width, *height], [0.0, *height]];
            Ok((vec![polygon_loop(&corners)?], style))
        }
        ModelSpec::Polygon { points } => Ok((vec![polygon_loop(&validate_polygon(points)?)?], style)),
        ModelSpec::Circle { radius } => {
            if !(*radius > 0.0 && radius.is_finite()) {
                return Err(invalid("circle radius must be positive"));
            }
            Ok((vec![vec![Edge::circle(*radius, DVec3::Z).map_err(occ("circle"))?]], style))
        }
        ModelSpec::Cutout { outer, holes } => {
            if holes.is_empty() {
                return Err(invalid("cutout needs at least one hole"));
            }
            let (mut loops, style) = build_profile(outer, style)?;
            for hole in holes {
                loops.extend(build_profile(hole, Style::default())?.0);
            }
            Ok((loops, style))
        }
        ModelSpec::Translate { offset, shape } => {
            let (loops, style) = build_profile(shape, style)?;
            let v = vec3(*offset);
            Ok((map_edges(loops, |e| e.translate(v)), style))
        }
        ModelSpec::Rotate {
            axis,
            degrees,
            origin,
            shape,
        } => {
            let (axis, angle, origin) = rotation(*axis, *degrees, *origin)?;
            let (loops, style) = build_profile(shape, style)?;
            Ok((map_edges(loops, |e| e.rotate(origin, axis, angle)), style))
        }
        ModelSpec::Color { rgb, shape } => build_profile(shape, Style { color: *rgb, ..style }),
        ModelSpec::Part { name, shape } => build_profile(
            shape,
            Style {
                part: Some(part_name(name)?),
                ..style
            },
        ),
        _ => Err(invalid(
            "extrude profile must be a single 2D shape (rect, polygon, circle or cutout, optionally moved, colored or named)",
        )),
    }
}

fn polygon_loop(points: &[[f64; 2]]) -> Result<Vec<Edge>, KernelError> {
    let points: Vec<DVec3> = points.iter().map(|[x, y]| DVec3::new(*x, *y, 0.0)).collect();
    Edge::polygon(&points).map_err(occ("polygon"))
}

fn map_edges(loops: Vec<Vec<Edge>>, f: impl Fn(Edge) -> Edge) -> Vec<Vec<Edge>> {
    loops.into_iter().map(|l| l.into_iter().map(&f).collect()).collect()
}

fn rotation(axis: [f64; 3], degrees: f64, origin: [f64; 3]) -> Result<(DVec3, f64, DVec3), KernelError> {
    let axis = unit(axis, "rotate axis")?;
    if !degrees.is_finite() || !origin.iter().all(|c| c.is_finite()) {
        return Err(invalid("rotate angle and origin must be finite"));
    }
    Ok((axis, degrees.to_radians(), vec3(origin)))
}

fn profile_points(loops: &[Vec<Edge>]) -> Vec<DVec3> {
    loops
        .iter()
        .flatten()
        .flat_map(|e| e.approximation_segments(TESSELLATION))
        .collect()
}

type Bodies = Vec<(Solid, Style)>;

fn build_solids(spec: &ModelSpec, style: &Style, plates: &mut Vec<Plate>) -> Result<Bodies, KernelError> {
    match spec {
        ModelSpec::Rect { .. } | ModelSpec::Polygon { .. } | ModelSpec::Circle { .. } | ModelSpec::Cutout { .. } => {
            Err(KernelError::Unsupported(
                "the OCC kernel builds solids only: extrude or revolve 2D profiles".into(),
            ))
        }
        ModelSpec::Extrude {
            profile,
            distance,
            direction,
        } => {
            if !(*distance > 0.0) {
                return Err(invalid("extrude distance must be positive"));
            }
            let direction = match direction {
                None => DVec3::Z,
                Some(d) => unit(*d, "extrude direction")?,
            };
            let (loops, style) = build_profile(profile, style.clone())?;
            let edges: Vec<Edge> = loops.into_iter().flatten().collect();
            let solid = Solid::extrude(&edges, direction * *distance).map_err(occ("extrude"))?;
            if solid.volume().abs() < 1e-9 {
                return Err(invalid(
                    "extrude direction lies in the profile's plane: it would sweep no volume",
                ));
            }
            Ok(vec![(solid, style)])
        }
        ModelSpec::Revolve { profile, degrees } => {
            if !(*degrees > 0.0 && *degrees <= 360.0) {
                return Err(invalid("revolve degrees must be in (0, 360]"));
            }
            let (loops, style) = build_profile(profile, style.clone())?;
            let points = profile_points(&loops);
            if points.iter().any(|p| p.z.abs() > 1e-9) {
                return Err(invalid("revolve profile must lie in the XY plane"));
            }
            if points.iter().any(|p| p.x < -1e-9) {
                return Err(invalid("revolve profile must stay on the x >= 0 side of the axis"));
            }
            // Stand the profile up (its Y becomes Z), then spin it around Z.
            let upright: Vec<Edge> = loops
                .into_iter()
                .flatten()
                .map(|e| e.rotate(DVec3::ZERO, DVec3::X, std::f64::consts::FRAC_PI_2))
                .collect();
            let solid = Solid::revolve(&upright, DVec3::ZERO, DVec3::Z, degrees.to_radians())
                .map_err(occ("revolve"))?;
            Ok(vec![(solid, style)])
        }
        ModelSpec::Translate { offset, shape } => {
            let v = vec3(*offset);
            Ok(build_solids(shape, style, plates)?
                .into_iter()
                .map(|(s, st)| (s.translate(v), st))
                .collect())
        }
        ModelSpec::Rotate {
            axis,
            degrees,
            origin,
            shape,
        } => {
            let (axis, angle, origin) = rotation(*axis, *degrees, *origin)?;
            Ok(build_solids(shape, style, plates)?
                .into_iter()
                .map(|(s, st)| (s.rotate(origin, axis, angle), st))
                .collect())
        }
        ModelSpec::Group { children } => {
            if children.is_empty() {
                return Err(invalid("group needs at least one child"));
            }
            let mut bodies = Vec::new();
            for child in children {
                bodies.extend(build_solids(child, style, plates)?);
            }
            Ok(bodies)
        }
        ModelSpec::Color { rgb, shape } => build_solids(
            shape,
            &Style {
                color: *rgb,
                ..style.clone()
            },
            plates,
        ),
        ModelSpec::Part { name, shape } => build_solids(
            shape,
            &Style {
                part: Some(part_name(name)?),
                ..style.clone()
            },
            plates,
        ),
        ModelSpec::Union { children } | ModelSpec::Intersect { children } => {
            let op = if matches!(spec, ModelSpec::Union { .. }) { "union" } else { "intersect" };
            let mut operands = Vec::new();
            for child in children {
                operands.extend(build_solids(child, style, plates)?);
            }
            let Some((first, first_style)) = operands.first() else {
                return Err(invalid(format!("{op} needs at least one shape")));
            };
            let mut expr = Boolean::from(first);
            for (solid, _) in &operands[1..] {
                expr = if op == "union" { expr + solid } else { expr * solid };
            }
            let pieces = expr.build_vec().map_err(occ(op))?;
            if pieces.is_empty() {
                return Err(invalid(format!("{op} is empty: the shapes don't overlap")));
            }
            Ok(pieces.into_iter().map(|s| (s, first_style.clone())).collect())
        }
        ModelSpec::Difference { shape, tools } => {
            if tools.is_empty() {
                return Err(invalid("difference needs at least one tool"));
            }
            let mut cutters = Vec::new();
            for tool in tools {
                cutters.extend(build_solids(tool, style, plates)?);
            }
            let mut result = Vec::new();
            for (base, base_style) in build_solids(shape, style, plates)? {
                let mut expr = Boolean::from(&base);
                for (cutter, _) in &cutters {
                    expr = expr - cutter;
                }
                for piece in expr.build_vec().map_err(occ("difference"))? {
                    result.push((piece, base_style.clone()));
                }
            }
            Ok(result)
        }
        ModelSpec::Arrange { gap, children, bed } => {
            if children.is_empty() {
                return Err(invalid("arrange needs at least one child"));
            }
            if !(*gap >= 0.0 && gap.is_finite()) {
                return Err(invalid("arrange gap must be >= 0"));
            }
            if let Some(bed) = bed {
                if ![bed.width, bed.depth, bed.height].iter().all(|d| *d > 0.0 && d.is_finite()) {
                    return Err(invalid("bed width, depth and height must be positive"));
                }
            }
            let mut built = Vec::new();
            let mut footprints = Vec::new();
            for child in children {
                let bodies = build_solids(child, style, plates)?;
                let (min, max) = bounds(&bodies).ok_or_else(|| invalid("arrange child has no geometry"))?;
                footprints.push([max.x - min.x, max.y - min.y]);
                built.push((bodies, min));
            }
            let packing = pack(&footprints, *gap, bed.as_ref());
            plates.extend(packing.plates);
            let mut arranged = Vec::new();
            for ((bodies, min), [x, y]) in built.into_iter().zip(packing.positions) {
                let offset = DVec3::new(x - min.x, y - min.y, -min.z);
                arranged.extend(bodies.into_iter().map(|(s, st)| (s.translate(offset), st)));
            }
            Ok(arranged)
        }
    }
}

fn bounds(bodies: &Bodies) -> Option<(DVec3, DVec3)> {
    bodies
        .iter()
        .map(|(s, _)| s.bounding_box())
        .reduce(|[amin, amax], [bmin, bmax]| [amin.min(bmin), amax.max(bmax)])
        .map(|[min, max]| (min, max))
}

fn to_f32(v: DVec3) -> [f32; 3] {
    [v.x as f32, v.y as f32, v.z as f32]
}

/// Appends `solid`'s triangles (all painted `style.color`) and edges to
/// `scene`, recording its vertex and index ranges under its part name.
fn tessellate_into(scene: &mut TessellatedScene, solid: &Solid, style: &Style) -> Result<(), KernelError> {
    let mesh = Solid::mesh([solid], TESSELLATION).map_err(occ("meshing"))?;
    let vertex_start = scene.positions.len() as u32;
    let index_start = scene.indices.len() as u32;

    scene.positions.extend(mesh.vertices.iter().map(|v| to_f32(*v)));
    scene.normals.extend(mesh.normals.iter().map(|n| to_f32(*n)));
    scene.colors.extend(std::iter::repeat_n(style.color, mesh.vertices.len()));
    scene.indices.extend(mesh.indices.iter().map(|i| vertex_start + *i as u32));

    // Edge polylines, NaN-separated, into segment pairs.
    for polyline in mesh.edges.split(|p| p.is_nan()) {
        for pair in polyline.windows(2) {
            scene.edges.push(to_f32(pair[0]));
            scene.edges.push(to_f32(pair[1]));
        }
    }

    if let Some(name) = &style.part {
        let vertices = vertex_start..scene.positions.len() as u32;
        let indices = index_start..scene.indices.len() as u32;
        match scene.parts.last_mut() {
            Some(last)
                if &last.name == name && last.vertices.end == vertices.start && last.indices.end == indices.start =>
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_profiles_are_unsupported_for_now() {
        let err = OccKernel
            .build(&ModelSpec::Rect {
                width: 10.0,
                height: 10.0,
            })
            .unwrap_err();
        assert!(matches!(err, KernelError::Unsupported(_)), "{err}");
    }
}
