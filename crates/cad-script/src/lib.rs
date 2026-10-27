use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Once;

use cad_ir::ModelViews;
use rustyscript::deno_core::error::ModuleLoaderError;
use rustyscript::deno_core::{ModuleSpecifier, RequestedModuleType, ResolutionKind};
use rustyscript::module_loader::ImportProvider;
use rustyscript::{Error as RsError, Module, ModuleHandle, Runtime, RuntimeOptions, json_args};
use thiserror::Error;

static INIT_V8: Once = Once::new();

#[derive(Debug, Error)]
pub enum ScriptError {
    #[error("{0}")]
    Runtime(String),
    #[error("failed to read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("script did not export default or model()")]
    MissingExport,
    #[error("model export is not a shape or {{ assembled, print }}: {0}")]
    BadExport(String),
}

impl From<RsError> for ScriptError {
    fn from(value: RsError) -> Self {
        Self::Runtime(value.to_string())
    }
}

/// Evaluates a model script's default export (or `model()`): either a bare shape
/// (assembled-only) or `{ assembled, print? }`.
pub trait ScriptHost {
    fn eval_source(&mut self, filename: &str, source: &str) -> Result<ModelViews, ScriptError>;
    fn eval_file(&mut self, path: &Path) -> Result<ModelViews, ScriptError>;

    /// Files the last eval read or tried to read (the model file and every
    /// import, including ones that failed to load), whether or not it succeeded.
    /// This is what hot reload should watch.
    fn last_dependencies(&self) -> &BTreeSet<PathBuf>;
}

/// Host-side file reads for relative TS imports (`../dsl/cad.ts`), limited to
/// files under `root`. The JS sandbox stays closed; only the Rust loader touches disk.
struct HostFsImports {
    root: PathBuf,
    /// Every file `import` was asked for during this eval.
    requested: Rc<RefCell<BTreeSet<PathBuf>>>,
}

impl HostFsImports {
    fn allowed_path(&self, specifier: &ModuleSpecifier) -> Option<Result<PathBuf, ModuleLoaderError>> {
        if specifier.scheme() != "file" {
            return None;
        }
        let path = match specifier.to_file_path() {
            Ok(path) => path,
            Err(()) => return Some(Err(ModuleLoaderError::generic(format!("bad file url {specifier}")))),
        };
        if path.starts_with(&self.root) {
            Some(Ok(path))
        } else {
            Some(Err(ModuleLoaderError::generic(format!(
                "import {} is outside {}",
                path.display(),
                self.root.display()
            ))))
        }
    }
}

impl ImportProvider for HostFsImports {
    fn resolve(
        &mut self,
        specifier: &ModuleSpecifier,
        _referrer: &str,
        _kind: ResolutionKind,
    ) -> Option<Result<ModuleSpecifier, ModuleLoaderError>> {
        self.allowed_path(specifier)
            .map(|result| result.map(|_| specifier.clone()))
    }

    fn import(
        &mut self,
        specifier: &ModuleSpecifier,
        _referrer: Option<&ModuleSpecifier>,
        _is_dyn_import: bool,
        _requested_module_type: RequestedModuleType,
    ) -> Option<Result<String, ModuleLoaderError>> {
        self.allowed_path(specifier).map(|result| {
            let path = result?;
            // Record before reading: a missing import should reload once it's created.
            self.requested.borrow_mut().insert(path.clone());
            std::fs::read_to_string(&path).map_err(|e| {
                ModuleLoaderError::generic(format!("failed to read {}: {e}", path.display()))
            })
        })
    }
}

/// Evaluates model scripts with rustyscript. Each eval gets a fresh `Runtime`
/// so edited modules are never served from V8's module map (hot reload).
pub struct RustyScriptHost {
    import_root: PathBuf,
    dependencies: BTreeSet<PathBuf>,
}

impl RustyScriptHost {
    /// `import_root` bounds which files scripts may import (e.g. the repo root
    /// containing `dsl/` and `models/`).
    pub fn new(import_root: impl AsRef<Path>) -> Result<Self, ScriptError> {
        INIT_V8.call_once(|| {
            rustyscript::init_platform(0, false);
        });
        let import_root = absolute(import_root.as_ref())?;
        Ok(Self {
            import_root,
            dependencies: BTreeSet::new(),
        })
    }

    /// Runs `module`, recording `entry` (the module's own file, if any) and every
    /// import it requests into `self.dependencies`.
    fn eval_module(&mut self, module: &Module, entry: Option<PathBuf>) -> Result<ModelViews, ScriptError> {
        let requested = Rc::new(RefCell::new(BTreeSet::from_iter(entry)));
        let result = (|| {
            let mut runtime = Runtime::new(RuntimeOptions {
                import_provider: Some(Box::new(HostFsImports {
                    root: self.import_root.clone(),
                    requested: requested.clone(),
                })),
                ..Default::default()
            })?;
            let handle = runtime.load_module(module)?;
            call_model(&mut runtime, &handle)
        })();
        self.dependencies = requested.take();
        result
    }
}

fn call_model(runtime: &mut Runtime, handle: &ModuleHandle) -> Result<ModelViews, ScriptError> {
    for name in ["default", "model"] {
        match runtime.call_function::<serde_json::Value>(Some(handle), name, json_args!()) {
            Ok(value) => {
                return ModelViews::from_script_value(value)
                    .map_err(|e| ScriptError::BadExport(e.to_string()));
            }
            Err(RsError::ValueNotFound(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Err(ScriptError::MissingExport)
}

/// Absolute path with `.`/`..` folded lexically, so it compares cleanly against
/// the normalized paths inside `file://` specifiers.
fn absolute(path: &Path) -> Result<PathBuf, ScriptError> {
    use std::path::Component;
    let abs = std::path::absolute(path).map_err(|source| ScriptError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let mut out = PathBuf::new();
    for component in abs.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

impl ScriptHost for RustyScriptHost {
    fn eval_source(&mut self, filename: &str, source: &str) -> Result<ModelViews, ScriptError> {
        let module = Module::new(self.import_root.join(filename), source);
        self.eval_module(&module, None)
    }

    fn eval_file(&mut self, path: &Path) -> Result<ModelViews, ScriptError> {
        let path = absolute(path)?;
        let module = match Module::load(&path) {
            Ok(module) => module,
            Err(source) => {
                // Still a dependency: creating the file should trigger a reload.
                self.dependencies = BTreeSet::from([path.clone()]);
                return Err(ScriptError::Io { path, source });
            }
        };
        self.eval_module(&module, Some(path))
    }

    fn last_dependencies(&self) -> &BTreeSet<PathBuf> {
        &self.dependencies
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cad_ir::ModelSpec;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    fn host() -> RustyScriptHost {
        RustyScriptHost::new(repo_root()).unwrap()
    }

    #[test]
    fn eval_inline_rect_spec() {
        let spec = host()
            .eval_source(
                "inline.ts",
                r#"
                export default function model() {
                    return { kind: "Rect", width: 10, height: 10 };
                }
                "#,
            )
            .unwrap().assembled;
        assert_eq!(
            spec,
            ModelSpec::Rect {
                width: 10.0,
                height: 10.0
            }
        );
    }

    // The files under models/ are meant to be edited live, so only check their shape.
    #[test]
    fn eval_square_model_file() {
        let spec = host()
            .eval_file(&repo_root().join("models/square.ts"))
            .unwrap().assembled;
        assert!(matches!(spec, ModelSpec::Rect { .. }), "{spec:?}");
    }

    #[test]
    fn eval_cube_model_file() {
        let spec = host()
            .eval_file(&repo_root().join("models/cube.ts"))
            .unwrap().assembled;
        assert!(matches!(spec, ModelSpec::Extrude { .. }), "{spec:?}");
    }

    #[test]
    fn dsl_cube_extrudes_square_profile() {
        let spec = host()
            .eval_source(
                "cube.ts",
                r#"
                import { cube } from "./dsl/cad.ts";
                export default () => cube(10);
                "#,
            )
            .unwrap().assembled;
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

    #[test]
    fn dsl_box_maps_width_depth_height_to_xyz() {
        let spec = host()
            .eval_source(
                "box.ts",
                r#"
                import { box } from "./dsl/cad.ts";
                export default () => box({ width: 1, depth: 2, height: 3 });
                "#,
            )
            .unwrap().assembled;
        assert_eq!(
            spec,
            ModelSpec::Extrude {
                profile: Box::new(ModelSpec::Rect {
                    width: 1.0,
                    height: 2.0
                }),
                distance: 3.0,
                direction: None,
            }
        );
    }

    #[test]
    fn dsl_rejects_non_positive_extrude() {
        let err = host()
            .eval_source(
                "bad_extrude.ts",
                r#"
                import { extrude, rect } from "./dsl/cad.ts";
                export default () => extrude(rect({ width: 1, height: 1 }), { distance: 0 });
                "#,
            )
            .unwrap_err();
        assert!(err.to_string().contains("extrude distance must be positive"), "{err}");
    }

    fn eval(source: &str) -> Result<ModelSpec, ScriptError> {
        eval_views(source).map(|views| views.assembled)
    }

    fn eval_views(source: &str) -> Result<ModelViews, ScriptError> {
        host().eval_source("test_model.ts", source)
    }

    #[test]
    fn default_export_can_provide_print_view() {
        let views = eval_views(
            r#"
            import { arrange, cube } from "./dsl/cad.ts";
            export default () => ({ assembled: cube(1), print: arrange([cube(1)], { gap: 3 }) });
            "#,
        )
        .unwrap();
        assert_eq!(views.assembled, unit_cube());
        assert_eq!(
            views.print,
            Some(ModelSpec::Arrange {
                gap: 3.0,
                children: vec![unit_cube()],
                bed: None,
            })
        );
    }

    #[test]
    fn bare_shape_export_has_no_print_view() {
        let views = eval_views(
            r#"import { cube } from "./dsl/cad.ts"; export default () => cube(1);"#,
        )
        .unwrap();
        assert_eq!(views, ModelViews::assembled_only(unit_cube()));
    }

    #[test]
    fn dsl_arrange_accepts_an_explicit_bed() {
        let views = eval_views(
            r#"import { arrange, cube } from "./dsl/cad.ts";
            export default () => ({
                assembled: cube(1),
                print: arrange([cube(1)], { bed: { width: 180, depth: 180, height: 180 } }),
            });"#,
        )
        .unwrap();
        let Some(ModelSpec::Arrange { bed, .. }) = views.print else {
            panic!("{views:?}")
        };
        assert_eq!(
            bed,
            Some(cad_ir::Bed {
                width: 180.0,
                depth: 180.0,
                height: 180.0
            })
        );
    }

    #[test]
    fn malformed_export_is_reported() {
        let err = eval_views(r#"export default () => ({ assembled: 42 });"#).unwrap_err();
        assert!(matches!(err, ScriptError::BadExport(_)), "{err}");
        assert!(err.to_string().contains("assembled, print"), "{err}");
    }

    #[test]
    fn dsl_arrange_defaults_gap_and_validates() {
        let views = eval_views(
            r#"import { arrange, cube } from "./dsl/cad.ts";
            export default () => ({ assembled: cube(1), print: arrange([cube(1)]) });"#,
        )
        .unwrap();
        assert!(
            matches!(views.print, Some(ModelSpec::Arrange { gap, .. }) if gap == 10.0),
            "{views:?}"
        );

        for bad in [
            "arrange([])",
            "arrange([cube(1)], { gap: -1 })",
            "arrange([cube(1)], { bed: { width: 0, depth: 1, height: 1 } })",
        ] {
            let source = format!(
                r#"import {{ arrange, cube }} from "./dsl/cad.ts";
                export default () => {bad};"#
            );
            assert!(eval_views(&source).is_err(), "{bad} accepted");
        }
    }

    fn unit_cube() -> ModelSpec {
        ModelSpec::Extrude {
            profile: Box::new(ModelSpec::Rect {
                width: 1.0,
                height: 1.0,
            }),
            distance: 1.0,
            direction: None,
        }
    }

    #[test]
    fn dsl_translate_and_group() {
        let spec = eval(
            r#"
            import { cube, group, translate } from "./dsl/cad.ts";
            export default () => group(cube(1), translate(cube(1), [2, 0, 0]));
            "#,
        )
        .unwrap();
        assert_eq!(
            spec,
            ModelSpec::Group {
                children: vec![
                    unit_cube(),
                    ModelSpec::Translate {
                        offset: [2.0, 0.0, 0.0],
                        shape: Box::new(unit_cube()),
                    },
                ],
            }
        );
    }

    #[test]
    fn dsl_rotate_accepts_axis_names_and_vectors() {
        for (axis, want) in [("\"x\"", [1.0, 0.0, 0.0]), ("\"z\"", [0.0, 0.0, 1.0]), ("[1, 1, 0]", [1.0, 1.0, 0.0])] {
            let source = format!(
                r#"import {{ cube, rotate }} from "./dsl/cad.ts";
                export default () => rotate(cube(1), {{ axis: {axis}, degrees: 45, origin: [1, 2, 3] }});"#
            );
            assert_eq!(
                eval(&source).unwrap(),
                ModelSpec::Rotate {
                    axis: want,
                    degrees: 45.0,
                    origin: [1.0, 2.0, 3.0],
                    shape: Box::new(unit_cube()),
                },
                "{axis}"
            );
        }
    }

    #[test]
    fn dsl_rotate_rejects_bad_input() {
        for opts in [
            r#"{ axis: "w", degrees: 90 }"#,
            r#"{ axis: [0, 0, 0], degrees: 90 }"#,
            r#"{ axis: [0, 1], degrees: 90 }"#,
            r#"{ axis: "z", degrees: NaN }"#,
            r#"{ axis: "z", degrees: 90, origin: [0, Infinity, 0] }"#,
        ] {
            let source = format!(
                r#"import {{ cube, rotate }} from "./dsl/cad.ts";
                export default () => rotate(cube(1), {opts});"#
            );
            let err = eval(&source).unwrap_err().to_string();
            assert!(err.contains("rotate"), "{opts}: {err}");
        }
    }

    #[test]
    fn dsl_part_names_shapes() {
        let spec = eval(
            r#"import { cube, part } from "./dsl/cad.ts";
            export default () => part("  leg ", cube(1));"#,
        )
        .unwrap();
        assert_eq!(
            spec,
            ModelSpec::Part {
                name: "leg".into(),
                shape: Box::new(unit_cube()),
            }
        );

        for bad in [r#"part("", cube(1))"#, r#"part("  ", cube(1))"#, "part(7, cube(1))"] {
            let source = format!(
                r#"import {{ cube, part }} from "./dsl/cad.ts";
                export default () => {bad};"#
            );
            let err = eval(&source).unwrap_err().to_string();
            assert!(err.contains("part name"), "{bad}: {err}");
        }
    }

    #[test]
    fn dsl_polygon_passes_points_through_and_validates() {
        let spec = eval(
            r#"import { polygon } from "./dsl/cad.ts";
            export default () => polygon([[0, 0], [4, 0], [0, 3]]);"#,
        )
        .unwrap();
        assert_eq!(
            spec,
            ModelSpec::Polygon {
                points: vec![[0.0, 0.0], [4.0, 0.0], [0.0, 3.0]]
            }
        );
        for bad in ["polygon([[0, 0], [1, 0]])", "polygon([[0, 0], [1, 0], [0, NaN]])", "polygon([[0, 0], [1], [0, 1]])"] {
            let source = format!(
                r#"import {{ polygon }} from "./dsl/cad.ts";
                export default () => {bad};"#
            );
            let err = eval(&source).unwrap_err().to_string();
            assert!(err.contains("polygon"), "{bad}: {err}");
        }
    }

    #[test]
    fn dsl_booleans() {
        let spec = eval(
            r#"import { cube, difference, intersect, union } from "./dsl/cad.ts";
            export default () => difference(union(cube(1), cube(1)), intersect(cube(1)), cube(1));"#,
        )
        .unwrap();
        let ModelSpec::Difference { shape, tools } = spec else { panic!("{spec:?}") };
        assert!(matches!(*shape, ModelSpec::Union { ref children } if children.len() == 2));
        assert!(matches!(tools[0], ModelSpec::Intersect { ref children } if children.len() == 1));
        assert_eq!(tools[1], unit_cube());

        for bad in ["union()", "intersect()", "difference(cube(1))"] {
            let source = format!(
                r#"import {{ cube, difference, intersect, union }} from "./dsl/cad.ts";
                export default () => {bad};"#
            );
            let err = eval(&source).unwrap_err().to_string();
            assert!(err.contains("needs at least one"), "{bad}: {err}");
        }
    }

    #[test]
    fn dsl_circle_and_cutout() {
        let spec = eval(
            r#"import { circle, cutout } from "./dsl/cad.ts";
            export default () => cutout(circle(10), circle(2, { at: [3, 4] }));"#,
        )
        .unwrap();
        assert_eq!(
            spec,
            ModelSpec::Cutout {
                outer: Box::new(ModelSpec::Circle { radius: 10.0 }),
                holes: vec![ModelSpec::Translate {
                    offset: [3.0, 4.0, 0.0],
                    shape: Box::new(ModelSpec::Circle { radius: 2.0 }),
                }],
            }
        );
        for (bad, message) in [("circle(0)", "circle radius"), ("cutout(circle(1))", "at least one hole")] {
            let source = format!(
                r#"import {{ circle, cutout }} from "./dsl/cad.ts";
                export default () => {bad};"#
            );
            let err = eval(&source).unwrap_err().to_string();
            assert!(err.contains(message), "{bad}: {err}");
        }
    }

    #[test]
    fn dsl_sweeps() {
        let spec = eval(
            r#"import { extrude, rect } from "./dsl/cad.ts";
            export default () => extrude(rect({ width: 1, height: 1 }), { distance: 2, direction: [0, 0, -3] });"#,
        )
        .unwrap();
        assert!(matches!(spec, ModelSpec::Extrude { direction: Some([0.0, 0.0, -3.0]), .. }), "{spec:?}");

        let spec = eval(
            r#"import { revolve, rect } from "./dsl/cad.ts";
            export default () => revolve(rect({ width: 1, height: 1 }));"#,
        )
        .unwrap();
        assert!(matches!(spec, ModelSpec::Revolve { degrees, .. } if degrees == 360.0), "{spec:?}");

        for bad in [
            "extrude(rect({ width: 1, height: 1 }), { distance: 1, direction: [0, 0, 0] })",
            "revolve(rect({ width: 1, height: 1 }), { degrees: 0 })",
            "revolve(rect({ width: 1, height: 1 }), { degrees: 400 })",
        ] {
            let source = format!(
                r#"import {{ extrude, revolve, rect }} from "./dsl/cad.ts";
                export default () => {bad};"#
            );
            assert!(eval(&source).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn dsl_primitives() {
        let cases = [
            ("sphere(5)", ModelSpec::Sphere { radius: 5.0 }),
            ("cylinder({ radius: 2, height: 7 })", ModelSpec::Cylinder { radius: 2.0, height: 7.0 }),
            (
                "cone({ radius: 3, height: 4 })",
                ModelSpec::Cone {
                    radius: 3.0,
                    top: 0.0,
                    height: 4.0,
                },
            ),
            ("torus({ radius: 10, tube: 2 })", ModelSpec::Torus { radius: 10.0, tube: 2.0 }),
        ];
        let source = |expr: &str| {
            format!(
                r#"import {{ sphere, cylinder, cone, torus }} from "./dsl/cad.ts";
                export default () => {expr};"#
            )
        };
        for (expr, spec) in cases {
            assert_eq!(eval(&source(expr)).unwrap(), spec, "{expr}");
        }

        let placed = eval(&source("sphere(1, { at: [1, 2, 3] })")).unwrap();
        assert_eq!(
            placed,
            ModelSpec::Translate {
                offset: [1.0, 2.0, 3.0],
                shape: Box::new(ModelSpec::Sphere { radius: 1.0 }),
            }
        );

        for (bad, why) in [
            ("sphere(0)", "sphere radius must be positive"),
            ("sphere(1, { at: [1, 2] })", "sphere at must be"),
            ("cylinder({ radius: 1, height: -2 })", "cylinder height must be positive"),
            ("cone({ radius: -1, height: 2 })", "cone radius and top must be 0 or more"),
            ("cone({ radius: 0, height: 2 })", "cone needs a radius or top"),
            ("torus({ radius: 2, tube: 2 })", "torus radius must be larger than its tube"),
        ] {
            let err = eval(&source(bad)).unwrap_err().to_string();
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn dsl_fillet_and_chamfer() {
        use cad_ir::EdgeSelector;
        let spec = eval(
            r#"import { chamfer, cube, fillet } from "./dsl/cad.ts";
            export default () => chamfer(fillet(cube(1), 0.2, { edges: ["|Z", ">Z"] }), 0.1, { edges: "<Z" });"#,
        )
        .unwrap();
        let ModelSpec::Chamfer { shape, distance, edges } = spec else { panic!("{spec:?}") };
        assert_eq!((distance, edges), (0.1, vec![EdgeSelector::MinZ]));
        assert!(
            matches!(*shape, ModelSpec::Fillet { radius, ref edges, .. }
                if radius == 0.2 && *edges == vec![EdgeSelector::ParallelZ, EdgeSelector::MaxZ]),
            "{shape:?}"
        );
        for bad in ["fillet(cube(1), 0)", "chamfer(cube(1), 1, { edges: \"^Z\" })"] {
            let source = format!(
                r#"import {{ chamfer, cube, fillet }} from "./dsl/cad.ts";
                export default () => {bad};"#
            );
            assert!(eval(&source).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn dsl_pipeline_matches_nested_calls() {
        let source = |expr: &str| {
            format!(
                r##"import {{ color, cube, difference, extrude, fillet, part, pipeline, rect, step, translate, union }} from "./dsl/cad.ts";
                export default () => {expr};"##
            )
        };
        let piped = eval(&source(
            r##"pipeline(
                rect({ width: 2, height: 1 }),
                step.extrude({ distance: 3 }),
                step.fillet(0.2, { edges: "|Z" }),
                step.cut(cube(0.5)),
                step.add(cube(1)),
                step.translate([1, 2, 3]),
                (s) => part("plate", s),
                step.color("#ff0000"),
            )"##,
        ))
        .unwrap();
        let nested = eval(&source(
            r##"color(part("plate", translate(union(difference(
                fillet(extrude(rect({ width: 2, height: 1 }), { distance: 3 }), 0.2, { edges: "|Z" }),
                cube(0.5)), cube(1)), [1, 2, 3])), "#ff0000")"##,
        ))
        .unwrap();
        assert_eq!(piped, nested);

        // No steps returns the shape unchanged.
        assert_eq!(eval(&source("pipeline(cube(1))")).unwrap(), unit_cube());

        for (bad, why) in [
            ("pipeline(cube(1), 3)", "pipeline step 1 must be a function"),
            ("pipeline(cube(1), step.translate([1, 0, 0]), () => 1)", "pipeline step 2 must return a shape"),
            ("pipeline(cube(1), step.fillet(0))", "fillet radius must be positive"),
            ("pipeline(cube(1), step.cut())", "difference needs at least one tool"),
        ] {
            let err = eval(&source(bad)).unwrap_err().to_string();
            assert!(err.contains(why), "{bad}: {err}");
        }
    }

    #[test]
    fn dsl_box_at_wraps_in_translate() {
        let spec = eval(
            r#"
            import { box } from "./dsl/cad.ts";
            export default () => box({ width: 1, depth: 1, height: 1, at: [1, 2, 3] });
            "#,
        )
        .unwrap();
        assert_eq!(
            spec,
            ModelSpec::Translate {
                offset: [1.0, 2.0, 3.0],
                shape: Box::new(unit_cube()),
            }
        );
    }

    #[test]
    fn dsl_rejects_empty_group_and_bad_offsets() {
        let err = eval(r#"import { group } from "./dsl/cad.ts"; export default () => group();"#)
            .unwrap_err();
        assert!(err.to_string().contains("at least one child"), "{err}");

        let err = eval(
            r#"import { cube, translate } from "./dsl/cad.ts";
            export default () => translate(cube(1), [0, NaN, 0]);"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("must be finite"), "{err}");
    }

    /// Bottom-face Z of each board (a translated box) in depth-first order,
    /// looking through groups, colors and part names.
    fn board_z(spec: &ModelSpec) -> Vec<f64> {
        match spec {
            ModelSpec::Group { children } => children.iter().flat_map(board_z).collect(),
            ModelSpec::Color { shape, .. } | ModelSpec::Part { shape, .. } => board_z(shape),
            ModelSpec::Translate { offset, .. } => vec![offset[2]],
            other => panic!("expected translated board, got {other:?}"),
        }
    }

    #[test]
    fn bookshelf_spaces_shelves_evenly() {
        let spec = eval(
            r#"
            import { bookshelf } from "./models/bookshelf.ts";
            export default () => bookshelf({ height: 100, thickness: 2, shelves: 2 });
            "#,
        )
        .unwrap();
        // left, right, bottom, top, shelf 1, shelf 2, back
        let z = board_z(&spec);
        assert_eq!(z.len(), 7);
        let (bottom, top, shelves) = (z[2], z[3], &z[4..6]);
        assert_eq!((bottom, top), (0.0, 98.0));

        // Clear gap above each board (its top is z + thickness) is the same everywhere.
        let stack = [bottom, shelves[0], shelves[1], top];
        let gaps: Vec<f64> = stack.windows(2).map(|w| w[1] - (w[0] + 2.0)).collect();
        for gap in &gaps {
            assert!((gap - gaps[0]).abs() < 1e-9, "{gaps:?}");
        }
        assert!((gaps[0] - 92.0 / 3.0).abs() < 1e-9, "{gaps:?}");
    }

    #[test]
    fn bookshelf_rejects_impossible_params() {
        let err = eval(
            r#"import { bookshelf } from "./models/bookshelf.ts";
            export default () => bookshelf({ height: 20, thickness: 2, shelves: 9 });"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("don't fit"), "{err}");

        let err = eval(
            r#"import { bookshelf } from "./models/bookshelf.ts";
            export default () => bookshelf({ shelves: 1.5 });"#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("whole number"), "{err}");
    }

    fn painted_rgb(source: &str) -> Result<[u8; 3], ScriptError> {
        match eval(source)? {
            ModelSpec::Color { rgb, .. } => Ok(rgb),
            other => panic!("expected color, got {other:?}"),
        }
    }

    #[test]
    fn dsl_color_accepts_hex_and_rgb() {
        for (input, want) in [
            (r##""#b07a4f""##, [176, 122, 79]),
            (r##""#B07A4F""##, [176, 122, 79]),
            (r##""#3a7""##, [0x33, 0xaa, 0x77]),
            ("[0, 128, 255]", [0, 128, 255]),
        ] {
            let source = format!(
                r#"import {{ color, cube }} from "./dsl/cad.ts";
                export default () => color(cube(1), {input});"#
            );
            assert_eq!(painted_rgb(&source).unwrap(), want, "{input}");
        }
    }

    #[test]
    fn dsl_color_rejects_bad_input() {
        for input in [
            r#""red""#,
            r##""#12""##,
            r##""#12345g""##,
            r#""b07a4f""#,
            "[256, 0, 0]",
            "[-1, 0, 0]",
            "[0.5, 0, 0]",
            "[0, 0]",
        ] {
            let source = format!(
                r#"import {{ color, cube }} from "./dsl/cad.ts";
                export default () => color(cube(1), {input});"#
            );
            let err = painted_rgb(&source).unwrap_err().to_string();
            assert!(err.contains("color must be"), "{input}: {err}");
        }
    }

    #[test]
    fn bookshelf_paints_frame_and_back_separately() {
        let spec = eval(
            r##"
            import { bookshelf } from "./models/bookshelf.ts";
            export default () => bookshelf({ wood: "#102030", backing: [1, 2, 3] });
            "##,
        )
        .unwrap();
        let ModelSpec::Group { children } = &spec else {
            panic!("expected group, got {spec:?}")
        };
        let colors: Vec<[u8; 3]> = children
            .iter()
            .map(|c| match c {
                ModelSpec::Color { rgb, .. } => *rgb,
                other => panic!("expected colored part, got {other:?}"),
            })
            .collect();
        assert_eq!(colors, vec![[0x10, 0x20, 0x30], [1, 2, 3]]);
    }

    #[test]
    fn script_errors_surface() {
        let err = host()
            .eval_source(
                "bad.ts",
                r#"
                import { rect } from "./dsl/cad.ts";
                export default () => rect({ width: -1, height: 10 });
                "#,
            )
            .unwrap_err();
        assert!(err.to_string().contains("must be positive"), "{err}");
    }

    #[test]
    fn imports_outside_root_are_rejected() {
        let mut host = RustyScriptHost::new(repo_root().join("models")).unwrap();
        let err = host
            .eval_file(&repo_root().join("models/square.ts"))
            .unwrap_err();
        assert!(err.to_string().contains("is outside"), "{err}");
    }

    /// Dependencies relative to the repo root, as forward-slash strings.
    fn relative_deps(host: &RustyScriptHost) -> Vec<String> {
        let root = absolute(&repo_root()).unwrap();
        host.last_dependencies()
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap_or_else(|_| panic!("{} outside repo", p.display()))
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn records_model_file_and_its_imports() {
        let mut host = host();
        host.eval_file(&repo_root().join("models/bookshelf.ts")).unwrap();
        assert_eq!(relative_deps(&host), ["dsl/cad.ts", "models/bookshelf.ts"]);
    }

    #[test]
    fn records_transitive_imports_from_source() {
        let mut host = host();
        host.eval_source(
            "test_model.ts",
            r#"import { bookshelf } from "./models/bookshelf.ts";
            export default () => bookshelf();"#,
        )
        .unwrap();
        // Inline source has no file of its own; bookshelf.ts pulls in the DSL.
        assert_eq!(relative_deps(&host), ["dsl/cad.ts", "models/bookshelf.ts"]);
    }

    // Note: TS drops imports whose bindings are never used, so such files are
    // never loaded and correctly aren't dependencies. The tests below use theirs.
    #[test]
    fn records_dependencies_of_failed_evals() {
        let mut host = host();
        let err = host
            .eval_source(
                "test_model.ts",
                r#"import { cube } from "./dsl/cad.ts";
                import { nope } from "./models/does_not_exist.ts";
                export default () => nope ?? cube(1);"#,
            )
            .unwrap_err();
        assert!(err.to_string().contains("does_not_exist"), "{err}");
        let deps = relative_deps(&host);
        assert!(deps.contains(&"models/does_not_exist.ts".to_string()), "{deps:?}");
    }

    #[test]
    fn missing_model_file_is_its_own_dependency() {
        let mut host = host();
        let missing = repo_root().join("models/not_yet.ts");
        assert!(host.eval_file(&missing).is_err());
        assert_eq!(relative_deps(&host), ["models/not_yet.ts"]);
    }

    #[test]
    fn dependencies_reset_between_evals() {
        let mut host = host();
        host.eval_file(&repo_root().join("models/bookshelf.ts")).unwrap();
        host.eval_source("plain.ts", "export default () => ({ kind: 'Rect', width: 1, height: 1 });")
            .unwrap();
        assert!(host.last_dependencies().is_empty(), "{:?}", host.last_dependencies());
    }

    #[test]
    fn missing_export_is_reported() {
        let err = host()
            .eval_source("empty.ts", "export const x = 1;")
            .unwrap_err();
        assert!(matches!(err, ScriptError::MissingExport), "{err}");
    }
}
