use std::ops::Range;

use serde::{Deserialize, Serialize};

/// sRGB color, one byte per channel.
pub type Rgb = [u8; 3];

/// Color for geometry no `Color` node applies to.
pub const DEFAULT_COLOR: Rgb = [150, 175, 210];

/// Kernel-agnostic model description produced by the TypeScript DSL.
/// All lengths are millimeters.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ModelSpec {
    /// Planar rectangle in the XY plane with a corner at the origin.
    Rect { width: f64, height: f64 },
    /// Planar simple polygon in the XY plane, from its outline. Either winding;
    /// the closing point is implied.
    Polygon { points: Vec<[f64; 2]> },
    /// Planar circle in the XY plane, centered on the origin.
    Circle { radius: f64 },
    /// A profile with holes: `outer` minus each of `holes`, all coplanar
    /// profiles. Holes should sit inside `outer` without touching each other.
    Cutout {
        outer: Box<ModelSpec>,
        holes: Vec<ModelSpec>,
    },
    /// Sweeps a 2D `profile` by `distance` along `direction` (normalized;
    /// default +Z) into a solid.
    Extrude {
        profile: Box<ModelSpec>,
        distance: f64,
        #[serde(default)]
        direction: Option<[f64; 3]>,
    },
    /// Lathe: the profile is drawn in XY with X as the radius and Y as the
    /// height, then spun `degrees` (default a full turn) around +Z. The
    /// profile must stay on the x >= 0 side of the axis.
    Revolve {
        profile: Box<ModelSpec>,
        #[serde(default = "full_turn")]
        degrees: f64,
    },
    /// Solid sphere centered on the origin.
    Sphere { radius: f64 },
    /// Solid cylinder standing on the XY plane: base centered on the origin,
    /// `height` up +Z.
    Cylinder { radius: f64, height: f64 },
    /// Solid cone or frustum standing on the XY plane: base of `radius`
    /// centered on the origin, top of `top` (0 for a point) at `height`.
    /// One of the radii may be 0.
    Cone { radius: f64, top: f64, height: f64 },
    /// Solid torus lying flat around +Z, centered on the origin: the tube's
    /// center runs on a circle of `radius`, and `tube` is its own radius
    /// (less than `radius`).
    Torus { radius: f64, tube: f64 },
    /// Moves `shape` by `offset` (2D profiles stay profiles; solids stay solids).
    Translate {
        offset: [f64; 3],
        shape: Box<ModelSpec>,
    },
    /// Rotates `shape` by `degrees` (right-hand rule) about the line through
    /// `origin` along `axis` (any non-zero length). Works on profiles and solids.
    Rotate {
        axis: [f64; 3],
        degrees: f64,
        #[serde(default)]
        origin: [f64; 3],
        shape: Box<ModelSpec>,
    },
    /// Several independent shapes shown together, not fused (see `Union`).
    Group { children: Vec<ModelSpec> },
    /// Fuses solids into one. truck needs their faces to cross: solids that
    /// only touch, or share a face plane, fail with an error.
    Union { children: Vec<ModelSpec> },
    /// Cuts every tool out of `shape`. Tools must overshoot the faces they cut
    /// through rather than end flush with them.
    Difference {
        shape: Box<ModelSpec>,
        tools: Vec<ModelSpec>,
    },
    /// Keeps the volume common to all children.
    Intersect { children: Vec<ModelSpec> },
    /// Rounds the selected edges of every solid in `shape` (OCC kernel only).
    Fillet {
        shape: Box<ModelSpec>,
        radius: f64,
        /// Union of selectors; empty means every edge.
        #[serde(default)]
        edges: Vec<EdgeSelector>,
    },
    /// Bevels the selected edges by `distance` (OCC kernel only).
    Chamfer {
        shape: Box<ModelSpec>,
        distance: f64,
        #[serde(default)]
        edges: Vec<EdgeSelector>,
    },
    /// Paints `shape`. The innermost `Color` wins; a colored profile colors its extrusion.
    Color { rgb: Rgb, shape: Box<ModelSpec> },
    /// Names `shape` as a part (for export and part lists). The innermost
    /// `Part` wins; a named profile names its extrusion.
    Part { name: String, shape: Box<ModelSpec> },
    /// Lays children out for printing: each moves (as a unit, never rotated)
    /// onto the plate at z = 0. Children fill rows along +X, `gap` apart.
    /// With a `bed`, rows wrap at the bed width, a new plate starts when the
    /// bed depth runs out, and plates are laid out in a grid. Without one,
    /// rows wrap so the block is roughly square.
    Arrange {
        gap: f64,
        children: Vec<ModelSpec>,
        #[serde(default)]
        bed: Option<Bed>,
    },
}

/// Picks edges of a solid, CadQuery style. `|Z` is a straight edge parallel
/// to Z; `>Z` / `<Z` is an edge lying entirely at the solid's max / min Z
/// (likewise for X and Y). Several selectors select their union.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeSelector {
    #[serde(rename = "all")]
    All,
    #[serde(rename = "|X")]
    ParallelX,
    #[serde(rename = "|Y")]
    ParallelY,
    #[serde(rename = "|Z")]
    ParallelZ,
    #[serde(rename = ">X")]
    MaxX,
    #[serde(rename = "<X")]
    MinX,
    #[serde(rename = ">Y")]
    MaxY,
    #[serde(rename = "<Y")]
    MinY,
    #[serde(rename = ">Z")]
    MaxZ,
    #[serde(rename = "<Z")]
    MinZ,
}

/// A printer's build volume, mm. Unknown keys are rejected so typos in
/// `printer.toml` don't go unnoticed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bed {
    pub width: f64,
    pub depth: f64,
    pub height: f64,
}

/// One build plate's footprint in a print layout, mm (XY, at z = 0).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Plate {
    pub min: [f32; 2],
    pub size: [f32; 2],
}

/// How finely kernels mesh curved surfaces. Coarser meshes build and draw
/// faster; [`MeshQuality::Fine`] (the default) is what exports use unless told
/// otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MeshQuality {
    Draft,
    Normal,
    #[default]
    Fine,
}

impl MeshQuality {
    pub const ALL: [MeshQuality; 3] = [MeshQuality::Draft, MeshQuality::Normal, MeshQuality::Fine];

    /// Largest distance from the mesh to the true surface, mm. Every preset is
    /// finer than an FDM printer resolves (about 0.1 mm).
    pub const fn chord_tolerance(self) -> f64 {
        match self {
            MeshQuality::Draft => 0.1,
            MeshQuality::Normal => 0.03,
            MeshQuality::Fine => 0.01,
        }
    }

    /// Largest angle between neighbouring facets, radians. Only matters for
    /// curves too tight for the chord tolerance to subdivide.
    pub const fn angular_tolerance(self) -> f64 {
        0.5
    }

    /// Capitalized name, for menus.
    pub fn label(self) -> &'static str {
        match self {
            MeshQuality::Draft => "Draft",
            MeshQuality::Normal => "Normal",
            MeshQuality::Fine => "Fine",
        }
    }
}

fn full_turn() -> f64 {
    360.0
}

/// The ways a model can be shown. `print` is optional: models without one are
/// assembled-only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Views<T> {
    pub assembled: T,
    /// Missing in JSON means `None` (serde's behavior for `Option` fields).
    pub print: Option<T>,
}

impl<T> Views<T> {
    pub fn assembled_only(assembled: T) -> Self {
        Self {
            assembled,
            print: None,
        }
    }
}

/// What a model script's default export evaluates to.
pub type ModelViews = Views<ModelSpec>;

impl ModelViews {
    /// Accepts either a bare spec (`{ kind: ... }`, assembled-only) or
    /// `{ assembled, print? }`. Decided by shape first so errors point at the
    /// real problem instead of "matched no variant".
    pub fn from_script_value(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        if value.get("assembled").is_some() {
            serde_json::from_value(value)
        } else {
            serde_json::from_value(value).map(Self::assembled_only)
        }
    }
}

/// Triangle mesh the viewer can upload without knowing the kernel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TessellatedScene {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    /// One per position. Empty means "no color information" (viewer default).
    #[serde(default)]
    pub colors: Vec<Rgb>,
    pub indices: Vec<u32>,
    /// Named parts, in order. Each covers a contiguous run of vertices and of
    /// indices, and its indices only reference its own vertices. Geometry
    /// outside any `Part` isn't listed.
    #[serde(default)]
    pub parts: Vec<ScenePart>,
    /// Build plates from an `Arrange` with a bed (empty otherwise).
    #[serde(default)]
    pub plates: Vec<Plate>,
    /// Optional line-list of boundary samples (pairs of points).
    #[serde(default)]
    pub edges: Vec<[f32; 3]>,
}

/// A named run of a [`TessellatedScene`]'s vertices and triangle indices.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenePart {
    pub name: String,
    pub vertices: Range<u32>,
    pub indices: Range<u32>,
}

impl TessellatedScene {
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty() || self.positions.is_empty()
    }

    pub fn aabb(&self) -> Option<Aabb> {
        let mut iter = self.positions.iter();
        let first = iter.next()?;
        let mut min = *first;
        let mut max = *first;
        for p in iter {
            for i in 0..3 {
                min[i] = min[i].min(p[i]);
                max[i] = max[i].max(p[i]);
            }
        }
        Some(Aabb { min, max })
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    pub min: [f32; 3],
    pub max: [f32; 3],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// False only for errors (the update then carries no new meshes).
    pub ok: bool,
    /// An `ok` diagnostic worth the user's attention (e.g. a part too big for the bed).
    #[serde(default)]
    pub warning: bool,
    pub message: String,
}

impl Diagnostic {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            warning: false,
            message: message.into(),
        }
    }

    pub fn warning(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            warning: true,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            warning: false,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mesh_quality_is_lowercase_and_gets_finer() {
        assert_eq!(serde_json::to_value(MeshQuality::Normal).unwrap(), "normal");
        assert_eq!(MeshQuality::default(), MeshQuality::Fine);
        let tolerances = MeshQuality::ALL.map(MeshQuality::chord_tolerance);
        assert!(tolerances.windows(2).all(|w| w[0] > w[1]), "{tolerances:?}");
        // Fine is the tolerance exports have always used.
        assert_eq!(MeshQuality::Fine.chord_tolerance(), 0.01);
    }

    #[test]
    fn rect_spec_round_trips_ts_shape() {
        let json = r#"{"kind":"Rect","width":10.0,"height":10.0}"#;
        let spec: ModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec,
            ModelSpec::Rect {
                width: 10.0,
                height: 10.0
            }
        );
        let back = serde_json::to_value(&spec).unwrap();
        assert_eq!(back["kind"], "Rect");
        assert_eq!(back["width"], 10.0);
        assert_eq!(back["height"], 10.0);
    }

    #[test]
    fn group_and_translate_match_ts_shape() {
        let json = r#"{"kind":"Group","children":[
            {"kind":"Rect","width":1,"height":2},
            {"kind":"Translate","offset":[1,2,3],"shape":{"kind":"Rect","width":1,"height":1}}
        ]}"#;
        let spec: ModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec,
            ModelSpec::Group {
                children: vec![
                    ModelSpec::Rect {
                        width: 1.0,
                        height: 2.0
                    },
                    ModelSpec::Translate {
                        offset: [1.0, 2.0, 3.0],
                        shape: Box::new(ModelSpec::Rect {
                            width: 1.0,
                            height: 1.0
                        }),
                    },
                ],
            }
        );
    }

    #[test]
    fn color_spec_matches_ts_shape() {
        let json = r#"{"kind":"Color","rgb":[176,122,79],"shape":{"kind":"Rect","width":1,"height":1}}"#;
        let spec: ModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec,
            ModelSpec::Color {
                rgb: [176, 122, 79],
                shape: Box::new(ModelSpec::Rect {
                    width: 1.0,
                    height: 1.0
                }),
            }
        );
    }

    #[test]
    fn color_channels_must_be_bytes() {
        for rgb in ["[256,0,0]", "[-1,0,0]", "[0.5,0,0]", "[0,0]"] {
            let json = format!(
                r#"{{"kind":"Color","rgb":{rgb},"shape":{{"kind":"Rect","width":1,"height":1}}}}"#
            );
            assert!(serde_json::from_str::<ModelSpec>(&json).is_err(), "{rgb} accepted");
        }
    }

    #[test]
    fn scene_without_colors_still_deserializes() {
        let json = r#"{"positions":[],"normals":[],"indices":[]}"#;
        let scene: TessellatedScene = serde_json::from_str(json).unwrap();
        assert!(scene.colors.is_empty());
    }

    #[test]
    fn rotate_matches_ts_shape_and_defaults_origin() {
        let json = r#"{"kind":"Rotate","axis":[0,0,1],"degrees":90,"shape":{"kind":"Rect","width":1,"height":1}}"#;
        let spec: ModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec,
            ModelSpec::Rotate {
                axis: [0.0, 0.0, 1.0],
                degrees: 90.0,
                origin: [0.0; 3],
                shape: Box::new(ModelSpec::Rect {
                    width: 1.0,
                    height: 1.0
                }),
            }
        );
    }

    #[test]
    fn polygon_matches_ts_shape() {
        let json = r#"{"kind":"Polygon","points":[[0,0],[10,0],[0,5]]}"#;
        let spec: ModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec,
            ModelSpec::Polygon {
                points: vec![[0.0, 0.0], [10.0, 0.0], [0.0, 5.0]]
            }
        );
    }

    #[test]
    fn booleans_match_ts_shape() {
        let rect = r#"{"kind":"Rect","width":1,"height":1}"#;
        let json = format!(r#"{{"kind":"Difference","shape":{rect},"tools":[{rect},{rect}]}}"#);
        let ModelSpec::Difference { tools, .. } = serde_json::from_str(&json).unwrap() else {
            panic!("not a difference")
        };
        assert_eq!(tools.len(), 2);
        let json = format!(r#"{{"kind":"Union","children":[{rect}]}}"#);
        assert!(matches!(serde_json::from_str(&json).unwrap(), ModelSpec::Union { .. }));
        let json = format!(r#"{{"kind":"Intersect","children":[{rect}]}}"#);
        assert!(matches!(serde_json::from_str(&json).unwrap(), ModelSpec::Intersect { .. }));
    }

    #[test]
    fn circle_and_cutout_match_ts_shape() {
        let json = r#"{"kind":"Cutout","outer":{"kind":"Circle","radius":10},"holes":[{"kind":"Circle","radius":4}]}"#;
        let spec: ModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec,
            ModelSpec::Cutout {
                outer: Box::new(ModelSpec::Circle { radius: 10.0 }),
                holes: vec![ModelSpec::Circle { radius: 4.0 }],
            }
        );
    }

    #[test]
    fn sweeps_match_ts_shape_with_defaults() {
        let rect = r#"{"kind":"Rect","width":1,"height":1}"#;
        let json = format!(r#"{{"kind":"Extrude","profile":{rect},"distance":2,"direction":[1,0,0]}}"#);
        let ModelSpec::Extrude { direction, .. } = serde_json::from_str(&json).unwrap() else { panic!() };
        assert_eq!(direction, Some([1.0, 0.0, 0.0]));
        let json = format!(r#"{{"kind":"Revolve","profile":{rect}}}"#);
        let ModelSpec::Revolve { degrees, .. } = serde_json::from_str(&json).unwrap() else { panic!() };
        assert_eq!(degrees, 360.0);
    }

    #[test]
    fn primitives_match_ts_shape() {
        let cases = [
            (r#"{"kind":"Sphere","radius":5}"#, ModelSpec::Sphere { radius: 5.0 }),
            (
                r#"{"kind":"Cylinder","radius":2,"height":7}"#,
                ModelSpec::Cylinder { radius: 2.0, height: 7.0 },
            ),
            (
                r#"{"kind":"Cone","radius":3,"top":0,"height":4}"#,
                ModelSpec::Cone {
                    radius: 3.0,
                    top: 0.0,
                    height: 4.0,
                },
            ),
            (
                r#"{"kind":"Torus","radius":10,"tube":2}"#,
                ModelSpec::Torus { radius: 10.0, tube: 2.0 },
            ),
        ];
        for (json, spec) in cases {
            assert_eq!(serde_json::from_str::<ModelSpec>(json).unwrap(), spec, "{json}");
        }
    }

    #[test]
    fn fillet_and_chamfer_match_ts_shape() {
        let rect = r#"{"kind":"Rect","width":1,"height":1}"#;
        let json = format!(r#"{{"kind":"Fillet","shape":{rect},"radius":2,"edges":["|Z",">Z","all"]}}"#);
        let ModelSpec::Fillet { radius, edges, .. } = serde_json::from_str(&json).unwrap() else { panic!() };
        assert_eq!(radius, 2.0);
        assert_eq!(edges, vec![EdgeSelector::ParallelZ, EdgeSelector::MaxZ, EdgeSelector::All]);
        let json = format!(r#"{{"kind":"Chamfer","shape":{rect},"distance":1}}"#);
        let ModelSpec::Chamfer { edges, .. } = serde_json::from_str(&json).unwrap() else { panic!() };
        assert!(edges.is_empty());
        let bad = format!(r#"{{"kind":"Chamfer","shape":{rect},"distance":1,"edges":["^Z"]}}"#);
        assert!(serde_json::from_str::<ModelSpec>(&bad).is_err());
    }

    #[test]
    fn part_matches_ts_shape() {
        let json = r#"{"kind":"Part","name":"leg","shape":{"kind":"Rect","width":1,"height":1}}"#;
        let spec: ModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec,
            ModelSpec::Part {
                name: "leg".into(),
                shape: Box::new(ModelSpec::Rect {
                    width: 1.0,
                    height: 1.0
                }),
            }
        );
    }

    #[test]
    fn arrange_matches_ts_shape() {
        let json = r#"{"kind":"Arrange","gap":10,"children":[{"kind":"Rect","width":1,"height":1}]}"#;
        let spec: ModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec,
            ModelSpec::Arrange {
                gap: 10.0,
                children: vec![ModelSpec::Rect {
                    width: 1.0,
                    height: 1.0
                }],
                bed: None,
            }
        );

        let json = r#"{"kind":"Arrange","gap":5,"children":[],"bed":{"width":256,"depth":250,"height":200}}"#;
        let ModelSpec::Arrange { bed, .. } = serde_json::from_str(json).unwrap() else {
            panic!("not an arrange")
        };
        assert_eq!(
            bed,
            Some(Bed {
                width: 256.0,
                depth: 250.0,
                height: 200.0
            })
        );
    }

    #[test]
    fn diagnostics_without_warning_flag_still_parse() {
        let d: Diagnostic = serde_json::from_str(r#"{"ok":true,"message":"hi"}"#).unwrap();
        assert_eq!(d, Diagnostic::ok("hi"));
        assert!(Diagnostic::warning("w").ok && Diagnostic::warning("w").warning);
        assert!(!Diagnostic::error("e").ok && !Diagnostic::error("e").warning);
    }

    fn unit_rect() -> ModelSpec {
        ModelSpec::Rect {
            width: 1.0,
            height: 1.0,
        }
    }

    #[test]
    fn bare_spec_is_assembled_only() {
        let value = serde_json::json!({ "kind": "Rect", "width": 1, "height": 1 });
        let views = ModelViews::from_script_value(value).unwrap();
        assert_eq!(views, ModelViews::assembled_only(unit_rect()));
    }

    #[test]
    fn views_object_with_and_without_print() {
        let rect = serde_json::json!({ "kind": "Rect", "width": 1, "height": 1 });

        let both = serde_json::json!({ "assembled": rect, "print": rect });
        let views = ModelViews::from_script_value(both).unwrap();
        assert_eq!(views.print, Some(unit_rect()));

        let assembled_only = serde_json::json!({ "assembled": rect });
        let views = ModelViews::from_script_value(assembled_only).unwrap();
        assert_eq!(views, ModelViews::assembled_only(unit_rect()));
    }

    #[test]
    fn view_errors_point_at_the_bad_field() {
        let bad = serde_json::json!({
            "assembled": { "kind": "Rect", "width": 1, "height": 1 },
            "print": { "kind": "Helix" }
        });
        let err = ModelViews::from_script_value(bad).unwrap_err().to_string();
        assert!(err.contains("Helix"), "{err}");

        let neither = serde_json::json!({ "shape": 1 });
        let err = ModelViews::from_script_value(neither).unwrap_err().to_string();
        assert!(err.contains("kind"), "{err}");
    }

    #[test]
    fn extrude_spec_round_trips_ts_shape() {
        let json = r#"{"kind":"Extrude","profile":{"kind":"Rect","width":10,"height":10},"distance":10}"#;
        let spec: ModelSpec = serde_json::from_str(json).unwrap();
        assert_eq!(
            spec,
            ModelSpec::Extrude {
                profile: Box::new(ModelSpec::Rect {
                    width: 10.0,
                    height: 10.0
                }),
                distance: 10.0,
                direction: None,
            }
        );
    }
}
