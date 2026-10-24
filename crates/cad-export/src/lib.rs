//! Exports a [`TessellatedScene`] for slicers.
//!
//! 3MF: one object per named part (plus an `unnamed` object for geometry
//! outside any part), welded into watertight meshes, colored with core-spec
//! base materials. Units are millimeters, matching the IR.

use std::collections::HashMap;
use std::io::{Seek, Write};

use cad_ir::{DEFAULT_COLOR, Rgb, TessellatedScene};
use thiserror::Error;

/// Name of the object holding geometry that isn't in any `part`.
pub const UNNAMED: &str = "unnamed";

/// Positions closer than this (mm) are welded into one vertex.
const WELD_TOLERANCE: f32 = 1e-5;

#[derive(Debug, Error)]
pub enum ExportError {
    #[error("nothing to export: the scene has no triangles")]
    Empty,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
}

/// One exported mesh: shared vertices (no per-face duplicates) so edges are
/// manifold, plus each triangle's color.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportObject {
    pub name: String,
    pub vertices: Vec<[f32; 3]>,
    pub triangles: Vec<[u32; 3]>,
    pub triangle_colors: Vec<Rgb>,
}

impl ExportObject {
    /// Closed and consistently oriented (what slicers need): every directed
    /// edge appears exactly once and its reverse exactly once. Several
    /// disjoint closed shells in one object are fine.
    pub fn is_closed(&self) -> bool {
        let mut edges: HashMap<(u32, u32), usize> = HashMap::new();
        for [a, b, c] in &self.triangles {
            for edge in [(*a, *b), (*b, *c), (*c, *a)] {
                *edges.entry(edge).or_default() += 1;
            }
        }
        !edges.is_empty()
            && edges
                .iter()
                .all(|(&(from, to), &count)| count == 1 && edges.get(&(to, from)) == Some(&1))
    }
}

/// Splits `scene` into one welded object per part name (first-seen order; a
/// part split into several runs becomes one object), then [`UNNAMED`] for any
/// triangles outside parts.
pub fn objects(scene: &TessellatedScene) -> Vec<ExportObject> {
    let mut by_name: Vec<(String, Vec<std::ops::Range<usize>>)> = Vec::new();
    let mut covered = vec![false; scene.indices.len()];
    for part in &scene.parts {
        let range = part.indices.start as usize..part.indices.end as usize;
        covered[range.clone()].fill(true);
        match by_name.iter_mut().find(|(name, _)| *name == part.name) {
            Some((_, ranges)) => ranges.push(range),
            None => by_name.push((part.name.clone(), vec![range])),
        }
    }

    // Uncovered triangles, as ranges.
    let mut unnamed = Vec::new();
    let mut start = None;
    for (i, c) in covered.iter().enumerate().step_by(3) {
        match (c, start) {
            (false, None) => start = Some(i),
            (true, Some(s)) => {
                unnamed.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        unnamed.push(s..scene.indices.len());
    }
    if !unnamed.is_empty() {
        by_name.push((UNNAMED.to_string(), unnamed));
    }

    by_name
        .into_iter()
        .map(|(name, ranges)| weld(scene, name, &ranges))
        .filter(|object| !object.triangles.is_empty())
        .collect()
}

/// Merges positions within [`WELD_TOLERANCE`] of each other. Points are
/// bucketed on a tolerance-sized grid and matched against the neighbouring
/// cells too, so two near-equal points straddling a cell boundary still merge
/// (boolean results compute the same point twice with float noise).
struct Welder {
    cells: HashMap<[i64; 3], Vec<u32>>,
}

impl Welder {
    fn cell(p: [f32; 3]) -> [i64; 3] {
        p.map(|c| (c / WELD_TOLERANCE).floor() as i64)
    }

    fn index(&mut self, vertices: &mut Vec<[f32; 3]>, p: [f32; 3]) -> u32 {
        let [cx, cy, cz] = Self::cell(p);
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    for &i in self.cells.get(&[cx + dx, cy + dy, cz + dz]).into_iter().flatten() {
                        let q = vertices[i as usize];
                        let d2 = (0..3).map(|k| (p[k] - q[k]).powi(2)).sum::<f32>();
                        if d2 <= WELD_TOLERANCE * WELD_TOLERANCE {
                            return i;
                        }
                    }
                }
            }
        }
        vertices.push(p);
        let i = (vertices.len() - 1) as u32;
        self.cells.entry([cx, cy, cz]).or_default().push(i);
        i
    }
}

fn weld(scene: &TessellatedScene, name: String, ranges: &[std::ops::Range<usize>]) -> ExportObject {
    let mut welder = Welder {
        cells: HashMap::new(),
    };
    let mut object = ExportObject {
        name,
        vertices: Vec::new(),
        triangles: Vec::new(),
        triangle_colors: Vec::new(),
    };
    for range in ranges {
        for tri in scene.indices[range.clone()].chunks_exact(3) {
            let welded =
                [tri[0], tri[1], tri[2]].map(|i| welder.index(&mut object.vertices, scene.positions[i as usize]));
            if welded[0] == welded[1] || welded[1] == welded[2] || welded[0] == welded[2] {
                continue; // Degenerate after welding.
            }
            object.triangles.push(welded);
            object
                .triangle_colors
                .push(scene.colors.get(tri[0] as usize).copied().unwrap_or(DEFAULT_COLOR));
        }
    }
    object
}

/// The `3D/3dmodel.model` document for `objects`.
pub fn model_xml(objects: &[ExportObject]) -> String {
    let mut colors: Vec<Rgb> = Vec::new();
    for c in objects.iter().flat_map(|o| &o.triangle_colors) {
        if !colors.contains(c) {
            colors.push(*c);
        }
    }
    let color_index = |c: &Rgb| colors.iter().position(|x| x == c).unwrap();
    const MATERIALS_ID: usize = 1;
    let object_id = |i: usize| i + 2;

    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(
        "<model unit=\"millimeter\" xml:lang=\"en-US\" \
         xmlns=\"http://schemas.microsoft.com/3dmanufacturing/core/2015/02\">\n",
    );
    xml.push_str(" <resources>\n");
    xml.push_str(&format!("  <basematerials id=\"{MATERIALS_ID}\">\n"));
    for c in &colors {
        let hex = format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2]);
        xml.push_str(&format!("   <base name=\"{hex}\" displaycolor=\"{hex}\"/>\n"));
    }
    xml.push_str("  </basematerials>\n");

    for (i, object) in objects.iter().enumerate() {
        let default = object.triangle_colors.first().map(color_index).unwrap_or(0);
        xml.push_str(&format!(
            "  <object id=\"{}\" type=\"model\" name=\"{}\" pid=\"{MATERIALS_ID}\" pindex=\"{default}\">\n   <mesh>\n    <vertices>\n",
            object_id(i),
            escape(&object.name)
        ));
        for [x, y, z] in &object.vertices {
            xml.push_str(&format!("     <vertex x=\"{x}\" y=\"{y}\" z=\"{z}\"/>\n"));
        }
        xml.push_str("    </vertices>\n    <triangles>\n");
        for ([v1, v2, v3], color) in object.triangles.iter().zip(&object.triangle_colors) {
            let index = color_index(color);
            if index == default {
                xml.push_str(&format!("     <triangle v1=\"{v1}\" v2=\"{v2}\" v3=\"{v3}\"/>\n"));
            } else {
                xml.push_str(&format!(
                    "     <triangle v1=\"{v1}\" v2=\"{v2}\" v3=\"{v3}\" p1=\"{index}\"/>\n"
                ));
            }
        }
        xml.push_str("    </triangles>\n   </mesh>\n  </object>\n");
    }
    xml.push_str(" </resources>\n <build>\n");
    for i in 0..objects.len() {
        xml.push_str(&format!("  <item objectid=\"{}\"/>\n", object_id(i)));
    }
    xml.push_str(" </build>\n</model>\n");
    xml
}

const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
 <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
 <Default Extension="model" ContentType="application/vnd.ms-package.3dmanufacturing-3dmodel+xml"/>
</Types>
"#;

const RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
 <Relationship Target="/3D/3dmodel.model" Id="rel0" Type="http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel"/>
</Relationships>
"#;

/// Writes `scene` as a 3MF package. Returns the number of objects written.
pub fn write_3mf<W: Write + Seek>(scene: &TessellatedScene, writer: W) -> Result<usize, ExportError> {
    let objects = objects(scene);
    if objects.is_empty() {
        return Err(ExportError::Empty);
    }
    let mut zip = zip::ZipWriter::new(writer);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (path, contents) in [
        ("[Content_Types].xml", CONTENT_TYPES.to_string()),
        ("_rels/.rels", RELS.to_string()),
        ("3D/3dmodel.model", model_xml(&objects)),
    ] {
        zip.start_file(path, options)?;
        zip.write_all(contents.as_bytes())?;
    }
    zip.finish()?;
    Ok(objects.len())
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::{Cursor, Read};

    use cad_ir::ModelSpec;
    use cad_kernel::Kernel;
    use cad_kernel_truck::TruckKernel;

    use super::*;

    fn rect(width: f64, height: f64) -> ModelSpec {
        ModelSpec::Rect { width, height }
    }

    fn sized_box(x: f64, y: f64, z: f64) -> ModelSpec {
        ModelSpec::Extrude {
            profile: Box::new(rect(x, y)),
            distance: z,
        }
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

    fn translate(offset: [f64; 3], shape: ModelSpec) -> ModelSpec {
        ModelSpec::Translate {
            offset,
            shape: Box::new(shape),
        }
    }

    fn group(children: Vec<ModelSpec>) -> ModelSpec {
        ModelSpec::Group { children }
    }

    fn build(spec: &ModelSpec) -> TessellatedScene {
        TruckKernel.build(spec).unwrap()
    }

    /// Closed and consistently oriented: each directed edge appears once and
    /// its reverse appears once.
    fn assert_watertight(object: &ExportObject) {
        let mut edges: HashMap<(u32, u32), usize> = HashMap::new();
        for [a, b, c] in &object.triangles {
            for (from, to) in [(*a, *b), (*b, *c), (*c, *a)] {
                *edges.entry((from, to)).or_default() += 1;
            }
        }
        for (&(from, to), &count) in &edges {
            assert_eq!(count, 1, "{}: edge {from}->{to} used {count} times", object.name);
            assert_eq!(
                edges.get(&(to, from)),
                Some(&1),
                "{}: edge {from}->{to} has no opposite",
                object.name
            );
        }
    }

    /// Signed volume via the divergence theorem; positive means outward normals.
    fn signed_volume(object: &ExportObject) -> f64 {
        object
            .triangles
            .iter()
            .map(|t| {
                let [a, b, c] = t.map(|i| object.vertices[i as usize].map(f64::from));
                (a[0] * (b[1] * c[2] - b[2] * c[1]) - a[1] * (b[0] * c[2] - b[2] * c[0])
                    + a[2] * (b[0] * c[1] - b[1] * c[0]))
                    / 6.0
            })
            .sum()
    }

    #[test]
    fn welding_merges_near_points_across_grid_cells() {
        let mut welder = Welder {
            cells: HashMap::new(),
        };
        let mut vertices = Vec::new();
        // Straddles a cell boundary at x = 0 but is well within tolerance.
        let a = welder.index(&mut vertices, [-WELD_TOLERANCE * 0.1, 1.0, 1.0]);
        let b = welder.index(&mut vertices, [WELD_TOLERANCE * 0.1, 1.0, 1.0]);
        assert_eq!(a, b);
        // Clearly separate points stay separate.
        let c = welder.index(&mut vertices, [WELD_TOLERANCE * 3.0, 1.0, 1.0]);
        assert_ne!(a, c);
        assert_eq!(vertices.len(), 2);
    }

    #[test]
    fn is_closed_rejects_open_surfaces() {
        let plate = objects(&build(&part("plate", rect(5.0, 5.0))));
        assert!(!plate[0].is_closed(), "a lone face has open edges");
        let block = objects(&build(&part("block", sized_box(1.0, 1.0, 1.0))));
        assert!(block[0].is_closed());
        // Two disjoint boxes in one part are still closed.
        let pair = group(vec![sized_box(1.0, 1.0, 1.0), translate([3.0, 0.0, 0.0], sized_box(1.0, 1.0, 1.0))]);
        assert!(objects(&build(&part("pair", pair)))[0].is_closed());
    }

    #[test]
    fn box_part_exports_as_closed_outward_mesh() {
        let scene = build(&part("block", sized_box(2.0, 3.0, 4.0)));
        let objects = objects(&scene);

        assert_eq!(objects.len(), 1);
        let block = &objects[0];
        assert_eq!(block.name, "block");
        assert_eq!(block.vertices.len(), 8, "corners must be welded");
        assert_eq!(block.triangles.len(), 12);
        assert_watertight(block);
        assert!(block.is_closed());
        assert!((signed_volume(block) - 24.0).abs() < 1e-3, "{}", signed_volume(block));
    }

    #[test]
    fn non_convex_polygon_prism_exports_closed_with_exact_volume() {
        let u = ModelSpec::Polygon {
            points: vec![
                [0.0, 0.0], [3.0, 0.0], [3.0, 15.0], [23.0, 15.0],
                [23.0, 0.0], [26.0, 0.0], [26.0, 18.0], [0.0, 18.0],
            ],
        };
        let prism = ModelSpec::Extrude {
            profile: Box::new(u),
            distance: 10.0,
        };
        let object = &objects(&build(&part("holder", prism)))[0];
        assert!(object.is_closed());
        assert_watertight(object);
        let expected = (26.0 * 18.0 - 20.0 * 15.0) * 10.0;
        assert!((signed_volume(object) - expected).abs() < 1e-2, "{}", signed_volume(object));
    }

    #[test]
    fn boolean_results_export_closed_with_exact_volume() {
        let ten = || translate([0.0; 3], sized_box(10.0, 10.0, 10.0));
        let cases = [
            // 1000 - a 4 x 4 x 10 through-hole.
            (
                ModelSpec::Difference {
                    shape: Box::new(ten()),
                    tools: vec![translate([3.0, 3.0, -1.0], sized_box(4.0, 4.0, 12.0))],
                },
                840.0,
            ),
            // Two 1000 cubes overlapping in a 5 x 4.5 x 4 corner.
            (
                ModelSpec::Union {
                    children: vec![ten(), translate([5.0, 5.5, 6.0], sized_box(10.0, 10.0, 10.0))],
                },
                2000.0 - 90.0,
            ),
        ];
        for (spec, volume) in cases {
            let object = &objects(&build(&part("solid", spec)))[0];
            assert!(object.is_closed());
            assert!((signed_volume(object) - volume).abs() < 1e-2, "{} vs {volume}", signed_volume(object));
        }
    }

    #[test]
    fn round_hole_by_difference_with_a_cylinder() {
        use std::f64::consts::PI;
        let plate = translate([0.0; 3], sized_box(20.0, 20.0, 4.0));
        let cutter = translate(
            [10.0, 10.0, -1.0],
            ModelSpec::Extrude {
                profile: Box::new(ModelSpec::Circle { radius: 3.0 }),
                distance: 6.0,
            },
        );
        let spec = ModelSpec::Difference {
            shape: Box::new(plate),
            tools: vec![cutter],
        };
        let object = &objects(&build(&part("plate", spec)))[0];
        assert!(object.is_closed());
        let volume = (400.0 - PI * 9.0) * 4.0;
        let got = signed_volume(object);
        assert!((got - volume).abs() < volume * 0.005, "{got} vs {volume}");
    }

    #[test]
    fn curved_profiles_export_closed_with_the_right_volume() {
        use std::f64::consts::PI;
        let circle = |radius: f64| ModelSpec::Circle { radius };
        let extrude = |profile: ModelSpec, distance: f64| ModelSpec::Extrude {
            profile: Box::new(profile),
            distance,
        };
        let cutout = |outer: ModelSpec, holes: Vec<ModelSpec>| ModelSpec::Cutout {
            outer: Box::new(outer),
            holes,
        };
        let cases = [
            ("cylinder", extrude(circle(10.0), 5.0), PI * 100.0 * 5.0),
            ("washer", extrude(cutout(circle(10.0), vec![circle(4.0)]), 2.0), PI * (100.0 - 16.0) * 2.0),
            (
                "holed plate",
                extrude(cutout(rect(40.0, 20.0), vec![translate([10.0, 10.0, 0.0], circle(3.0))]), 3.0),
                (800.0 - PI * 9.0) * 3.0,
            ),
        ];
        for (name, spec, volume) in cases {
            let object = &objects(&build(&part(name, spec)))[0];
            assert!(object.is_closed(), "{name} not watertight");
            // Tessellated circles are inscribed polygons: solids come out a
            // little small, holes a little small too (so the part a little big).
            let got = signed_volume(object);
            assert!((got - volume).abs() < volume * 0.005, "{name}: {got} vs {volume}");
        }
    }

    #[test]
    fn every_part_is_its_own_watertight_object() {
        let spec = group(vec![
            paint([255, 0, 0], part("left", sized_box(1.0, 1.0, 1.0))),
            paint([0, 0, 255], part("right", translate([3.0, 0.0, 0.0], sized_box(1.0, 2.0, 3.0)))),
        ]);
        let objects = objects(&build(&spec));

        let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["left", "right"]);
        for object in &objects {
            assert_watertight(object);
            assert!(signed_volume(object) > 0.0);
        }
        assert!(objects[0].triangle_colors.iter().all(|c| *c == [255, 0, 0]));
        assert!(objects[1].triangle_colors.iter().all(|c| *c == [0, 0, 255]));
    }

    #[test]
    fn unnamed_geometry_becomes_one_trailing_object() {
        let spec = group(vec![
            sized_box(1.0, 1.0, 1.0),
            translate([2.0, 0.0, 0.0], part("named", sized_box(1.0, 1.0, 1.0))),
            translate([4.0, 0.0, 0.0], sized_box(1.0, 1.0, 1.0)),
        ]);
        let objects = objects(&build(&spec));
        let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["named", UNNAMED]);
        assert_eq!(objects[1].triangles.len(), 24, "both unnamed boxes");
        assert_eq!(objects[1].vertices.len(), 16);
    }

    #[test]
    fn split_part_runs_merge_into_one_object() {
        let spec = part(
            "frame",
            group(vec![
                sized_box(1.0, 1.0, 1.0),
                translate([2.0, 0.0, 0.0], part("leg", sized_box(1.0, 1.0, 1.0))),
                translate([4.0, 0.0, 0.0], sized_box(1.0, 1.0, 1.0)),
            ]),
        );
        let objects = objects(&build(&spec));
        let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["frame", "leg"]);
        assert_eq!(objects[0].triangles.len(), 24);
    }

    fn parse_model(xml: &str) -> roxmltree::Document<'_> {
        roxmltree::Document::parse(xml).unwrap()
    }

    #[test]
    fn model_xml_declares_units_materials_objects_and_build() {
        let spec = group(vec![
            paint([0xB0, 0x7A, 0x4F], part("side", sized_box(1.0, 1.0, 1.0))),
            paint([0xE8, 0xE0, 0xD0], part("back", translate([2.0, 0.0, 0.0], sized_box(1.0, 1.0, 1.0)))),
        ]);
        let xml = model_xml(&objects(&build(&spec)));
        let doc = parse_model(&xml);
        let root = doc.root_element();

        assert_eq!(root.attribute("unit"), Some("millimeter"));
        let colors: Vec<&str> = root
            .descendants()
            .filter(|n| n.has_tag_name("base"))
            .map(|n| n.attribute("displaycolor").unwrap())
            .collect();
        assert_eq!(colors, ["#B07A4F", "#E8E0D0"]);

        let objects: Vec<_> = root.descendants().filter(|n| n.has_tag_name("object")).collect();
        let names: Vec<&str> = objects.iter().map(|o| o.attribute("name").unwrap()).collect();
        assert_eq!(names, ["side", "back"]);
        let pindex: Vec<&str> = objects.iter().map(|o| o.attribute("pindex").unwrap()).collect();
        assert_eq!(pindex, ["0", "1"]);

        let items: Vec<&str> = root
            .descendants()
            .filter(|n| n.has_tag_name("item"))
            .map(|n| n.attribute("objectid").unwrap())
            .collect();
        let ids: Vec<&str> = objects.iter().map(|o| o.attribute("id").unwrap()).collect();
        assert_eq!(items, ids);

        // Every triangle references a vertex of its own object.
        for object in &objects {
            let vertices = object.descendants().filter(|n| n.has_tag_name("vertex")).count();
            for t in object.descendants().filter(|n| n.has_tag_name("triangle")) {
                for v in ["v1", "v2", "v3"] {
                    let i: usize = t.attribute(v).unwrap().parse().unwrap();
                    assert!(i < vertices);
                }
            }
        }
    }

    #[test]
    fn mixed_color_object_marks_off_color_triangles() {
        let spec = part(
            "two-tone",
            group(vec![
                paint([1, 1, 1], sized_box(1.0, 1.0, 1.0)),
                paint([2, 2, 2], translate([2.0, 0.0, 0.0], sized_box(1.0, 1.0, 1.0))),
            ]),
        );
        let xml = model_xml(&objects(&build(&spec)));
        let doc = parse_model(&xml);
        let p1: Vec<Option<&str>> = doc
            .descendants()
            .filter(|n| n.has_tag_name("triangle"))
            .map(|t| t.attribute("p1"))
            .collect();
        assert_eq!(p1.iter().filter(|p| p.is_none()).count(), 12);
        assert_eq!(p1.iter().filter(|p| **p == Some("1")).count(), 12);
    }

    #[test]
    fn coordinates_round_trip() {
        let scene = build(&part("moved", translate([10.5, -3.25, 7.0], sized_box(1.0, 2.0, 3.0))));
        let doc_xml = model_xml(&objects(&scene));
        let doc = parse_model(&doc_xml);
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        for v in doc.descendants().filter(|n| n.has_tag_name("vertex")) {
            for (i, axis) in ["x", "y", "z"].into_iter().enumerate() {
                let c: f32 = v.attribute(axis).unwrap().parse().unwrap();
                min[i] = min[i].min(c);
                max[i] = max[i].max(c);
            }
        }
        assert_eq!(min, [10.5, -3.25, 7.0]);
        assert_eq!(max, [11.5, -1.25, 10.0]);
    }

    #[test]
    fn names_are_escaped() {
        let name = r#"a<b & "c" 'd'>"#;
        let xml = model_xml(&objects(&build(&part(name, sized_box(1.0, 1.0, 1.0)))));
        let doc = parse_model(&xml);
        let object = doc.descendants().find(|n| n.has_tag_name("object")).unwrap();
        assert_eq!(object.attribute("name"), Some(name));
    }

    #[test]
    fn empty_scene_is_an_error() {
        let err = write_3mf(&TessellatedScene::default(), Cursor::new(Vec::new())).unwrap_err();
        assert!(matches!(err, ExportError::Empty), "{err}");
    }

    #[test]
    fn package_has_content_types_rels_and_model() {
        let scene = build(&part("block", sized_box(1.0, 1.0, 1.0)));
        let mut bytes = Cursor::new(Vec::new());
        write_3mf(&scene, &mut bytes).unwrap();

        let mut archive = zip::ZipArchive::new(Cursor::new(bytes.into_inner())).unwrap();
        let mut read = |path: &str| {
            let mut text = String::new();
            archive.by_name(path).unwrap().read_to_string(&mut text).unwrap();
            text
        };
        assert!(read("[Content_Types].xml").contains("3dmanufacturing-3dmodel+xml"));
        assert!(read("_rels/.rels").contains("Target=\"/3D/3dmodel.model\""));
        let model = read("3D/3dmodel.model");
        assert_eq!(model, model_xml(&objects(&scene)));
    }
}
