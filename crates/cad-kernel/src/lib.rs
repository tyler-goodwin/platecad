use cad_ir::{Bed, Bounds, EdgeSelector, MeshQuality, ModelSpec, Plate, TessellatedScene};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KernelError {
    #[error("{0}")]
    Message(String),
    #[error("invalid spec: {0}")]
    Invalid(String),
}

pub trait Kernel {
    fn name(&self) -> &'static str;

    /// Builds `spec` and meshes it at `quality`. Only the mesh depends on
    /// `quality`: edge selection and arrange layouts don't.
    fn build_with(&mut self, spec: &ModelSpec, quality: MeshQuality) -> Result<TessellatedScene, KernelError>;

    /// [`Kernel::build_with`] at [`MeshQuality::Fine`].
    fn build(&mut self, spec: &ModelSpec) -> Result<TessellatedScene, KernelError> {
        self.build_with(spec, MeshQuality::Fine)
    }

    /// The tight bounds of `spec`'s solids, as `arrange` measures them: within
    /// the fine chord tolerance of exact, and exact on flat faces. Solids it
    /// builds are cached for the next build, so measuring a shape a model
    /// then uses costs nothing extra.
    fn bounds(&mut self, spec: &ModelSpec) -> Result<Bounds, KernelError>;
}

/// What [`edge_selected`] needs to know about an edge.
pub struct EdgeShape<'a> {
    /// Points along the edge, start to end (at least the two endpoints).
    pub points: &'a [[f64; 3]],
    /// Unit tangents at the start and end.
    pub tangents: [[f64; 3]; 2],
}

/// Whether any of `selectors` (empty = all) picks `edge` of a solid whose
/// bounds are `min`..`max`.
pub fn edge_selected(selectors: &[EdgeSelector], edge: &EdgeShape, min: [f64; 3], max: [f64; 3]) -> bool {
    if selectors.is_empty() {
        return true;
    }
    let size = (0..3).map(|i| max[i] - min[i]).fold(0.0, f64::max);
    let tol = (size * 1e-7).max(1e-6);
    let (first, last) = (edge.points[0], edge.points[edge.points.len() - 1]);
    let chord: [f64; 3] = std::array::from_fn(|i| last[i] - first[i]);
    let chord_len = chord.iter().map(|c| c * c).sum::<f64>().sqrt();
    // Straight: tangents both run along the chord.
    let along = |t: [f64; 3]| (0..3).map(|i| t[i] * chord[i]).sum::<f64>().abs() / chord_len.max(1e-300);
    let straight = chord_len > tol && edge.tangents.iter().all(|t| (along(*t) - 1.0).abs() < 1e-6);
    let parallel = |axis: usize| straight && (chord[axis].abs() / chord_len - 1.0).abs() < 1e-6;
    let at = |axis: usize, value: f64| edge.points.iter().all(|p| (p[axis] - value).abs() <= tol);
    selectors.iter().any(|s| match s {
        EdgeSelector::All => true,
        EdgeSelector::ParallelX => parallel(0),
        EdgeSelector::ParallelY => parallel(1),
        EdgeSelector::ParallelZ => parallel(2),
        EdgeSelector::MaxX => at(0, max[0]),
        EdgeSelector::MinX => at(0, min[0]),
        EdgeSelector::MaxY => at(1, max[1]),
        EdgeSelector::MinY => at(1, min[1]),
        EdgeSelector::MaxZ => at(2, max[2]),
        EdgeSelector::MinZ => at(2, min[2]),
    })
}

// --- Shared by every kernel: arrange layout and profile validation. ---

/// Where `arrange` puts each child (the min corner of its XY footprint) and
/// which build plates that uses.
#[derive(Debug, Clone, PartialEq)]
pub struct Packing {
    pub positions: Vec<[f64; 2]>,
    pub plates: Vec<Plate>,
}

/// Tolerance so parts that exactly fill the bed still fit.
const FIT_EPSILON: f64 = 1e-6;

/// Next-fit row packing, keeping children in order. Rows wrap at the bed width
/// (or, without a bed, at a width that makes the block roughly square). With a
/// bed, a new plate starts when the next row would overrun the bed depth,
/// plates are laid out in a near-square grid, and each plate's block of parts
/// is centered on it, as a slicer would place them. Without a bed, the block is
/// centered on the origin. A part bigger than the bed still gets a spot (alone
/// at the start of a row or plate, overhanging evenly); callers warn about it.
pub fn pack(footprints: &[[f64; 2]], gap: f64, bed: Option<&Bed>) -> Packing {
    let wrap = match bed {
        Some(bed) => bed.width,
        None => {
            let area: f64 = footprints.iter().map(|[w, d]| (w + gap) * (d + gap)).sum();
            let widest = footprints.iter().map(|[w, _]| *w).fold(0.0, f64::max);
            widest.max(area.sqrt() * 1.3)
        }
    };

    // Plate-local placement.
    let mut local = Vec::with_capacity(footprints.len());
    let (mut plate, mut x, mut y, mut row_depth) = (0usize, 0.0, 0.0, 0.0f64);
    for &[w, d] in footprints {
        if x > 0.0 && x + w > wrap + FIT_EPSILON {
            x = 0.0;
            y += row_depth + gap;
            row_depth = 0.0;
        }
        if let Some(bed) = bed {
            if y > 0.0 && y + d > bed.depth + FIT_EPSILON {
                plate += 1;
                (x, y, row_depth) = (0.0, 0.0, 0.0);
            }
        }
        local.push((plate, x, y));
        x += w + gap;
        row_depth = row_depth.max(d);
    }

    // Extent of each plate's block of parts (blocks start at 0, 0).
    let count = plate + 1;
    let mut used = vec![[0.0f64; 2]; count];
    for (&(k, x, y), [w, d]) in local.iter().zip(footprints) {
        used[k] = [used[k][0].max(x + w), used[k][1].max(y + d)];
    }

    let Some(bed) = bed else {
        let [uw, ud] = used[0];
        return Packing {
            positions: local.into_iter().map(|(_, x, y)| [x - uw / 2.0, y - ud / 2.0]).collect(),
            plates: Vec::new(),
        };
    };

    // Plates in a grid. The step grows if an oversize block overhangs its plate,
    // so neighbouring plates' parts never overlap.
    let columns = (count as f64).sqrt().ceil() as usize;
    let widest = used.iter().map(|u| u[0]).fold(bed.width, f64::max);
    let deepest = used.iter().map(|u| u[1]).fold(bed.depth, f64::max);
    let plate_gap = (bed.width.min(bed.depth) * 0.1).max(gap);
    let origin = |k: usize| {
        [
            (k % columns) as f64 * (widest + plate_gap),
            (k / columns) as f64 * (deepest + plate_gap),
        ]
    };
    Packing {
        positions: local
            .into_iter()
            .map(|(k, x, y)| {
                let [ox, oy] = origin(k);
                let center = [(bed.width - used[k][0]) / 2.0, (bed.depth - used[k][1]) / 2.0];
                [ox + center[0] + x, oy + center[1] + y]
            })
            .collect(),
        plates: (0..count)
            .map(|k| {
                let [ox, oy] = origin(k);
                Plate {
                    min: [ox as f32, oy as f32],
                    size: [bed.width as f32, bed.depth as f32],
                }
            })
            .collect(),
    }
}

/// How far `2 * radius` may overshoot a rounded rect's shorter side and still
/// count as fully round, so `radius: width / 2` survives float rounding.
pub const ROUND_END_SLACK: f64 = 1e-9;

/// Checks the sizes of a primitive (`Sphere`, `Cylinder`, `Cone`, `Torus`,
/// `RoundedRect`); other nodes pass.
pub fn validate_primitive(spec: &ModelSpec) -> Result<(), KernelError> {
    let invalid = |why: String| Err(KernelError::Invalid(why));
    let positive = |what: &str, v: f64| if v > 0.0 && v.is_finite() { Ok(()) } else { invalid(format!("{what} must be positive")) };
    match *spec {
        ModelSpec::Sphere { radius } => positive("sphere radius", radius),
        ModelSpec::Cylinder { radius, height } => {
            positive("cylinder radius", radius)?;
            positive("cylinder height", height)
        }
        ModelSpec::Cone { radius, top, height } => {
            positive("cone height", height)?;
            if !(radius >= 0.0 && top >= 0.0 && radius.is_finite() && top.is_finite()) {
                return invalid("cone radii must be 0 or more".into());
            }
            if radius == 0.0 && top == 0.0 {
                return invalid("cone needs a radius above 0 at the base or the top".into());
            }
            Ok(())
        }
        ModelSpec::Torus { radius, tube } => {
            positive("torus tube", tube)?;
            if !(radius > tube && radius.is_finite()) {
                return invalid(format!("torus radius must be larger than its tube (got {radius} and {tube})"));
            }
            Ok(())
        }
        ModelSpec::RoundedRect { width, height, radius } => {
            positive("rounded rect width", width)?;
            positive("rounded rect height", height)?;
            positive("rounded rect radius", radius)?;
            if 2.0 * radius > width.min(height) + ROUND_END_SLACK {
                return invalid(format!(
                    "rounded rect radius must be at most half the shorter side (got {radius} for {width} x {height})"
                ));
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// A simple polygon's outline, counter-clockwise with no closing duplicate.
/// Rejects fewer than 3 points, non-finite or repeated points, zero area and
/// self-intersections.
pub fn validate_polygon(points: &[[f64; 2]]) -> Result<Vec<[f64; 2]>, KernelError> {
    let invalid = |why: &str| Err(KernelError::Invalid(format!("polygon {why}")));
    let mut pts = points.to_vec();
    if pts.len() > 1 && pts.first() == pts.last() {
        pts.pop();
    }
    if pts.len() < 3 {
        return invalid("needs at least 3 points");
    }
    if !pts.iter().flatten().all(|c| c.is_finite()) {
        return invalid("points must be finite");
    }
    let n = pts.len();
    for i in 0..n {
        let [a, b] = [pts[i], pts[(i + 1) % n]];
        if (a[0] - b[0]).hypot(a[1] - b[1]) < 1e-9 {
            return invalid("has repeated consecutive points");
        }
    }
    // Simple: no two non-adjacent edges touch.
    let cross = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0]);
    let touches = |p1: [f64; 2], p2: [f64; 2], q1: [f64; 2], q2: [f64; 2]| {
        let (d1, d2) = (cross(q1, q2, p1), cross(q1, q2, p2));
        let (d3, d4) = (cross(p1, p2, q1), cross(p1, p2, q2));
        let on = |o: [f64; 2], a: [f64; 2], p: [f64; 2], d: f64| {
            d.abs() < 1e-12
                && p[0] >= o[0].min(a[0]) - 1e-12
                && p[0] <= o[0].max(a[0]) + 1e-12
                && p[1] >= o[1].min(a[1]) - 1e-12
                && p[1] <= o[1].max(a[1]) + 1e-12
        };
        ((d1 > 0.0) != (d2 > 0.0) && (d3 > 0.0) != (d4 > 0.0) && d1 != 0.0 && d2 != 0.0 && d3 != 0.0 && d4 != 0.0)
            || on(q1, q2, p1, d1)
            || on(q1, q2, p2, d2)
            || on(p1, p2, q1, d3)
            || on(p1, p2, q2, d4)
    };
    for i in 0..n {
        for j in i + 1..n {
            let adjacent = j == i + 1 || (i == 0 && j == n - 1);
            if !adjacent && touches(pts[i], pts[(i + 1) % n], pts[j], pts[(j + 1) % n]) {
                return invalid("must not intersect itself");
            }
        }
    }
    // After the crossing check: a symmetric bow-tie's signed area cancels to 0.
    let area2: f64 = (0..n)
        .map(|i| {
            let [a, b] = [pts[i], pts[(i + 1) % n]];
            a[0] * b[1] - b[0] * a[1]
        })
        .sum();
    if area2.abs() < 1e-9 {
        return invalid("has zero area");
    }
    if area2 < 0.0 {
        pts.reverse();
    }
    Ok(pts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(a: [f64; 3], b: [f64; 3]) -> (Vec<[f64; 3]>, [[f64; 3]; 2]) {
        let d: [f64; 3] = std::array::from_fn(|i| b[i] - a[i]);
        let len = d.iter().map(|c| c * c).sum::<f64>().sqrt();
        let t = d.map(|c| c / len);
        (vec![a, b], [t, t])
    }

    fn picks(selectors: &[EdgeSelector], (points, tangents): &(Vec<[f64; 3]>, [[f64; 3]; 2])) -> bool {
        let edge = EdgeShape {
            points,
            tangents: *tangents,
        };
        edge_selected(selectors, &edge, [0.0; 3], [10.0; 3])
    }

    #[test]
    fn edge_selectors_on_a_cube() {
        use EdgeSelector::*;
        let vertical = line([10.0, 0.0, 0.0], [10.0, 0.0, 10.0]);
        let top_front = line([0.0, 0.0, 10.0], [10.0, 0.0, 10.0]);
        let bottom_left = line([0.0, 0.0, 0.0], [0.0, 10.0, 0.0]);

        assert!(picks(&[], &vertical) && picks(&[All], &bottom_left));
        assert!(picks(&[ParallelZ], &vertical) && !picks(&[ParallelZ], &top_front));
        assert!(picks(&[ParallelX], &top_front) && picks(&[ParallelY], &bottom_left));
        assert!(picks(&[MaxZ], &top_front) && !picks(&[MaxZ], &vertical));
        assert!(picks(&[MinZ], &bottom_left) && picks(&[MinX], &bottom_left));
        assert!(picks(&[MaxX], &vertical) && !picks(&[MaxX], &bottom_left));
        // Union.
        assert!(picks(&[MaxZ, ParallelZ], &vertical) && picks(&[MaxZ, ParallelZ], &top_front));
        assert!(!picks(&[MaxZ, ParallelZ], &bottom_left));
    }

    #[test]
    fn curved_edges_are_never_parallel_but_can_sit_at_an_extreme() {
        use EdgeSelector::*;
        // A circle of radius 5 on the top face, sampled.
        let points: Vec<[f64; 3]> = (0..=16)
            .map(|i| {
                let a = i as f64 / 16.0 * std::f64::consts::TAU;
                [5.0 + 5.0 * a.cos(), 5.0 + 5.0 * a.sin(), 10.0]
            })
            .collect();
        let circle = (points, [[0.0, 1.0, 0.0], [0.0, 1.0, 0.0]]);
        assert!(!picks(&[ParallelX], &circle) && !picks(&[ParallelY], &circle));
        assert!(picks(&[MaxZ], &circle) && !picks(&[MinZ], &circle));
    }

    fn bed(width: f64, depth: f64) -> Bed {
        Bed {
            width,
            depth,
            height: 250.0,
        }
    }

    fn assert_positions(got: &[[f64; 2]], want: &[[f64; 2]]) {
        assert_eq!(got.len(), want.len(), "{got:?}");
        for (g, w) in got.iter().zip(want) {
            assert!((g[0] - w[0]).abs() < 1e-9 && (g[1] - w[1]).abs() < 1e-9, "{got:?} != {want:?}");
        }
    }

    #[test]
    fn pack_without_bed_makes_a_roughly_square_block() {
        let packing = pack(&[[10.0, 10.0]; 4], 0.0, None);
        // 2 x 2, centered on the origin.
        assert_positions(&packing.positions, &[[-10.0, -10.0], [0.0, -10.0], [-10.0, 0.0], [0.0, 0.0]]);
        assert!(packing.plates.is_empty());
    }

    #[test]
    fn pack_wraps_rows_at_bed_width_and_fills_plates() {
        // 90 mm coasters, 10 mm gap, 256 mm bed: 2 per row, 2 rows per plate.
        // A full plate's 190 x 190 block sits 33 mm in from each edge; the
        // second plate's single row (190 x 90) is centered too.
        let packing = pack(&[[90.0, 90.0]; 6], 10.0, Some(&bed(256.0, 256.0)));
        let second_plate = 256.0 + 25.6;
        assert_positions(
            &packing.positions,
            &[
                [33.0, 33.0],
                [133.0, 33.0],
                [33.0, 133.0],
                [133.0, 133.0],
                [second_plate + 33.0, 83.0],
                [second_plate + 133.0, 83.0],
            ],
        );
        assert_eq!(packing.plates.len(), 2);
        assert_eq!(packing.plates[1].min, [second_plate as f32, 0.0]);
        assert_eq!(packing.plates[1].size, [256.0, 256.0]);
    }

    #[test]
    fn pack_exact_fit_shares_a_row() {
        let packing = pack(&[[123.0, 50.0], [123.0, 50.0]], 10.0, Some(&bed(256.0, 256.0)));
        assert_positions(&packing.positions, &[[0.0, 103.0], [133.0, 103.0]]);
        assert_eq!(packing.plates.len(), 1);
    }

    #[test]
    fn pack_centers_each_plates_parts_on_it() {
        let parts: Vec<[f64; 2]> =
            [[120.0, 40.0], [30.0, 200.0], [80.0, 80.0], [250.0, 20.0], [60.0, 60.0], [200.0, 100.0]].to_vec();
        let packing = pack(&parts, 5.0, Some(&bed(256.0, 256.0)));
        for plate in &packing.plates {
            let (px, py) = (plate.min[0] as f64, plate.min[1] as f64);
            let mut min = [f64::MAX; 2];
            let mut max = [f64::MIN; 2];
            for ([w, d], [x, y]) in parts.iter().zip(&packing.positions) {
                let on_plate = *x >= px - 1e-3 && *x < px + 256.0;
                if on_plate {
                    min = [min[0].min(*x), min[1].min(*y)];
                    max = [max[0].max(x + w), max[1].max(y + d)];
                }
            }
            let center = [(min[0] + max[0]) / 2.0, (min[1] + max[1]) / 2.0];
            assert!((center[0] - (px + 128.0)).abs() < 1e-3, "{center:?} on {plate:?}");
            assert!((center[1] - (py + 128.0)).abs() < 1e-3, "{center:?} on {plate:?}");
        }
    }

    #[test]
    fn pack_lays_many_plates_out_in_a_grid() {
        // One 200 mm square per 256 mm plate: 5 plates -> 3 columns, 2 rows.
        let packing = pack(&[[200.0, 200.0]; 5], 10.0, Some(&bed(256.0, 256.0)));
        let mins: Vec<[f32; 2]> = packing.plates.iter().map(|p| p.min).collect();
        let step = 256.0 + 25.6;
        assert_eq!(
            mins,
            [[0.0, 0.0], [step, 0.0], [2.0 * step, 0.0], [0.0, step], [step, step]]
        );
    }

    #[test]
    fn pack_gives_oversize_parts_their_own_spot_without_overlap() {
        let parts = [[300.0, 300.0], [300.0, 300.0], [50.0, 50.0]];
        let packing = pack(&parts, 10.0, Some(&bed(256.0, 256.0)));
        assert_eq!(packing.plates.len(), 3, "{packing:?}");
        assert_no_overlap(&parts, &packing.positions);
        // Plates are spaced by the oversize footprint, so they don't collide either.
        assert!(packing.plates[1].min[0] >= 300.0);
    }

    fn assert_no_overlap(parts: &[[f64; 2]], positions: &[[f64; 2]]) {
        for i in 0..parts.len() {
            for j in i + 1..parts.len() {
                let (a, b) = (positions[i], positions[j]);
                let apart = a[0] + parts[i][0] <= b[0] + 1e-9
                    || b[0] + parts[j][0] <= a[0] + 1e-9
                    || a[1] + parts[i][1] <= b[1] + 1e-9
                    || b[1] + parts[j][1] <= a[1] + 1e-9;
                assert!(apart, "parts {i} and {j} overlap: {positions:?}");
            }
        }
    }

    #[test]
    fn pack_keeps_fitting_parts_on_their_plate_without_overlap() {
        let parts: Vec<[f64; 2]> = [
            [120.0, 40.0], [30.0, 200.0], [80.0, 80.0], [250.0, 20.0], [10.0, 10.0],
            [100.0, 150.0], [60.0, 60.0], [200.0, 100.0], [45.0, 45.0], [256.0, 256.0],
        ]
        .to_vec();
        let bed = bed(256.0, 256.0);
        let packing = pack(&parts, 5.0, Some(&bed));
        assert_no_overlap(&parts, &packing.positions);
        for ([w, d], [x, y]) in parts.iter().zip(&packing.positions) {
            // Plates are f32 (281.6 becomes 281.6000061), so allow f32 rounding.
            let on_a_plate = packing.plates.iter().any(|p| {
                let (px, py) = (p.min[0] as f64, p.min[1] as f64);
                *x >= px - 1e-3
                    && *y >= py - 1e-3
                    && x + w <= px + bed.width + 1e-3
                    && y + d <= py + bed.depth + 1e-3
            });
            assert!(on_a_plate, "{w}x{d} at {x},{y} not inside any plate");
        }
    }

    #[test]
    fn primitives_need_sensible_sizes() {
        let ok = [
            ModelSpec::Sphere { radius: 1.0 },
            ModelSpec::Cylinder { radius: 1.0, height: 2.0 },
            ModelSpec::Cone { radius: 1.0, top: 0.0, height: 2.0 },
            ModelSpec::Cone { radius: 0.0, top: 1.0, height: 2.0 },
            ModelSpec::Torus { radius: 3.0, tube: 1.0 },
            ModelSpec::RoundedRect { width: 4.0, height: 2.0, radius: 1.0 },
            ModelSpec::RoundedRect { width: 0.3, height: 0.3, radius: 0.15 },
        ];
        for spec in &ok {
            assert!(validate_primitive(spec).is_ok(), "{spec:?}");
        }
        let bad = [
            (ModelSpec::Sphere { radius: 0.0 }, "sphere radius must be positive"),
            (ModelSpec::Cylinder { radius: 1.0, height: -1.0 }, "cylinder height must be positive"),
            (ModelSpec::Cone { radius: -1.0, top: 1.0, height: 1.0 }, "cone radii must be 0 or more"),
            (ModelSpec::Cone { radius: 0.0, top: 0.0, height: 1.0 }, "cone needs a radius"),
            (ModelSpec::Torus { radius: 1.0, tube: 1.0 }, "torus radius must be larger than its tube"),
            (ModelSpec::Sphere { radius: f64::NAN }, "sphere radius must be positive"),
            (ModelSpec::RoundedRect { width: 4.0, height: 2.0, radius: 0.0 }, "rounded rect radius must be positive"),
            (ModelSpec::RoundedRect { width: 4.0, height: 2.0, radius: 1.1 }, "at most half the shorter side"),
        ];
        for (spec, why) in &bad {
            let err = validate_primitive(spec).unwrap_err().to_string();
            assert!(err.contains(why), "{spec:?}: {err}");
        }
    }
}
