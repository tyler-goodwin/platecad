use std::collections::{HashMap, HashSet};

use cad_ir::{DEFAULT_COLOR, ModelSpec, Rgb, ScenePart, TessellatedScene};
use cad_kernel::{Kernel, KernelError};
use truck_meshalgo::tessellation::{MeshableShape, MeshedShape};
use truck_modeling::{Face, InnerSpace, Point3, Rad, Shell, Vector3, builder};

const TESSELLATION_TOL: f64 = 0.01;

#[derive(Debug, Default)]
pub struct TruckKernel;

impl Kernel for TruckKernel {
    fn name(&self) -> &'static str {
        "truck"
    }

    fn build(&mut self, spec: &ModelSpec) -> Result<TessellatedScene, KernelError> {
        let mut scene = TessellatedScene::default();
        for body in build_bodies(spec, &Style::default())? {
            tessellate_into(&mut scene, &body);
        }
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

fn build_bodies(spec: &ModelSpec, style: &Style) -> Result<Vec<Body>, KernelError> {
    match spec {
        ModelSpec::Rect { .. } => Ok(vec![Body {
            shell: Shell::from(vec![build_face(spec)?]),
            style: style.clone(),
        }]),
        ModelSpec::Extrude { profile, distance } => {
            if !(*distance > 0.0) {
                return Err(KernelError::Invalid(
                    "extrude distance must be positive".into(),
                ));
            }
            let face = build_face(profile)?;
            let solid = builder::tsweep(&face, Vector3::new(0.0, 0.0, *distance));
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
        ModelSpec::Translate { offset, shape } => Ok(build_bodies(shape, style)?
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
            Ok(build_bodies(shape, style)?
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
                bodies.extend(build_bodies(child, style)?);
            }
            Ok(bodies)
        }
        ModelSpec::Color { rgb, shape } => build_bodies(
            shape,
            &Style {
                color: *rgb,
                ..style.clone()
            },
        ),
        ModelSpec::Part { name, shape } => build_bodies(
            shape,
            &Style {
                part: Some(validate_part_name(name)?),
                ..style.clone()
            },
        ),
        ModelSpec::Arrange { gap, children } => {
            if children.is_empty() {
                return Err(KernelError::Invalid("arrange needs at least one child".into()));
            }
            if !(*gap >= 0.0) {
                return Err(KernelError::Invalid("arrange gap must be >= 0".into()));
            }
            let mut arranged = Vec::new();
            let mut x = 0.0;
            for child in children {
                let bodies = build_bodies(child, style)?;
                let (min, max) = bodies_bounds(&bodies)
                    .ok_or_else(|| KernelError::Invalid("arrange child has no geometry".into()))?;
                let offset = Vector3::new(x - min.x, -min.y, -min.z);
                arranged.extend(
                    bodies
                        .into_iter()
                        .map(|body| body.map(|shell| builder::translated(shell, offset))),
                );
                x += (max.x - min.x) + gap;
            }
            Ok(arranged)
        }
    }
}

fn validate_part_name(name: &str) -> Result<String, KernelError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(KernelError::Invalid("part name must not be empty".into()));
    }
    Ok(trimmed.to_string())
}

/// Bounding box of the bodies' B-rep vertices. Exact for the planar faces we
/// build today; curved faces would need their surfaces sampled.
fn bodies_bounds(bodies: &[Body]) -> Option<(Point3, Point3)> {
    let mut points = bodies
        .iter()
        .flat_map(|body| body.shell.vertex_iter())
        .map(|v| v.point());
    let first = points.next()?;
    Some(points.fold((first, first), |(min, max), p| {
        (
            Point3::new(min.x.min(p.x), min.y.min(p.y), min.z.min(p.z)),
            Point3::new(max.x.max(p.x), max.y.max(p.y), max.z.max(p.z)),
        )
    }))
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
        ModelSpec::Extrude { .. } | ModelSpec::Group { .. } | ModelSpec::Arrange { .. } => {
            Err(KernelError::Invalid(
                "extrude profile must be a single 2D shape (rect, optionally moved, colored or named)"
                    .into(),
            ))
        }
    }
}

fn to_vector([x, y, z]: [f64; 3]) -> Vector3 {
    Vector3::new(x, y, z)
}

/// Appends `body`'s triangles and edges to `scene`, and records its vertex and
/// index ranges under its part name. A body continuing the previous body's
/// part extends that range instead of starting a new one.
fn tessellate_into(scene: &mut TessellatedScene, body: &Body) {
    let vertex_start = scene.positions.len() as u32;
    let index_start = scene.indices.len() as u32;

    let mesh = body.shell.triangulation(TESSELLATION_TOL).to_polygon();
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
            let a = edge.front().point();
            let b = edge.back().point();
            scene.edges.push(to_f32(a.x, a.y, a.z));
            scene.edges.push(to_f32(b.x, b.y, b.z));
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

    fn rect(width: f64, height: f64) -> ModelSpec {
        ModelSpec::Rect { width, height }
    }

    fn extrude(profile: ModelSpec, distance: f64) -> ModelSpec {
        ModelSpec::Extrude {
            profile: Box::new(profile),
            distance,
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
        ModelSpec::Arrange { gap, children }
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
        let spec = arrange(
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

        let expected = [
            ([0.0, 0.0, 0.0], [10.0, 20.0, 2.0]),
            ([15.0, 0.0, 0.0], [45.0, 5.0, 4.0]),
            ([50.0, 0.0, 0.0], [51.0, 1.0, 1.0]),
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
    fn arrange_moves_groups_as_a_unit() {
        let pair = ModelSpec::Group {
            children: vec![unit_box(), translate([0.0, 0.0, 5.0], unit_box())],
        };
        let scene = TruckKernel
            .build(&arrange(0.0, vec![translate([9.0, 9.0, 9.0], pair)]))
            .unwrap();
        // Stack keeps its internal spacing; only the whole thing moved to the plate.
        assert_aabb(&scene, [0.0; 3], [1.0, 1.0, 6.0]);
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
        assert_aabb(&scene, [0.0; 3], [20.0, 10.0, 1.0]);
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
