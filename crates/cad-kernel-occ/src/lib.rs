//! The geometry kernel: OpenCascade, through cadrum (statically linked OCCT 8).
//!
//! It builds solids only. A bare 2D profile (e.g. `models/square.ts`) is
//! shown as a plate `FLAT_THICKNESS` thick.

use cad_ir::{Bounds, DEFAULT_COLOR, MeshQuality, ModelSpec, Plate, Rgb, ScenePart, TessellatedScene};
use cad_kernel::{
    EdgeShape, Kernel, KernelError, ROUND_END_SLACK, edge_selected, pack, validate_polygon, validate_primitive,
};
use cadrum::{Boolean, DVec3, Edge, Solid, Tessellation};

/// Edge sampling for selection and bounds, at the finest quality whatever
/// the mesh quality, so `fillet(..., { edges })` picks the same edges.
const TESSELLATION: Tessellation = tessellation(MeshQuality::Fine);

/// Thickness of a bare profile shown on its own, mm: thin enough to read as
/// flat, thick enough to mesh.
pub const FLAT_THICKNESS: f64 = 0.01;

/// Absolute chord and angular tolerances for `quality`.
const fn tessellation(quality: MeshQuality) -> Tessellation {
    Tessellation {
        deflection_linear: quality.chord_tolerance(),
        deflection_angular: quality.angular_tolerance(),
        relative_linear: false,
    }
}

#[derive(Debug, Default)]
pub struct OccKernel {
    cache: SolidCache,
}

impl OccKernel {
    /// Booleans, fillets and chamfers answered from the cache so far.
    pub fn cache_hits(&self) -> usize {
        self.cache.hits
    }
}

impl Kernel for OccKernel {
    fn name(&self) -> &'static str {
        "occ"
    }

    fn build_with(&mut self, spec: &ModelSpec, quality: MeshQuality) -> Result<TessellatedScene, KernelError> {
        let mut build = Build {
            plates: Vec::new(),
            cache: &mut self.cache,
        };
        let bodies = build_solids(spec, &Style::default(), &mut build);
        let plates = build.plates;
        self.cache.end_build();
        let mut scene = TessellatedScene::default();
        for (solid, style) in &bodies? {
            tessellate_into(&mut scene, solid, style, tessellation(quality))?;
        }
        scene.plates = plates;
        Ok(scene)
    }

    fn bounds(&mut self, spec: &ModelSpec) -> Result<Bounds, KernelError> {
        // No `end_build`: queries run during a script's eval, and what they
        // build should stay warm for the build that follows it.
        let mut build = Build {
            plates: Vec::new(),
            cache: &mut self.cache,
        };
        let bodies = build_solids(spec, &Style::default(), &mut build)?;
        let (min, max) = self
            .cache
            .bounds(SolidCache::any_key(spec), &bodies)?
            .ok_or_else(|| invalid("bounds of a shape with no solids"))?;
        Ok(Bounds {
            min: min.to_array(),
            max: max.to_array(),
        })
    }
}

/// How many builds an unused cache entry survives. A reload builds two views
/// (assembled and print), so entries outlive one edit that skips them.
const CACHE_BUILDS: u64 = 4;

/// Results of the slow operations (booleans, fillets, chamfers), keyed by
/// the subtree alone and stored unstyled (see [`Style::under`]). They're reused within a build,
/// e.g. a print view that repeats the assembled parts, and across builds,
/// so a reload only redoes the parts whose specs changed. Solids don't
/// depend on mesh quality, so a quality change reuses them too. The tight
/// bounds of `arrange` children are kept the same way, since each costs a
/// fine mesh.
#[derive(Debug, Default)]
struct SolidCache {
    entries: std::collections::HashMap<String, CacheEntry<Bodies>>,
    bounds: std::collections::HashMap<String, CacheEntry<(DVec3, DVec3)>>,
    build: u64,
    hits: usize,
}

#[derive(Debug)]
struct CacheEntry<T> {
    value: T,
    last_used: u64,
}

impl SolidCache {
    /// The cache key for `spec`, or `None` if it isn't worth caching or
    /// can't be: an `arrange` inside records plates as it builds, which a
    /// cache hit would skip. (Matching the debug text is crude but errs
    /// safe: a part named "Arrange {" just goes uncached.)
    fn key(spec: &ModelSpec) -> Option<String> {
        let slow = matches!(
            spec,
            ModelSpec::Union { .. }
                | ModelSpec::Intersect { .. }
                | ModelSpec::Difference { .. }
                | ModelSpec::Fillet { .. }
                | ModelSpec::Chamfer { .. }
        );
        if slow { Self::any_key(spec) } else { None }
    }

    /// The key for any subtree, or `None` if it holds an `arrange` (see `key`).
    fn any_key(spec: &ModelSpec) -> Option<String> {
        let key = format!("{spec:?}");
        (!key.contains("Arrange {")).then_some(key)
    }

    /// A copy of the cached bodies (solids are deep-copied: cheap next to a boolean).
    fn get(&mut self, key: &str) -> Option<Bodies> {
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.build;
        self.hits += 1;
        Some(entry.value.clone())
    }

    fn insert(&mut self, key: String, bodies: Bodies) {
        let last_used = self.build;
        self.entries.insert(key, CacheEntry { value: bodies, last_used });
    }

    /// `bodies`' tight bounds, from the cache under `key` when there is one.
    fn bounds(&mut self, key: Option<String>, bodies: &Bodies) -> Result<Option<(DVec3, DVec3)>, KernelError> {
        if let Some(entry) = key.as_ref().and_then(|k| self.bounds.get_mut(k)) {
            entry.last_used = self.build;
            return Ok(Some(entry.value));
        }
        let found = tight_bounds(bodies)?;
        if let (Some(key), Some(value)) = (key, found) {
            let last_used = self.build;
            self.bounds.insert(key, CacheEntry { value, last_used });
        }
        Ok(found)
    }

    /// Forgets entries no recent build has used.
    fn end_build(&mut self) {
        let build = self.build;
        self.entries.retain(|_, e| build - e.last_used < CACHE_BUILDS);
        self.bounds.retain(|_, e| build - e.last_used < CACHE_BUILDS);
        self.build += 1;
    }
}

/// One build's state: the plates `arrange` lays out, and the kernel's cache.
struct Build<'a> {
    plates: Vec<Plate>,
    cache: &'a mut SolidCache,
}

/// Presentation inherited down the spec tree. The innermost `Color`/`Part`
/// wins; `None` means nothing inside set it, so it's whatever the outside
/// says (and finally the default color, unnamed).
#[derive(Debug, Clone, Default, PartialEq)]
struct Style {
    color: Option<Rgb>,
    part: Option<String>,
}

impl Style {
    /// This style inside `outer`: fields set here win, the rest are `outer`'s.
    /// Building a subtree with the default style and then putting each body
    /// under the inherited one gives what building it inherited would, since
    /// the innermost setting always wins. That's what lets the cache ignore
    /// styles.
    fn under(self, outer: &Style) -> Style {
        Style {
            color: self.color.or(outer.color),
            part: self.part.or_else(|| outer.part.clone()),
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
        ModelSpec::RoundedRect { width, height, radius } => {
            validate_primitive(spec)?;
            Ok((vec![rounded_rect_loop(*width, *height, *radius)?], style))
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
        ModelSpec::Color { rgb, shape } => build_profile(
            shape,
            Style {
                color: Some(*rgb),
                ..style
            },
        ),
        ModelSpec::Part { name, shape } => build_profile(
            shape,
            Style {
                part: Some(part_name(name)?),
                ..style
            },
        ),
        _ => Err(invalid(
            "extrude profile must be a single 2D shape (rect, rounded rect, polygon, circle or cutout, optionally moved, colored or named)",
        )),
    }
}

fn polygon_loop(points: &[[f64; 2]]) -> Result<Vec<Edge>, KernelError> {
    let points: Vec<DVec3> = points.iter().map(|[x, y]| DVec3::new(*x, *y, 0.0)).collect();
    Edge::polygon(&points).map_err(occ("polygon"))
}

/// Counter-clockwise from the bottom edge: a side, then the corner arc after
/// it. Sides a fully round end has used up are left out.
fn rounded_rect_loop(width: f64, height: f64, radius: f64) -> Result<Vec<Edge>, KernelError> {
    let r = radius.min(width / 2.0).min(height / 2.0);
    let p = |x: f64, y: f64| DVec3::new(x, y, 0.0);
    // (side start, side end, corner center); each arc runs from its side's
    // end to the next side's start.
    let sides = [
        (p(r, 0.0), p(width - r, 0.0), p(width - r, r)),
        (p(width, r), p(width, height - r), p(width - r, height - r)),
        (p(width - r, height), p(r, height), p(r, height - r)),
        (p(0.0, height - r), p(0.0, r), p(r, r)),
    ];
    let mut edges = Vec::new();
    for (i, &(start, end, center)) in sides.iter().enumerate() {
        if start.distance(end) > ROUND_END_SLACK {
            edges.push(Edge::line(start, end).map_err(occ("rounded rect"))?);
        }
        let next = sides[(i + 1) % 4].0;
        let mid = center + ((end - center) + (next - center)).normalize() * r;
        edges.push(Edge::arc_3pts(end, mid, next).map_err(occ("rounded rect"))?);
    }
    Ok(edges)
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

fn build_solids(spec: &ModelSpec, style: &Style, build: &mut Build) -> Result<Bodies, KernelError> {
    let Some(key) = SolidCache::key(spec) else {
        return build_uncached(spec, style, build);
    };
    // Cached unstyled, so the same shape under another color or part name
    // (or measured bare by a script's `bounds`) is still a hit.
    let bodies = match build.cache.get(&key) {
        Some(bodies) => bodies,
        None => {
            let bodies = build_uncached(spec, &Style::default(), build)?;
            build.cache.insert(key, bodies.clone());
            bodies
        }
    };
    Ok(bodies.into_iter().map(|(s, st)| (s, st.under(style))).collect())
}

fn build_uncached(spec: &ModelSpec, style: &Style, build: &mut Build) -> Result<Bodies, KernelError> {
    match spec {
        ModelSpec::Rect { .. }
        | ModelSpec::RoundedRect { .. }
        | ModelSpec::Polygon { .. }
        | ModelSpec::Circle { .. }
        | ModelSpec::Cutout { .. } => {
            // OCC meshes solids, not lone faces, so a profile that isn't swept
            // shows (and exports) as a plate `FLAT_THICKNESS` thick.
            let (loops, style) = build_profile(spec, style.clone())?;
            let edges: Vec<Edge> = loops.into_iter().flatten().collect();
            let solid = Solid::extrude(&edges, DVec3::Z * FLAT_THICKNESS).map_err(occ("profile"))?;
            Ok(vec![(solid, style)])
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
        ModelSpec::Sphere { .. } | ModelSpec::Cylinder { .. } | ModelSpec::Cone { .. } | ModelSpec::Torus { .. } => {
            validate_primitive(spec)?;
            let solid = match *spec {
                ModelSpec::Sphere { radius } => Solid::sphere(radius),
                ModelSpec::Cylinder { radius, height } => Solid::cylinder(radius, DVec3::Z * height),
                ModelSpec::Cone { radius, top, height } => Solid::cone(radius, top, DVec3::Z * height),
                ModelSpec::Torus { radius, tube } => Solid::torus(radius, tube, DVec3::Z),
                _ => unreachable!("matched above"),
            };
            Ok(vec![(solid, style.clone())])
        }
        ModelSpec::Translate { offset, shape } => {
            let v = vec3(*offset);
            Ok(build_solids(shape, style, build)?
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
            Ok(build_solids(shape, style, build)?
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
                bodies.extend(build_solids(child, style, build)?);
            }
            Ok(bodies)
        }
        ModelSpec::Color { rgb, shape } => build_solids(
            shape,
            &Style {
                color: Some(*rgb),
                ..style.clone()
            },
            build,
        ),
        ModelSpec::Part { name, shape } => build_solids(
            shape,
            &Style {
                part: Some(part_name(name)?),
                ..style.clone()
            },
            build,
        ),
        ModelSpec::Fillet { shape, radius, edges } | ModelSpec::Chamfer { shape, distance: radius, edges } => {
            let fillet = matches!(spec, ModelSpec::Fillet { .. });
            let (op, size) = if fillet { ("fillet", "radius") } else { ("chamfer", "distance") };
            if !(*radius > 0.0 && radius.is_finite()) {
                return Err(invalid(format!("{op} {size} must be positive")));
            }
            let mut result = Vec::new();
            for (solid, body_style) in build_solids(shape, style, build)? {
                let (min, max) = edge_bounds(&solid);
                let picked: Vec<&Edge> = solid
                    .iter_edge()
                    .filter(|e| {
                        let points = edge_points(e);
                        let tangents = [unit_or_zero(e.start_tangent()), unit_or_zero(e.end_tangent())];
                        edge_selected(edges, &EdgeShape { points: &points, tangents }, min, max)
                    })
                    .collect();
                if picked.is_empty() {
                    return Err(invalid(format!("{op}: no edges match {edges:?}")));
                }
                let rounded = if fillet {
                    solid.fillet_edges(*radius, picked)
                } else {
                    solid.chamfer_edges(*radius, picked)
                };
                let rounded = rounded.map_err(|e| {
                    KernelError::Message(format!(
                        "{op} failed in OpenCascade: {e}. The {size} may be too large for the selected edges, \
                         or a selector picked a cylinder's seam edge"
                    ))
                })?;
                result.push((rounded, body_style));
            }
            Ok(result)
        }
        ModelSpec::Union { children } | ModelSpec::Intersect { children } => {
            let op = if matches!(spec, ModelSpec::Union { .. }) { "union" } else { "intersect" };
            let mut operands = Vec::new();
            for child in children {
                operands.extend(build_solids(child, style, build)?);
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
                cutters.extend(build_solids(tool, style, build)?);
            }
            let mut result = Vec::new();
            for (base, base_style) in build_solids(shape, style, build)? {
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
                let bodies = build_solids(child, style, build)?;
                let (min, max) = build
                    .cache
                    .bounds(SolidCache::any_key(child), &bodies)?
                    .ok_or_else(|| invalid("arrange child has no geometry"))?;
                footprints.push([max.x - min.x, max.y - min.y]);
                built.push((bodies, min));
            }
            let packing = pack(&footprints, *gap, bed.as_ref());
            build.plates.extend(packing.plates);
            let mut arranged = Vec::new();
            for ((bodies, min), [x, y]) in built.into_iter().zip(packing.positions) {
                let offset = DVec3::new(x - min.x, y - min.y, -min.z);
                arranged.extend(bodies.into_iter().map(|(s, st)| (s.translate(offset), st)));
            }
            Ok(arranged)
        }
    }
}

fn edge_points(edge: &Edge) -> Vec<[f64; 3]> {
    edge.approximation_segments(TESSELLATION)
        .into_iter()
        .map(|p| p.to_array())
        .collect()
}

fn unit_or_zero(v: DVec3) -> [f64; 3] {
    v.normalize_or_zero().to_array()
}

/// Extremes of a solid's edges. Exact for selecting edges at the solid's
/// max/min, unlike OCC's bounding box, which is padded by a tolerance.
fn edge_bounds(solid: &Solid) -> ([f64; 3], [f64; 3]) {
    let mut min = [f64::MAX; 3];
    let mut max = [f64::MIN; 3];
    for p in solid.iter_edge().flat_map(edge_points) {
        for i in 0..3 {
            min[i] = min[i].min(p[i]);
            max[i] = max[i].max(p[i]);
        }
    }
    (min, max)
}

/// Bounds from a fine mesh's vertices, which lie on the surface: within the
/// fine chord tolerance (0.01 mm) of exact, and exact on planar faces and
/// edges. OCC's own bounding box is padded by tolerances and loose around
/// curved faces, so a part dropped onto the plate by it would float.
fn tight_bounds(bodies: &Bodies) -> Result<Option<(DVec3, DVec3)>, KernelError> {
    let mut bounds: Option<(DVec3, DVec3)> = None;
    for (solid, _) in bodies {
        // Meshing stores the triangulation on the shape, and a later coarser
        // mesh would keep it, so mesh a copy (deep copies drop meshes).
        let copy = solid.clone();
        let mesh = Solid::mesh([&copy], TESSELLATION).map_err(occ("meshing"))?;
        for &v in &mesh.vertices {
            bounds = Some(match bounds {
                None => (v, v),
                Some((min, max)) => (min.min(v), max.max(v)),
            });
        }
    }
    Ok(bounds)
}

fn to_f32(v: DVec3) -> [f32; 3] {
    [v.x as f32, v.y as f32, v.z as f32]
}

/// Appends `solid`'s triangles (all painted `style.color`) and edges, meshed
/// at `tessellation`, to `scene`, recording its vertex and index ranges under
/// its part name.
fn tessellate_into(
    scene: &mut TessellatedScene,
    solid: &Solid,
    style: &Style,
    tessellation: Tessellation,
) -> Result<(), KernelError> {
    let mesh = Solid::mesh([solid], tessellation).map_err(occ("meshing"))?;
    let vertex_start = scene.positions.len() as u32;
    let index_start = scene.indices.len() as u32;

    scene.positions.extend(mesh.vertices.iter().map(|v| to_f32(*v)));
    scene.normals.extend(mesh.normals.iter().map(|n| to_f32(*n)));
    let color = style.color.unwrap_or(DEFAULT_COLOR);
    scene.colors.extend(std::iter::repeat_n(color, mesh.vertices.len()));
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
    fn bare_profiles_show_as_thin_plates_keeping_their_style() {
        let square = ModelSpec::Translate {
            offset: [1.0, 2.0, 3.0],
            shape: Box::new(ModelSpec::Color {
                rgb: [255, 0, 0],
                shape: Box::new(ModelSpec::Rect {
                    width: 10.0,
                    height: 10.0,
                }),
            }),
        };
        let scene = OccKernel::default().build(&square).unwrap();
        let aabb = scene.aabb().unwrap();
        assert_eq!(aabb.min, [1.0, 2.0, 3.0]);
        assert!((aabb.max[0] - 11.0).abs() < 1e-4 && (aabb.max[2] - 3.0 - FLAT_THICKNESS as f32).abs() < 1e-4, "{aabb:?}");
        assert!(scene.colors.iter().all(|c| *c == [255, 0, 0]));
    }

    #[test]
    fn coarser_quality_meshes_curves_with_fewer_triangles_within_tolerance() {
        let disc = ModelSpec::Extrude {
            profile: Box::new(ModelSpec::Circle { radius: 40.0 }),
            distance: 5.0,
            direction: None,
        };
        let triangles: Vec<usize> = MeshQuality::ALL
            .iter()
            .map(|&q| {
                let scene = OccKernel::default().build_with(&disc, q).unwrap();
                let aabb = scene.aabb().unwrap();
                // Inscribed polygon: the rim comes in by at most the chord tolerance.
                let tol = q.chord_tolerance() as f32 + 1e-4;
                assert!(aabb.max[0] <= 40.0 + 1e-4 && aabb.max[0] >= 40.0 - tol, "{q:?}: {aabb:?}");
                scene.indices.len() / 3
            })
            .collect();
        assert!(triangles.windows(2).all(|w| w[0] < w[1]), "{triangles:?}");
        assert_eq!(OccKernel::default().build(&disc).unwrap().indices.len() / 3, triangles[2]);
    }

    fn cuboid(at: [f64; 3], size: f64) -> ModelSpec {
        ModelSpec::Translate {
            offset: at,
            shape: Box::new(ModelSpec::Extrude {
                profile: Box::new(ModelSpec::Rect {
                    width: size,
                    height: size,
                }),
                distance: size,
                direction: None,
            }),
        }
    }

    fn fused(shift: f64) -> ModelSpec {
        ModelSpec::Union {
            children: vec![cuboid([0.0; 3], 10.0), cuboid([5.0 + shift, 5.0, 5.0], 10.0)],
        }
    }

    fn moved(spec: ModelSpec) -> ModelSpec {
        ModelSpec::Translate {
            offset: [100.0, 0.0, 0.0],
            shape: Box::new(spec),
        }
    }

    #[test]
    fn booleans_are_cached_across_builds_views_and_styles() {
        let mut kernel = OccKernel::default();
        let first = kernel.build(&fused(0.0)).unwrap();
        assert_eq!(kernel.cache_hits(), 0);

        // The same union again, moved (as a print view moves parts): one hit, same mesh.
        let again = kernel.build(&moved(fused(0.0))).unwrap();
        assert_eq!(kernel.cache_hits(), 1);
        assert_eq!(again.indices, first.indices);
        assert_eq!(again.aabb().unwrap().min[0], first.aabb().unwrap().min[0] + 100.0);

        // A coarser mesh reuses the solids too.
        kernel.build_with(&fused(0.0), MeshQuality::Draft).unwrap();
        assert_eq!(kernel.cache_hits(), 2);

        // A changed operand is a different solid.
        kernel.build(&fused(1.0)).unwrap();
        assert_eq!(kernel.cache_hits(), 2);

        // A color or name around it isn't, and still applies to the hit.
        let red = ModelSpec::Color {
            rgb: [255, 0, 0],
            shape: Box::new(fused(0.0)),
        };
        assert!(kernel.build(&red).unwrap().colors.iter().all(|c| *c == [255, 0, 0]));
        assert_eq!(kernel.cache_hits(), 3);
        let named = ModelSpec::Part {
            name: "fused".into(),
            shape: Box::new(fused(0.0)),
        };
        let scene = kernel.build(&named).unwrap();
        assert_eq!(kernel.cache_hits(), 4);
        assert_eq!(scene.parts.len(), 1);
        assert_eq!(scene.parts[0].name, "fused");
        assert!(scene.colors.iter().all(|c| *c == DEFAULT_COLOR));

        // So is measuring it, as a script's `bounds` does before the build.
        let mut kernel = OccKernel::default();
        kernel.bounds(&fused(0.0)).unwrap();
        kernel.build(&named).unwrap();
        assert_eq!(kernel.cache_hits(), 1);
    }

    #[test]
    fn unused_cache_entries_are_dropped_after_a_few_builds() {
        let mut kernel = OccKernel::default();
        kernel.build(&fused(0.0)).unwrap();
        for _ in 0..CACHE_BUILDS {
            kernel.build(&cuboid([0.0; 3], 1.0)).unwrap();
        }
        assert!(kernel.cache.entries.is_empty());
        kernel.build(&fused(0.0)).unwrap();
        assert_eq!(kernel.cache_hits(), 0);
    }

    #[test]
    fn arranged_curved_parts_sit_exactly_on_the_plate() {
        // OCC's padded bounding box would leave these floating.
        let spec = ModelSpec::Arrange {
            gap: 5.0,
            children: vec![
                ModelSpec::Translate {
                    offset: [0.0, 0.0, 40.0],
                    shape: Box::new(ModelSpec::Sphere { radius: 5.0 }),
                },
                ModelSpec::Torus { radius: 8.0, tube: 2.0 },
            ],
            bed: None,
        };
        let mut kernel = OccKernel::default();
        for _ in 0..2 {
            let scene = kernel.build_with(&spec, MeshQuality::Draft).unwrap();
            let aabb = scene.aabb().unwrap();
            assert!(aabb.min[2].abs() < 1e-3, "{aabb:?}");
        }
        // The second build took both bounds from the cache.
        assert_eq!(kernel.cache.bounds.len(), 2);
    }

    #[test]
    fn booleans_holding_an_arrange_are_rebuilt_so_plates_come_back() {
        let arranged = ModelSpec::Union {
            children: vec![ModelSpec::Arrange {
                gap: 5.0,
                children: vec![cuboid([0.0; 3], 10.0)],
                bed: Some(cad_ir::Bed {
                    width: 200.0,
                    depth: 200.0,
                    height: 200.0,
                }),
            }],
        };
        let mut kernel = OccKernel::default();
        assert_eq!(kernel.build(&arranged).unwrap().plates.len(), 1);
        assert_eq!(kernel.build(&arranged).unwrap().plates.len(), 1);
        assert_eq!(kernel.cache_hits(), 0);
    }
}
