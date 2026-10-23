//! Reload pipeline behind the `cad-app` binary: TS file -> script host -> kernel
//! -> `SceneUpdate`, re-run whenever watched sources change. Kept windowless so it
//! can be tested headless.

use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use cad_kernel::Kernel;
use cad_kernel_occ::OccKernel;
use cad_kernel_truck::TruckKernel;
use cad_script::{RustyScriptHost, ScriptHost};
use cad_view::{SceneUpdate, SceneViews};
use clap::{Parser, ValueEnum};
use notify::{EventKind, RecursiveMode, Watcher};

pub const DEFAULT_MODEL: &str = "models/cube.ts";
pub const WATCH_DIRS: [&str; 2] = ["models", "dsl"];
pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// A directory containing this file is a project root (imports and watching are scoped to it).
pub const PROJECT_MARKER: &str = "dsl/cad.ts";

/// Edit a TypeScript CAD model and see it hot-reload in a 3D viewport.
#[derive(Debug, Parser)]
#[command(version)]
pub struct Args {
    /// Model script to open. The project root is found by walking up from it
    /// to the directory containing `dsl/cad.ts`.
    #[arg(default_value = DEFAULT_MODEL)]
    pub model: PathBuf,

    /// Geometry kernel.
    #[arg(long, env = "CAD_KERNEL", value_enum, default_value_t = KernelChoice::Truck)]
    pub kernel: KernelChoice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KernelChoice {
    /// truck B-rep kernel (pure Rust).
    Truck,
    /// OpenCascade (stub: not wired yet).
    Occ,
}

/// Absolute, lexically normalized path to an existing model file.
pub fn resolve_model(model: &Path, cwd: &Path) -> Result<PathBuf, String> {
    let path = normalize(&cwd.join(model));
    if path.is_file() {
        Ok(path)
    } else if path.exists() {
        Err(format!("model {} is not a file", path.display()))
    } else {
        Err(format!("model {} not found", path.display()))
    }
}

/// Nearest ancestor of `model` containing [`PROJECT_MARKER`]; for a standalone
/// script outside any project, the script's own directory.
pub fn project_root(model: &Path) -> PathBuf {
    let dir = model.parent().unwrap_or(Path::new("."));
    dir.ancestors()
        .find(|d| d.join(PROJECT_MARKER).is_file())
        .unwrap_or(dir)
        .to_path_buf()
}

/// Directories to watch recursively: the project's `models/` and `dsl/`, plus
/// the model's own directory when it lives elsewhere.
pub fn watch_dirs(root: &Path, model: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = WATCH_DIRS
        .iter()
        .map(|d| root.join(d))
        .filter(|d| d.is_dir())
        .collect();
    if let Some(dir) = model.parent() {
        if !dirs.iter().any(|d| dir.starts_with(d)) {
            dirs.push(dir.to_path_buf());
        }
    }
    dirs
}

/// Folds `.` and `..` without touching the filesystem (unlike `canonicalize`,
/// which yields `\\?\` paths on Windows that don't match `file://` URLs).
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Script host + kernel: one TS file in, one `SceneUpdate` out.
pub struct Pipeline {
    script: RustyScriptHost,
    kernel: Box<dyn Kernel + Send>,
}

impl Pipeline {
    pub fn new(root: &Path, kernel: KernelChoice) -> Result<Self, String> {
        let kernel: Box<dyn Kernel + Send> = match kernel {
            KernelChoice::Truck => Box::new(TruckKernel),
            KernelChoice::Occ => Box::new(OccKernel),
        };
        let script = RustyScriptHost::new(root).map_err(|e| e.to_string())?;
        Ok(Self { script, kernel })
    }

    pub fn evaluate(&mut self, model: &Path) -> SceneUpdate {
        let started = Instant::now();
        let name = model.file_name().unwrap_or_default().to_string_lossy();
        let views = match self.script.eval_file(model) {
            Ok(views) => views,
            Err(e) => return SceneUpdate::error(format!("{name}: {e}")),
        };
        let kernel = self.kernel.name();
        let assembled = match self.kernel.build(&views.assembled) {
            Ok(scene) => scene,
            Err(e) => return SceneUpdate::error(format!("{name}: {kernel} kernel: {e}")),
        };
        let print = match views.print.map(|spec| self.kernel.build(&spec)).transpose() {
            Ok(scene) => scene,
            Err(e) => {
                return SceneUpdate::error(format!("{name}: {kernel} kernel (print view): {e}"));
            }
        };

        let triangles = |scene: &cad_ir::TessellatedScene| scene.indices.len() / 3;
        let print_note = print
            .as_ref()
            .map(|p| format!("; print {}", triangles(p)))
            .unwrap_or_default();
        let message = format!(
            "{name} OK via {kernel} ({} triangles{print_note}, {} ms)",
            triangles(&assembled),
            started.elapsed().as_millis()
        );
        SceneUpdate::ok(SceneViews { assembled, print }, message)
    }
}

pub fn report(update: &SceneUpdate) {
    for d in &update.diagnostics {
        if d.ok {
            println!("{}", d.message);
        } else {
            eprintln!("{}", d.message);
        }
    }
}

/// Re-evaluates `model` whenever a script in [`watch_dirs`] changes,
/// waiting for `DEBOUNCE` of quiet so editor save bursts cause one rebuild.
/// Returns once the receiving end of `tx` (the viewer) is gone.
pub fn watch_and_reload(
    root: &Path,
    model: &Path,
    mut pipeline: Pipeline,
    tx: Sender<SceneUpdate>,
) -> notify::Result<()> {
    let (events_tx, events_rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(events_tx)?;
    for dir in watch_dirs(root, model) {
        watcher.watch(&dir, RecursiveMode::Recursive)?;
    }

    loop {
        // Block until something relevant changes (or the watcher goes away).
        let Ok(event) = events_rx.recv() else {
            return Ok(());
        };
        if !is_relevant(&event?) {
            continue;
        }
        // Swallow the rest of the burst.
        loop {
            match events_rx.recv_timeout(DEBOUNCE) {
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }

        let update = pipeline.evaluate(model);
        report(&update);
        if tx.send(update).is_err() {
            // Viewer closed.
            return Ok(());
        }
    }
}

pub fn is_relevant(event: &notify::Event) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) && event
        .paths
        .iter()
        .any(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("ts" | "js")))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::mpsc::Receiver;
    use std::thread::{self, JoinHandle};

    use cad_kernel::Kernel;
    use notify::event::{AccessKind, CreateKind, DataChange, ModifyKind};
    use notify::Event;

    use super::*;

    /// Upper bound for a reload to arrive; generous for slow CI file watchers.
    const RELOAD_TIMEOUT: Duration = Duration::from_secs(5);
    /// How long to wait to be sure *no* reload is coming.
    const QUIET: Duration = Duration::from_millis(600);

    /// Temp project with the real `dsl/cad.ts` and a `models/` dir.
    struct Project {
        dir: tempfile::TempDir,
    }

    impl Project {
        fn new(model_source: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let dsl = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dsl/cad.ts");
            std::fs::create_dir_all(dir.path().join("dsl")).unwrap();
            std::fs::create_dir_all(dir.path().join("models")).unwrap();
            std::fs::copy(dsl, dir.path().join("dsl/cad.ts")).unwrap();
            let project = Self { dir };
            project.write_model(model_source);
            project
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn model(&self) -> PathBuf {
            self.root().join("models/model.ts")
        }

        fn write_model(&self, source: &str) {
            std::fs::write(self.model(), source).unwrap();
        }

        fn pipeline(&self) -> Pipeline {
            Pipeline::new(self.root(), KernelChoice::Truck).unwrap()
        }

        /// Starts the watcher thread and waits until it is watching.
        fn watch(&self) -> (Receiver<SceneUpdate>, JoinHandle<notify::Result<()>>) {
            let (tx, rx) = mpsc::channel();
            let (root, model, pipeline) = (self.root().to_path_buf(), self.model(), self.pipeline());
            let handle = thread::spawn(move || watch_and_reload(&root, &model, pipeline, tx));
            // The watcher registers asynchronously; give it a moment before editing.
            thread::sleep(Duration::from_millis(300));
            (rx, handle)
        }
    }

    fn cube_source(size: u32) -> String {
        format!(
            "import {{ cube }} from \"../dsl/cad.ts\";\nexport default () => cube({size});\n"
        )
    }

    fn max_extent(update: &SceneUpdate) -> f32 {
        let views = update.views.as_ref().expect("views");
        let aabb = views.assembled.aabb().unwrap();
        aabb.max.into_iter().fold(f32::MIN, f32::max)
    }

    fn event(kind: EventKind, path: &str) -> Event {
        Event::new(kind).add_path(PathBuf::from(path))
    }

    // --- Pipeline -----------------------------------------------------------

    #[test]
    fn evaluates_model_through_truck() {
        let project = Project::new(&cube_source(10));
        let update = project.pipeline().evaluate(&project.model());

        assert_eq!(update.diagnostics.len(), 1);
        let d = &update.diagnostics[0];
        assert!(d.ok, "{}", d.message);
        assert!(d.message.contains("model.ts OK via truck (12 triangles"), "{}", d.message);
        assert_eq!(max_extent(&update), 10.0);
    }

    #[test]
    fn script_error_keeps_last_mesh() {
        let project = Project::new(&cube_source(0));
        let update = project.pipeline().evaluate(&project.model());

        assert!(update.views.is_none(), "error must not replace the mesh");
        let d = &update.diagnostics[0];
        assert!(!d.ok);
        assert!(d.message.starts_with("model.ts:"), "{}", d.message);
        assert!(d.message.contains("must be positive"), "{}", d.message);
    }

    #[test]
    fn kernel_error_names_the_kernel() {
        let project = Project::new(&cube_source(10));
        let mut pipeline = Pipeline::new(project.root(), KernelChoice::Occ).unwrap();
        let update = pipeline.evaluate(&project.model());

        assert!(update.views.is_none());
        assert!(update.diagnostics[0].message.contains("occ kernel"), "{:?}", update.diagnostics);
    }

    #[test]
    fn missing_model_file_is_an_error() {
        let project = Project::new(&cube_source(10));
        let update = project.pipeline().evaluate(&project.root().join("models/nope.ts"));

        assert!(update.views.is_none());
        assert!(update.diagnostics[0].message.starts_with("nope.ts:"));
    }

    // --- CLI -----------------------------------------------------------------

    fn parse(args: &[&str]) -> Result<Args, clap::Error> {
        Args::try_parse_from(std::iter::once("cad-app").chain(args.iter().copied()))
    }

    #[test]
    fn cli_defaults_to_cube_on_truck() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.model, PathBuf::from(DEFAULT_MODEL));
        // A CAD_KERNEL in the developer's shell legitimately changes the default.
        if std::env::var_os("CAD_KERNEL").is_none() {
            assert_eq!(args.kernel, KernelChoice::Truck);
        }
    }

    #[test]
    fn cli_takes_model_and_kernel() {
        let args = parse(&["models/bookshelf.ts", "--kernel", "occ"]).unwrap();
        assert_eq!(args.model, PathBuf::from("models/bookshelf.ts"));
        assert_eq!(args.kernel, KernelChoice::Occ);
    }

    #[test]
    fn cli_rejects_unknown_kernel_and_extra_args() {
        let err = parse(&["--kernel", "parasolid"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
        assert!(err.to_string().contains("truck"), "should list valid kernels: {err}");

        let err = parse(&["a.ts", "b.ts"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn cli_help_documents_model_kernel_and_env() {
        let err = parse(&["--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = err.to_string();
        for needle in ["[MODEL]", "--kernel", "CAD_KERNEL", "truck", "occ"] {
            assert!(help.contains(needle), "help is missing {needle}:\n{help}");
        }
    }

    #[test]
    fn resolve_model_is_relative_to_cwd() {
        let project = Project::new(&cube_source(10));
        let resolved =
            resolve_model(Path::new("./models/../models/model.ts"), project.root()).unwrap();
        assert_eq!(resolved, project.model());
        assert!(resolved.is_absolute());
        assert!(
            !resolved
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        );
    }

    #[test]
    fn resolve_model_fails_fast() {
        let project = Project::new(&cube_source(10));
        let err = resolve_model(Path::new("models/typo.ts"), project.root()).unwrap_err();
        assert!(err.contains("typo.ts") && err.contains("not found"), "{err}");

        let err = resolve_model(Path::new("models"), project.root()).unwrap_err();
        assert!(err.contains("not a file"), "{err}");
    }

    #[test]
    fn project_root_walks_up_to_dsl() {
        let project = Project::new(&cube_source(10));
        assert_eq!(project_root(&project.model()), project.root());

        let nested = project.root().join("models/parts/legs/leg.ts");
        assert_eq!(project_root(&nested), project.root());

        let elsewhere = project.root().join("scratch/try.ts");
        assert_eq!(project_root(&elsewhere), project.root());
    }

    #[test]
    fn project_root_falls_back_to_script_dir() {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("standalone.ts");
        assert_eq!(project_root(&model), dir.path());
    }

    #[test]
    fn opens_model_from_any_cwd() {
        // What main() does, with the cwd somewhere unrelated to the project.
        let project = Project::new(&cube_source(10));
        let cwd = tempfile::tempdir().unwrap();
        let model = resolve_model(&project.model(), cwd.path()).unwrap();
        let root = project_root(&model);

        let update = Pipeline::new(&root, KernelChoice::Truck)
            .unwrap()
            .evaluate(&model);
        assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
    }

    #[test]
    fn watch_dirs_add_model_dir_only_when_not_covered() {
        let project = Project::new(&cube_source(10));
        let root = project.root();
        let base = vec![root.join("models"), root.join("dsl")];

        assert_eq!(watch_dirs(root, &project.model()), base);
        assert_eq!(watch_dirs(root, &root.join("models/parts/leg.ts")), base);

        let mut expected = base.clone();
        expected.push(root.join("scratch"));
        assert_eq!(watch_dirs(root, &root.join("scratch/try.ts")), expected);
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn every_repo_model_builds() {
        let mut pipeline = Pipeline::new(&repo_root(), KernelChoice::Truck).unwrap();
        let mut count = 0;
        for entry in std::fs::read_dir(repo_root().join("models")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "ts") {
                let update = pipeline.evaluate(&path);
                assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
                let views = update.views.unwrap();
                assert!(!views.assembled.is_empty(), "{}", path.display());
                if let Some(print) = &views.print {
                    assert!(!print.is_empty(), "{} print", path.display());
                    let aabb = print.aabb().unwrap();
                    assert!(aabb.min[2].abs() < 1e-3, "{} print floats", path.display());
                }
                count += 1;
            }
        }
        assert!(count >= 3, "expected square, cube and bookshelf; found {count}");
    }

    /// Bookshelf views through the real script host and truck kernel.
    fn build_bookshelf_views(params: &str) -> SceneViews {
        let mut script = RustyScriptHost::new(repo_root()).unwrap();
        let source = format!(
            "import {{ bookshelf }} from \"./models/bookshelf.ts\";
             export default () => bookshelf({params});"
        );
        let views = script.eval_source("bookshelf_test.ts", &source).unwrap();
        SceneViews {
            assembled: TruckKernel.build(&views.assembled).unwrap(),
            print: Some(TruckKernel.build(&views.print.expect("print view")).unwrap()),
        }
    }

    fn build_bookshelf(params: &str) -> cad_ir::TessellatedScene {
        build_bookshelf_views(params).assembled
    }

    /// Extents (sorted, largest first) of each board, from `per_board`-vertex runs.
    fn board_extents(scene: &cad_ir::TessellatedScene, per_board: usize) -> Vec<[f32; 3]> {
        scene
            .positions
            .chunks(per_board)
            .map(|chunk| {
                let aabb = cad_ir::TessellatedScene {
                    positions: chunk.to_vec(),
                    ..Default::default()
                }
                .aabb()
                .unwrap();
                let mut e = [0.0; 3];
                for i in 0..3 {
                    e[i] = aabb.max[i] - aabb.min[i];
                }
                e.sort_by(|a, b| b.total_cmp(a));
                e
            })
            .collect()
    }

    #[test]
    fn bookshelf_print_view_lays_every_board_flat_on_the_plate() {
        let shelves = 2;
        let views = build_bookshelf_views(&format!("{{ shelves: {shelves}, thickness: 18 }}"));
        let print = views.print.unwrap();
        let boards = 5 + shelves;
        let per_board = print.positions.len() / boards;

        // Same parts, same colors, same triangles; only placement differs.
        assert_eq!(print.positions.len(), views.assembled.positions.len());
        assert_eq!(print.indices.len(), views.assembled.indices.len());
        let mut print_colors = print.colors.clone();
        let mut assembled_colors = views.assembled.colors.clone();
        print_colors.sort();
        assembled_colors.sort();
        assert_eq!(print_colors, assembled_colors);

        let mut previous_max_x = f32::MIN;
        for chunk in print.positions.chunks(per_board) {
            let aabb = cad_ir::TessellatedScene {
                positions: chunk.to_vec(),
                ..Default::default()
            }
            .aabb()
            .unwrap();
            // On the plate, lying on its largest face (thinnest extent is Z)...
            assert!(aabb.min[2].abs() < 1e-3, "{aabb:?}");
            let z = aabb.max[2] - aabb.min[2];
            assert!(z <= aabb.max[0] - aabb.min[0] && z <= aabb.max[1] - aabb.min[1], "{aabb:?}");
            // ...and not overlapping the board before it.
            assert!(aabb.min[0] >= previous_max_x, "{aabb:?}");
            previous_max_x = aabb.max[0];
        }

        // Each board has the same dimensions in both views.
        let mut assembled = board_extents(&views.assembled, per_board);
        let mut printed = board_extents(&print, per_board);
        let by_size = |a: &[f32; 3], b: &[f32; 3]| a.partial_cmp(b).unwrap();
        assembled.sort_by(by_size);
        printed.sort_by(by_size);
        for (a, p) in assembled.iter().zip(&printed) {
            for i in 0..3 {
                assert!((a[i] - p[i]).abs() < 1e-3, "{a:?} vs {p:?}");
            }
        }
    }

    #[test]
    fn bookshelf_defaults_are_millimeters() {
        // A real bookshelf is around a meter wide; a cm-scale model would be ~100.
        let aabb = build_bookshelf("{}").aabb().unwrap();
        assert!(aabb.max[0] >= 500.0, "{:?}", aabb.max);
    }

    #[test]
    fn pipeline_reports_print_view() {
        let mut pipeline = Pipeline::new(&repo_root(), KernelChoice::Truck).unwrap();
        let update = pipeline.evaluate(&repo_root().join("models/bookshelf.ts"));
        let message = &update.diagnostics[0].message;
        assert!(message.contains("; print "), "{message}");

        let update = pipeline.evaluate(&repo_root().join("models/cube.ts"));
        assert!(update.views.unwrap().print.is_none());
        assert!(!update.diagnostics[0].message.contains("print"));
    }

    #[test]
    fn print_view_errors_are_labelled() {
        let project = Project::new(
            "import { cube } from \"../dsl/cad.ts\";\n\
             export default () => ({ assembled: cube(1), print: { kind: \"Arrange\", gap: -1, children: [cube(1)] } });\n",
        );
        let update = project.pipeline().evaluate(&project.model());
        assert!(update.views.is_none());
        let message = &update.diagnostics[0].message;
        assert!(message.contains("(print view)") && message.contains("gap"), "{message}");
    }

    #[test]
    fn bookshelf_fills_its_outer_dimensions() {
        let scene = build_bookshelf("{ width: 80, depth: 30, height: 180, shelves: 4 }");
        let aabb = scene.aabb().unwrap();
        assert_eq!(aabb.min, [0.0, 0.0, 0.0]);
        for (got, want) in aabb.max.iter().zip([80.0, 30.0, 180.0]) {
            assert!((got - want).abs() < 1e-3, "{:?}", aabb.max);
        }
    }

    #[test]
    fn bookshelf_back_panel_is_the_only_backing_colored_board() {
        let shelves = 3;
        let scene = build_bookshelf(&format!(
            r##"{{ shelves: {shelves}, depth: 300, back: 6, wood: "#ff0000", backing: "#0000ff" }}"##
        ));
        let boards = 5 + shelves;
        let per_board = scene.positions.len() / boards;
        assert_eq!(per_board * boards, scene.positions.len());

        let blue = scene.colors.iter().filter(|c| **c == [0, 0, 255]).count();
        let red = scene.colors.iter().filter(|c| **c == [255, 0, 0]).count();
        assert_eq!(blue, per_board, "exactly one board is backing-colored");
        assert_eq!(red + blue, scene.positions.len(), "no default-colored vertices");

        // The blue board is the back panel: all its vertices sit at the rear.
        let back_min_y = scene
            .positions
            .iter()
            .zip(&scene.colors)
            .filter(|(_, c)| **c == [0, 0, 255])
            .map(|(p, _)| p[1])
            .fold(f32::MAX, f32::min);
        assert!((back_min_y - (300.0 - 6.0)).abs() < 1e-3, "{back_min_y}");
    }

    #[test]
    fn bookshelf_has_one_box_per_board() {
        // 2 sides + top + bottom + back = 5 boards, plus the shelves.
        // Every board is a box: 6 quads = 12 triangles and 12 edges.
        for shelves in [0, 1, 4, 7] {
            let scene = build_bookshelf(&format!("{{ shelves: {shelves} }}"));
            let boards = 5 + shelves;
            assert_eq!(scene.indices.len() / 3, boards * 12, "shelves={shelves}");
            assert_eq!(scene.edges.len() / 2, boards * 12, "shelves={shelves}");
        }
    }

    // --- Event filtering ----------------------------------------------------

    #[test]
    fn script_edits_are_relevant() {
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        assert!(is_relevant(&event(modify, "models/cube.ts")));
        assert!(is_relevant(&event(modify, "dsl/cad.js")));
        assert!(is_relevant(&event(EventKind::Create(CreateKind::File), "models/new.ts")));
        assert!(is_relevant(&event(EventKind::Remove(notify::event::RemoveKind::File), "dsl/cad.ts")));
    }

    #[test]
    fn other_files_and_reads_are_ignored() {
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        assert!(!is_relevant(&event(modify, "models/notes.md")));
        assert!(!is_relevant(&event(modify, "models/.cube.ts.swp")));
        assert!(!is_relevant(&event(modify, "models/no_extension")));
        assert!(!is_relevant(&event(EventKind::Access(AccessKind::Any), "models/cube.ts")));
    }

    // --- Hot reload (real file watcher, no window) ---------------------------

    #[test]
    fn reloads_when_model_changes() {
        let project = Project::new(&cube_source(10));
        let (rx, _handle) = project.watch();

        project.write_model(&cube_source(20));

        let update = rx.recv_timeout(RELOAD_TIMEOUT).expect("reload");
        assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
        assert_eq!(max_extent(&update), 20.0);
    }

    #[test]
    fn reloads_when_dsl_changes() {
        let project = Project::new(&cube_source(10));
        let (rx, _handle) = project.watch();

        // Library change, model untouched: make cube() double its size.
        let dsl = project.root().join("dsl/cad.ts");
        let source = std::fs::read_to_string(&dsl).unwrap();
        let patched = source.replace(
            "return box({ width: size, depth: size, height: size });",
            "return box({ width: size * 2, depth: size * 2, height: size * 2 });",
        );
        assert_ne!(source, patched, "dsl/cad.ts cube() changed shape; update this test");
        std::fs::write(&dsl, patched).unwrap();

        let update = rx.recv_timeout(RELOAD_TIMEOUT).expect("reload");
        assert_eq!(max_extent(&update), 20.0);
    }

    #[test]
    fn save_burst_triggers_one_reload_with_final_contents() {
        let project = Project::new(&cube_source(10));
        let (rx, _handle) = project.watch();

        for size in 11..=15 {
            project.write_model(&cube_source(size));
            thread::sleep(Duration::from_millis(20));
        }

        let update = rx.recv_timeout(RELOAD_TIMEOUT).expect("reload");
        assert_eq!(max_extent(&update), 15.0);
        assert!(rx.recv_timeout(QUIET).is_err(), "burst should debounce to one reload");
    }

    #[test]
    fn recovers_after_script_error() {
        let project = Project::new(&cube_source(10));
        let (rx, _handle) = project.watch();

        project.write_model(&cube_source(0));
        let broken = rx.recv_timeout(RELOAD_TIMEOUT).expect("error update");
        assert!(broken.views.is_none());
        assert!(!broken.diagnostics[0].ok);

        project.write_model(&cube_source(7));
        let fixed = rx.recv_timeout(RELOAD_TIMEOUT).expect("recovery");
        assert!(fixed.diagnostics[0].ok, "{:?}", fixed.diagnostics);
        assert_eq!(max_extent(&fixed), 7.0);
    }

    #[test]
    fn ignores_non_script_files() {
        let project = Project::new(&cube_source(10));
        let (rx, _handle) = project.watch();

        std::fs::write(project.root().join("models/notes.md"), "hello").unwrap();

        assert!(rx.recv_timeout(QUIET).is_err(), "non-TS edit must not reload");
    }

    #[test]
    fn reloads_model_outside_models_dir() {
        let project = Project::new(&cube_source(10));
        let model = project.root().join("scratch/try.ts");
        std::fs::create_dir_all(model.parent().unwrap()).unwrap();
        std::fs::write(&model, cube_source(3)).unwrap();

        let (tx, rx) = mpsc::channel();
        let (root, watched) = (project_root(&model), model.clone());
        let pipeline = Pipeline::new(&root, KernelChoice::Truck).unwrap();
        thread::spawn(move || watch_and_reload(&root, &watched, pipeline, tx));
        thread::sleep(Duration::from_millis(300));

        std::fs::write(&model, cube_source(4)).unwrap();

        let update = rx.recv_timeout(RELOAD_TIMEOUT).expect("reload");
        assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
        assert_eq!(max_extent(&update), 4.0);
    }

    #[test]
    fn stops_once_viewer_is_gone() {
        let project = Project::new(&cube_source(10));
        let (rx, handle) = project.watch();
        drop(rx);

        project.write_model(&cube_source(11));

        let deadline = Instant::now() + RELOAD_TIMEOUT;
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "watcher thread kept running");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(handle.join().unwrap().is_ok());
    }
}
