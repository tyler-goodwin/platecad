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

        for bad in ["arrange([])", "arrange([cube(1)], { gap: -1 })"] {
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
    /// looking through groups and colors.
    fn board_z(spec: &ModelSpec) -> Vec<f64> {
        match spec {
            ModelSpec::Group { children } => children.iter().flat_map(board_z).collect(),
            ModelSpec::Color { shape, .. } => board_z(shape),
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
