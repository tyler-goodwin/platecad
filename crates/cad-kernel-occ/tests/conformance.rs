//! The OCC and truck kernels must agree on everything truck can build, and
//! OCC must handle the lined-up-faces cases truck can't.

use cad_export::objects;
use cad_ir::{Bed, EdgeSelector, ModelSpec, Rgb, TessellatedScene};
use cad_kernel::Kernel;
use cad_kernel_occ::OccKernel;
use cad_kernel_truck::TruckKernel;

fn rect(width: f64, height: f64) -> ModelSpec {
    ModelSpec::Rect { width, height }
}

fn circle(radius: f64) -> ModelSpec {
    ModelSpec::Circle { radius }
}

fn polygon(points: &[[f64; 2]]) -> ModelSpec {
    ModelSpec::Polygon {
        points: points.to_vec(),
    }
}

fn extrude(profile: ModelSpec, distance: f64) -> ModelSpec {
    ModelSpec::Extrude {
        profile: Box::new(profile),
        distance,
        direction: None,
    }
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

fn translate(offset: [f64; 3], shape: ModelSpec) -> ModelSpec {
    ModelSpec::Translate {
        offset,
        shape: Box::new(shape),
    }
}

fn rotate(axis: [f64; 3], degrees: f64, shape: ModelSpec) -> ModelSpec {
    ModelSpec::Rotate {
        axis,
        degrees,
        origin: [0.0; 3],
        shape: Box::new(shape),
    }
}

fn cuboid(min: [f64; 3], size: [f64; 3]) -> ModelSpec {
    translate(min, extrude(rect(size[0], size[1]), size[2]))
}

fn part(name: &str, shape: ModelSpec) -> ModelSpec {
    ModelSpec::Part {
        name: name.into(),
        shape: Box::new(shape),
    }
}

fn paint(rgb: Rgb, shape: ModelSpec) -> ModelSpec {
    ModelSpec::Color {
        rgb,
        shape: Box::new(shape),
    }
}

fn cutout(outer: ModelSpec, holes: Vec<ModelSpec>) -> ModelSpec {
    ModelSpec::Cutout {
        outer: Box::new(outer),
        holes,
    }
}

/// Bounds, plus per-part (name, closed?, volume), colors and plate count.
struct Summary {
    min: [f32; 3],
    max: [f32; 3],
    parts: Vec<(String, bool, f64)>,
    colors: Vec<Rgb>,
    plates: usize,
}

fn summarize(scene: &TessellatedScene) -> Summary {
    let aabb = scene.aabb().expect("geometry");
    let parts = objects(scene)
        .iter()
        .map(|o| (o.name.clone(), o.is_closed(), o.signed_volume()))
        .collect();
    let mut colors = scene.colors.clone();
    colors.sort();
    colors.dedup();
    Summary {
        min: aabb.min,
        max: aabb.max,
        parts,
        colors,
        plates: scene.plates.len(),
    }
}

fn cases() -> Vec<(&'static str, ModelSpec)> {
    let ring = || translate([5.0, 0.0, 0.0], rect(5.0, 20.0));
    let ten = || cuboid([0.0; 3], [10.0; 3]);
    let other = || cuboid([5.0, 5.5, 6.0], [10.0; 3]);
    vec![
        ("box", part("b", cuboid([1.0, 2.0, 3.0], [10.0, 20.0, 5.0]))),
        ("rotated box", part("b", rotate([0.0, 0.0, 1.0], 30.0, cuboid([0.0; 3], [10.0, 4.0, 2.0])))),
        ("cylinder", part("c", extrude(circle(10.0), 5.0))),
        ("washer", part("w", extrude(cutout(circle(10.0), vec![circle(4.0)]), 2.0))),
        (
            "L prism",
            part(
                "l",
                extrude(polygon(&[[0.0, 0.0], [4.0, 0.0], [4.0, 1.0], [1.0, 1.0], [1.0, 3.0], [0.0, 3.0]]), 2.0),
            ),
        ),
        ("slanted", part("s", extrude_along(rect(2.0, 3.0), 4.0, [0.0, 3.0, 4.0]))),
        ("tube", part("t", revolve(ring(), 360.0))),
        ("quarter tube", part("q", revolve(ring(), 90.0))),
        ("torus", part("o", revolve(translate([20.0, 0.0, 0.0], circle(5.0)), 360.0))),
        ("crossing union", part("u", ModelSpec::Union { children: vec![ten(), other()] })),
        (
            "through hole",
            part(
                "h",
                ModelSpec::Difference {
                    shape: Box::new(ten()),
                    tools: vec![cuboid([3.0, 3.0, -1.0], [4.0, 4.0, 12.0])],
                },
            ),
        ),
        (
            "round hole",
            part(
                "r",
                ModelSpec::Difference {
                    shape: Box::new(cuboid([0.0; 3], [20.0, 20.0, 4.0])),
                    tools: vec![translate([10.0, 10.0, -1.0], extrude(circle(3.0), 6.0))],
                },
            ),
        ),
        ("intersection", part("i", ModelSpec::Intersect { children: vec![ten(), other()] })),
        (
            "colored parts on plates",
            ModelSpec::Arrange {
                gap: 10.0,
                bed: Some(Bed {
                    width: 256.0,
                    depth: 256.0,
                    height: 256.0,
                }),
                children: (0..5u8)
                    .map(|i| paint([i * 40, 0, 0], part(&format!("p{i}"), extrude(rect(90.0, 90.0), 4.0))))
                    .collect(),
            },
        ),
    ]
}

#[test]
fn occ_matches_truck() {
    for (name, spec) in cases() {
        let truck = summarize(&TruckKernel.build(&spec).unwrap_or_else(|e| panic!("{name}: truck: {e}")));
        let occ = summarize(&OccKernel::default().build(&spec).unwrap_or_else(|e| panic!("{name}: occ: {e}")));

        for i in 0..3 {
            assert!((truck.min[i] - occ.min[i]).abs() < 0.05, "{name}: min {:?} vs {:?}", truck.min, occ.min);
            assert!((truck.max[i] - occ.max[i]).abs() < 0.05, "{name}: max {:?} vs {:?}", truck.max, occ.max);
        }
        assert_eq!(truck.colors, occ.colors, "{name}: colors");
        assert_eq!(truck.plates, occ.plates, "{name}: plates");
        let names = |s: &Summary| s.parts.iter().map(|p| p.0.clone()).collect::<Vec<_>>();
        assert_eq!(names(&truck), names(&occ), "{name}: parts");
        for ((part, t_closed, t_vol), (_, o_closed, o_vol)) in truck.parts.iter().zip(&occ.parts) {
            assert!(*t_closed && *o_closed, "{name}/{part}: closed truck={t_closed} occ={o_closed}");
            assert!(*o_vol > 0.0, "{name}/{part}: OCC solid inside-out ({o_vol})");
            assert!(
                (t_vol - o_vol).abs() < t_vol.abs() * 0.005,
                "{name}/{part}: volume truck {t_vol} vs occ {o_vol}"
            );
        }
    }
}

/// Exactly what truck-shapeops can't do (its kernel tests pin those failures).
#[test]
fn occ_handles_lined_up_faces() {
    let floor = || cuboid([0.0; 3], [20.0, 20.0, 2.0]);
    let cases = [
        (
            "shared bottom plane",
            ModelSpec::Union {
                children: vec![floor(), cuboid([0.0; 3], [3.0, 20.0, 15.0])],
            },
            800.0 + 3.0 * 20.0 * 13.0,
        ),
        (
            "touching faces",
            ModelSpec::Union {
                children: vec![floor(), cuboid([0.0, 0.0, 2.0], [3.0, 20.0, 15.0])],
            },
            800.0 + 900.0,
        ),
        (
            "flush through-hole",
            ModelSpec::Difference {
                shape: Box::new(cuboid([0.0; 3], [10.0; 3])),
                tools: vec![cuboid([3.0, 3.0, 0.0], [4.0, 4.0, 10.0])],
            },
            840.0,
        ),
    ];
    for (name, spec, volume) in cases {
        let scene = OccKernel::default().build(&part("x", spec)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let object = &objects(&scene)[0];
        assert!(object.is_closed(), "{name}: not closed");
        assert!(
            (object.signed_volume() - volume).abs() < 1e-3,
            "{name}: {} vs {volume}",
            object.signed_volume()
        );
    }
}

#[test]
fn occ_validates_like_truck() {
    let bad = [
        extrude(rect(0.0, 1.0), 1.0),
        extrude(rect(1.0, 1.0), 0.0),
        extrude_along(rect(1.0, 1.0), 1.0, [1.0, 0.0, 0.0]),
        extrude(polygon(&[[0.0, 0.0], [2.0, 2.0], [2.0, 0.0], [0.0, 2.0]]), 1.0),
        revolve(translate([-1.0, 0.0, 0.0], rect(2.0, 1.0)), 360.0),
        revolve(translate([1.0, 0.0, 0.0], rect(1.0, 1.0)), 0.0),
        ModelSpec::Difference {
            shape: Box::new(cuboid([0.0; 3], [1.0; 3])),
            tools: vec![],
        },
    ];
    for spec in bad {
        assert!(TruckKernel.build(&spec).is_err(), "truck accepted {spec:?}");
        assert!(OccKernel::default().build(&spec).is_err(), "occ accepted {spec:?}");
    }
}

fn fillet(shape: ModelSpec, radius: f64, edges: Vec<EdgeSelector>) -> ModelSpec {
    ModelSpec::Fillet {
        shape: Box::new(shape),
        radius,
        edges,
    }
}

fn chamfer(shape: ModelSpec, distance: f64, edges: Vec<EdgeSelector>) -> ModelSpec {
    ModelSpec::Chamfer {
        shape: Box::new(shape),
        distance,
        edges,
    }
}

/// Closed, and the exported volume within 0.5% of `volume`.
fn assert_solid(name: &str, spec: ModelSpec, volume: f64) -> TessellatedScene {
    let scene = OccKernel::default().build(&part("x", spec)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let object = &objects(&scene)[0];
    assert!(object.is_closed(), "{name}: not closed");
    let got = object.signed_volume();
    assert!((got - volume).abs() < volume * 0.005, "{name}: {got} vs {volume}");
    scene
}

#[test]
fn fillets_and_chamfers_remove_the_right_volume() {
    use std::f64::consts::PI;
    use EdgeSelector::*;
    let cube = || cuboid([0.0; 3], [10.0; 3]);
    // Vertical edges rounded r = 2: each removes (r^2 - pi r^2 / 4) x height.
    assert_solid("fillet |Z", fillet(cube(), 2.0, vec![ParallelZ]), 1000.0 - 4.0 * (4.0 - PI) * 10.0);
    // Vertical edges beveled d = 1: each removes a d^2 / 2 prism.
    assert_solid("chamfer |Z", chamfer(cube(), 1.0, vec![ParallelZ]), 1000.0 - 4.0 * 0.5 * 10.0);
    // Every edge rounded r = 1: 12 edge strips plus 8 sphere-octant corners.
    let all = 1000.0 - 12.0 * 8.0 * (1.0 - PI / 4.0) - 8.0 * (1.0 - PI / 6.0);
    assert_solid("fillet all", fillet(cube(), 1.0, vec![]), all);
}

#[test]
fn selectors_keep_unselected_faces_intact() {
    use EdgeSelector::*;
    // Only the top edges: the bottom face stays a full 10 x 10 square.
    let scene = OccKernel::default()
        .build(&part("x", fillet(cuboid([0.0; 3], [10.0; 3]), 1.0, vec![MaxZ])))
        .unwrap();
    let on_bottom: Vec<_> = scene.positions.iter().filter(|p| p[2].abs() < 1e-4).collect();
    for corner in [[0.0, 0.0], [10.0, 0.0], [0.0, 10.0], [10.0, 10.0]] {
        assert!(
            on_bottom.iter().any(|p| (p[0] - corner[0]).abs() < 1e-4 && (p[1] - corner[1]).abs() < 1e-4),
            "bottom corner {corner:?} was rounded"
        );
    }
    // ...while the top corners are gone.
    assert!(!scene.positions.iter().any(|p| (p[0] - 10.0).abs() < 1e-4 && (p[1] - 10.0).abs() < 1e-4 && (p[2] - 10.0).abs() < 1e-4));
}

#[test]
fn chamfer_on_a_curved_edge() {
    use std::f64::consts::PI;
    // First-layer chamfer on a cylinder: removes a ring of triangular section
    // (Pappus: area d^2/2 at radius r - d/3).
    let (r, d) = (10.0, 1.0);
    let ring = 0.5 * d * d * 2.0 * PI * (r - d / 3.0);
    assert_solid(
        "chamfer <Z cylinder",
        chamfer(extrude(circle(r), 5.0), d, vec![EdgeSelector::MinZ]),
        PI * r * r * 5.0 - ring,
    );
}

#[test]
fn fillet_errors_explain() {
    use EdgeSelector::*;
    // No edge of a box turned 45 degrees is parallel to X.
    let turned = rotate([0.0, 0.0, 1.0], 45.0, cuboid([0.0; 3], [10.0; 3]));
    let err = OccKernel::default().build(&fillet(turned, 1.0, vec![ParallelX])).unwrap_err().to_string();
    assert!(err.contains("no edges match"), "{err}");
    // A radius bigger than the part.
    let err = OccKernel::default()
        .build(&fillet(cuboid([0.0; 3], [2.0; 3]), 5.0, vec![]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("too large"), "{err}");
    assert!(OccKernel::default().build(&fillet(cuboid([0.0; 3], [2.0; 3]), 0.0, vec![])).is_err());
}
