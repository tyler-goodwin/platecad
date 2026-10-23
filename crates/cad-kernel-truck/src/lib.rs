use std::collections::{HashMap, HashSet};

use cad_ir::{DEFAULT_COLOR, ModelSpec, Rgb, TessellatedScene};
use cad_kernel::{Kernel, KernelError};
use truck_meshalgo::tessellation::{MeshableShape, MeshedShape};
use truck_modeling::{Face, Point3, Shell, Vector3, builder};

const TESSELLATION_TOL: f64 = 0.01;

#[derive(Debug, Default)]
pub struct TruckKernel;

impl Kernel for TruckKernel {
    fn name(&self) -> &'static str {
        "truck"
    }

    fn build(&mut self, spec: &ModelSpec) -> Result<TessellatedScene, KernelError> {
        let mut scene = TessellatedScene::default();
        for (shell, color) in build_shells(spec, DEFAULT_COLOR)? {
            tessellate_into(&mut scene, &shell, color);
        }
        Ok(scene)
    }
}

/// Boundaries of the B-rep: a single-face shell per 2D spec, the closed skin per
/// solid. Each comes with the color of the innermost `Color` node above it
/// (`color` is the one inherited from the parents).
fn build_shells(spec: &ModelSpec, color: Rgb) -> Result<Vec<(Shell, Rgb)>, KernelError> {
    match spec {
        ModelSpec::Rect { .. } => Ok(vec![(Shell::from(vec![build_face(spec)?]), color)]),
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
            Ok(vec![(shell, profile_color(profile).unwrap_or(color))])
        }
        ModelSpec::Translate { offset, shape } => Ok(build_shells(shape, color)?
            .into_iter()
            .map(|(shell, color)| (builder::translated(&shell, to_vector(*offset)), color))
            .collect()),
        ModelSpec::Group { children } => {
            if children.is_empty() {
                return Err(KernelError::Invalid("group needs at least one child".into()));
            }
            let mut shells = Vec::new();
            for child in children {
                shells.extend(build_shells(child, color)?);
            }
            Ok(shells)
        }
        ModelSpec::Color { rgb, shape } => build_shells(shape, *rgb),
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
                let shells = build_shells(child, color)?;
                let (min, max) = shells_bounds(&shells)
                    .ok_or_else(|| KernelError::Invalid("arrange child has no geometry".into()))?;
                let offset = Vector3::new(x - min.x, -min.y, -min.z);
                arranged.extend(
                    shells
                        .into_iter()
                        .map(|(shell, color)| (builder::translated(&shell, offset), color)),
                );
                x += (max.x - min.x) + gap;
            }
            Ok(arranged)
        }
    }
}

/// Bounding box of the shells' B-rep vertices. Exact for the planar faces we
/// build today; curved faces would need their surfaces sampled.
fn shells_bounds(shells: &[(Shell, Rgb)]) -> Option<(Point3, Point3)> {
    let mut points = shells
        .iter()
        .flat_map(|(shell, _)| shell.vertex_iter())
        .map(|v| v.point());
    let first = points.next()?;
    Some(points.fold((first, first), |(min, max), p| {
        (
            Point3::new(min.x.min(p.x), min.y.min(p.y), min.z.min(p.z)),
            Point3::new(max.x.max(p.x), max.y.max(p.y), max.z.max(p.z)),
        )
    }))
}

/// Innermost color set inside a profile, which its extrusion inherits.
fn profile_color(spec: &ModelSpec) -> Option<Rgb> {
    match spec {
        ModelSpec::Color { rgb, shape } => profile_color(shape).or(Some(*rgb)),
        ModelSpec::Translate { shape, .. } => profile_color(shape),
        _ => None,
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
        // Color is applied by `build_shells` via `profile_color`.
        ModelSpec::Color { shape, .. } => build_face(shape),
        ModelSpec::Extrude { .. } | ModelSpec::Group { .. } | ModelSpec::Arrange { .. } => {
            Err(KernelError::Invalid(
                "extrude profile must be a single 2D shape (rect, optionally translated or colored)"
                    .into(),
            ))
        }
    }
}

fn to_vector([x, y, z]: [f64; 3]) -> Vector3 {
    Vector3::new(x, y, z)
}

/// Appends `shell`'s triangles (all painted `color`) and edges to `scene`.
fn tessellate_into(scene: &mut TessellatedScene, shell: &Shell, color: Rgb) {
    let mesh = shell.triangulation(TESSELLATION_TOL).to_polygon();
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
                scene.colors.push(color);
                (scene.positions.len() - 1) as u32
            });
            scene.indices.push(index);
        }
    }

    let mut seen = HashSet::new();
    for edge in shell.edge_iter() {
        if seen.insert(edge.id()) {
            let a = edge.front().point();
            let b = edge.back().point();
            scene.edges.push(to_f32(a.x, a.y, a.z));
            scene.edges.push(to_f32(b.x, b.y, b.z));
        }
    }
}

fn flat_normal([a, b, c]: [Point3; 3]) -> Vector3 {
    use truck_modeling::InnerSpace;
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
