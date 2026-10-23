use serde::{Deserialize, Serialize};

/// Kernel-agnostic model description produced by the TypeScript DSL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ModelSpec {
    Rect { width: f64, height: f64 },
}

/// Triangle mesh the viewer can upload without knowing the kernel.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TessellatedScene {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
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
}
