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
    /// Sweeps a 2D `profile` along +Z by `distance` into a solid.
    Extrude {
        profile: Box<ModelSpec>,
        distance: f64,
    },
    /// Moves `shape` by `offset` (2D profiles stay profiles; solids stay solids).
    Translate {
        offset: [f64; 3],
        shape: Box<ModelSpec>,
    },
    /// Several independent shapes shown together (no boolean union yet).
    Group { children: Vec<ModelSpec> },
    /// Paints `shape`. The innermost `Color` wins; a colored profile colors its extrusion.
    Color { rgb: Rgb, shape: Box<ModelSpec> },
    /// Lays children out for printing: each is moved so its bounding box sits on
    /// the plate (z = 0, y = 0), side by side along +X with `gap` between them.
    /// Orientation is left to the model.
    Arrange { gap: f64, children: Vec<ModelSpec> },
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
    /// Optional line-list of boundary samples (pairs of points).
    #[serde(default)]
    pub edges: Vec<[f32; 3]>,
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
    pub ok: bool,
    pub message: String,
}

impl Diagnostic {
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: message.into(),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            }
        );
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
            "print": { "kind": "Sphere" }
        });
        let err = ModelViews::from_script_value(bad).unwrap_err().to_string();
        assert!(err.contains("Sphere"), "{err}");

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
            }
        );
    }
}
