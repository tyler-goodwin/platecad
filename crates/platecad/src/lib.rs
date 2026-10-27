//! Library behind the `plate` binary: the CLI, `plate init`, the reload
//! pipeline (TS file -> script host -> kernel -> `SceneUpdate`) and the viewer
//! session that re-runs it whenever watched sources change. Kept windowless so
//! it can be tested headless.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use cad_ir::{Bed, Diagnostic, MeshQuality, ModelSpec, ModelViews, TessellatedScene};
use cad_kernel::Kernel;
use cad_kernel_occ::OccKernel;
use cad_script::{RustyScriptHost, ScriptHost};
use cad_view::{
    FileEntry, FolderListing, SceneUpdate, SceneViews, Settings, ViewMode, ViewerCommand, ViewerMessage,
};
use clap::{Parser, Subcommand, ValueEnum};
use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode, Watcher};

pub const DEBOUNCE: Duration = Duration::from_millis(150);
/// A directory containing this file is a project root (imports and watching are scoped to it).
pub const PROJECT_MARKER: &str = "dsl/cad.ts";
/// Optional printer limits at the project root.
pub const PRINTER_CONFIG: &str = "printer.toml";

/// Contents of [`PRINTER_CONFIG`].
#[derive(Debug, Clone, Copy, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrinterConfig {
    pub bed: Bed,
}

/// Reads `root/printer.toml`. `Ok(None)` if there isn't one; an error (naming
/// the file) if it's unreadable, malformed or has non-positive sizes.
pub fn load_printer_config(root: &Path) -> Result<Option<PrinterConfig>, String> {
    let text = match std::fs::read_to_string(root.join(PRINTER_CONFIG)) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{PRINTER_CONFIG}: {e}")),
    };
    let config: PrinterConfig =
        toml::from_str(&text).map_err(|e| format!("{PRINTER_CONFIG}: {}", e.message()))?;
    let Bed {
        width,
        depth,
        height,
    } = config.bed;
    if ![width, depth, height].iter().all(|d| *d > 0.0 && d.is_finite()) {
        return Err(format!("{PRINTER_CONFIG}: bed width, depth and height must be positive"));
    }
    Ok(Some(config))
}

/// `spec` with `bed` filled in on every `Arrange` that doesn't set its own.
pub fn with_default_bed(spec: ModelSpec, bed: Bed) -> ModelSpec {
    let boxed = |shape: Box<ModelSpec>| Box::new(with_default_bed(*shape, bed));
    let all = |children: Vec<ModelSpec>| {
        children
            .into_iter()
            .map(|c| with_default_bed(c, bed))
            .collect()
    };
    match spec {
        ModelSpec::Rect { .. }
        | ModelSpec::RoundedRect { .. }
        | ModelSpec::Polygon { .. }
        | ModelSpec::Circle { .. }
        | ModelSpec::Sphere { .. }
        | ModelSpec::Cylinder { .. }
        | ModelSpec::Cone { .. }
        | ModelSpec::Torus { .. } => spec,
        ModelSpec::Cutout { outer, holes } => ModelSpec::Cutout {
            outer: boxed(outer),
            holes: all(holes),
        },
        ModelSpec::Extrude {
            profile,
            distance,
            direction,
        } => ModelSpec::Extrude {
            profile: boxed(profile),
            distance,
            direction,
        },
        ModelSpec::Revolve { profile, degrees } => ModelSpec::Revolve {
            profile: boxed(profile),
            degrees,
        },
        ModelSpec::Translate { offset, shape } => ModelSpec::Translate {
            offset,
            shape: boxed(shape),
        },
        ModelSpec::Rotate {
            axis,
            degrees,
            origin,
            shape,
        } => ModelSpec::Rotate {
            axis,
            degrees,
            origin,
            shape: boxed(shape),
        },
        ModelSpec::Color { rgb, shape } => ModelSpec::Color {
            rgb,
            shape: boxed(shape),
        },
        ModelSpec::Fillet { shape, radius, edges } => ModelSpec::Fillet {
            shape: boxed(shape),
            radius,
            edges,
        },
        ModelSpec::Chamfer { shape, distance, edges } => ModelSpec::Chamfer {
            shape: boxed(shape),
            distance,
            edges,
        },
        ModelSpec::Part { name, shape } => ModelSpec::Part {
            name,
            shape: boxed(shape),
        },
        ModelSpec::Group { children } => ModelSpec::Group {
            children: all(children),
        },
        ModelSpec::Union { children } => ModelSpec::Union {
            children: all(children),
        },
        ModelSpec::Intersect { children } => ModelSpec::Intersect {
            children: all(children),
        },
        ModelSpec::Difference { shape, tools } => ModelSpec::Difference {
            shape: boxed(shape),
            tools: all(tools),
        },
        ModelSpec::Arrange {
            gap,
            children,
            bed: own,
        } => ModelSpec::Arrange {
            gap,
            children: all(children),
            bed: own.or(Some(bed)),
        },
    }
}

/// Parts of a print scene that don't fit `bed` as oriented, as one warning.
/// `bed.height` of infinity skips the height check.
pub fn oversize_warning(scene: &TessellatedScene, bed: Bed) -> Option<String> {
    let mut names: Vec<&str> = Vec::new();
    for part in &scene.parts {
        if !names.contains(&part.name.as_str()) {
            names.push(&part.name);
        }
    }
    const SLACK: f32 = 1e-3;
    let too_big: Vec<String> = names
        .into_iter()
        .filter_map(|name| {
            let positions = scene
                .parts
                .iter()
                .filter(|p| p.name == name)
                .flat_map(|p| &scene.positions[p.vertices.start as usize..p.vertices.end as usize]);
            let mut min = [f32::MAX; 3];
            let mut max = [f32::MIN; 3];
            for p in positions {
                for i in 0..3 {
                    min[i] = min[i].min(p[i]);
                    max[i] = max[i].max(p[i]);
                }
            }
            let size = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
            let fits = size[0] <= bed.width as f32 + SLACK
                && size[1] <= bed.depth as f32 + SLACK
                && size[2] <= bed.height as f32 + SLACK;
            (!fits).then(|| format!("{name} ({:.0} × {:.0} × {:.0})", size[0], size[1], size[2]))
        })
        .collect();
    if too_big.is_empty() {
        return None;
    }
    let bed_text = if bed.height.is_finite() {
        format!("{:.0} × {:.0} × {:.0} mm", bed.width, bed.depth, bed.height)
    } else {
        format!("{:.0} × {:.0} mm", bed.width, bed.depth)
    };
    let noun = if too_big.len() == 1 { "part doesn't" } else { "parts don't" };
    Some(format!(
        "{} {noun} fit the {bed_text} bed: {}",
        too_big.len(),
        too_big.join(", ")
    ))
}

/// Code-first CAD: write models in TypeScript, see them hot-reload in a 3D
/// viewport, and export 3MF for slicers.
///
/// `plate [PATH]` is short for `plate open [PATH]`.
#[derive(Debug, Parser)]
#[command(name = "plate", version, args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[command(flatten)]
    pub open: OpenArgs,
}

impl Cli {
    /// The subcommand to run; no subcommand means `open`.
    pub fn into_command(self) -> Command {
        self.command.unwrap_or(Command::Open(self.open))
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open the viewer on a model or folder.
    Open(OpenArgs),
    /// Write a model's 3MF for slicers, without opening the viewer.
    Export(ExportArgs),
    /// Start a project: printer config, the DSL and a starter model.
    Init(InitArgs),
}

#[derive(Debug, clap::Args)]
pub struct OpenArgs {
    /// Model script or folder to open. A model opens with its project root
    /// (the nearest folder up containing `dsl/cad.ts`) in the file list.
    /// Without one, the current folder is opened if it's a project root.
    pub path: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    /// Model script to export.
    pub model: PathBuf,

    /// 3MF file to write [default: <project root>/outputs/<model>_<view>.3mf].
    #[arg(short, long, value_name = "FILE.3mf")]
    pub output: Option<PathBuf>,

    /// Which view to export.
    #[arg(long, value_enum, default_value_t = ExportView::Auto)]
    pub view: ExportView,

    /// How finely curved surfaces are meshed.
    #[arg(long, value_enum, default_value_t = QualityArg::Fine)]
    pub quality: QualityArg,
}

#[derive(Debug, clap::Args)]
pub struct InitArgs {
    /// Folder to set up (created if missing).
    #[arg(default_value = ".")]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ExportView {
    /// The print layout if the model has one, else the assembled view.
    Auto,
    /// The print layout (error if the model has none).
    Print,
    /// The model as assembled.
    Assembled,
}

/// [`MeshQuality`] on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum QualityArg {
    /// 0.1 mm from the true surface: fastest.
    Draft,
    /// 0.03 mm.
    Normal,
    /// 0.01 mm (default).
    Fine,
}

impl From<QualityArg> for MeshQuality {
    fn from(arg: QualityArg) -> Self {
        match arg {
            QualityArg::Draft => MeshQuality::Draft,
            QualityArg::Normal => MeshQuality::Normal,
            QualityArg::Fine => MeshQuality::Fine,
        }
    }
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

/// Directories to watch so every dependency's changes are seen, and the
/// opened `folder` (recursively) so the file list is, mapped to whether the
/// watch must be recursive.
///
/// A file is watched through its folder (non-recursively), not directly:
/// editors that save via write-temp-then-rename replace the file, which would
/// drop a watch on the file itself. A dependency whose folder doesn't exist
/// yet (a missing import) is watched through its nearest existing ancestor,
/// recursively, so creating the folder and file is still seen. Folders inside
/// `folder` need no watch of their own.
pub fn watch_targets(dependencies: &BTreeSet<PathBuf>, folder: Option<&Path>) -> BTreeMap<PathBuf, bool> {
    let mut targets: BTreeMap<PathBuf, bool> = BTreeMap::new();
    if let Some(folder) = folder {
        targets.insert(folder.to_path_buf(), true);
    }
    for dep in dependencies {
        let Some(dir) = dep.parent() else { continue };
        let (dir, recursive) = if dir.is_dir() {
            (dir, false)
        } else {
            match dir.ancestors().find(|d| d.is_dir()) {
                Some(existing) => (existing, true),
                None => continue,
            }
        };
        if folder.is_some_and(|f| dir.starts_with(f)) {
            continue;
        }
        *targets.entry(dir.to_path_buf()).or_default() |= recursive;
    }
    targets
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

/// Script host + kernel: one TS file in, one `SceneUpdate` out. Also tracks
/// which files the model depends on, for the watcher.
pub struct Pipeline {
    root: PathBuf,
    script: RustyScriptHost,
    kernel: Box<dyn Kernel + Send>,
    dependencies: BTreeSet<PathBuf>,
    /// Mesh quality of [`Pipeline::evaluate`]'s scenes.
    quality: MeshQuality,
    /// The specs behind the last successful evaluation, print bed filled in,
    /// so [`Pipeline::rebuild`] can mesh them again at another quality.
    last_good: Option<ModelViews>,
}

impl Pipeline {
    /// A pipeline meshing at [`MeshQuality::Fine`].
    pub fn new(root: &Path) -> Result<Self, String> {
        let script = RustyScriptHost::new(root).map_err(|e| e.to_string())?;
        Ok(Self {
            root: normalize(&std::path::absolute(root).map_err(|e| e.to_string())?),
            script,
            kernel: Box::new(OccKernel::default()),
            dependencies: BTreeSet::new(),
            quality: MeshQuality::Fine,
            last_good: None,
        })
    }

    pub fn quality(&self) -> MeshQuality {
        self.quality
    }

    /// Mesh quality for later evaluations.
    pub fn set_quality(&mut self, quality: MeshQuality) {
        self.quality = quality;
    }

    /// Files the model read on its last successful eval, plus anything a failed
    /// eval since then touched, plus the project's [`PRINTER_CONFIG`] (whether
    /// or not it exists). Keeping the old set across failures means fixing a
    /// broken import (which may stop the eval before later imports load) still
    /// triggers a reload.
    pub fn dependencies(&self) -> &BTreeSet<PathBuf> {
        &self.dependencies
    }

    pub fn evaluate(&mut self, model: &Path) -> SceneUpdate {
        let started = Instant::now();
        let name = model.file_name().unwrap_or_default().to_string_lossy();
        let result = self.script.eval_file(model);
        let latest = self.script.last_dependencies().iter().cloned();
        if result.is_ok() {
            self.dependencies = latest.collect();
        } else {
            self.dependencies.extend(latest);
        }
        self.dependencies.insert(self.root.join(PRINTER_CONFIG));
        let views = match result {
            Ok(views) => views,
            Err(e) => return SceneUpdate::error(format!("{name}: {e}")),
        };
        let printer = match load_printer_config(&self.root) {
            Ok(printer) => printer,
            Err(e) => return SceneUpdate::error(e),
        };
        let print_spec = match printer {
            Some(config) => views.print.map(|spec| with_default_bed(spec, config.bed)),
            None => views.print,
        };
        let quality = self.quality;
        let assembled = match self.kernel.build_with(&views.assembled, quality) {
            Ok(scene) => scene,
            Err(e) => return SceneUpdate::error(format!("{name}: {e}")),
        };
        let print = match print_spec
            .as_ref()
            .map(|spec| self.kernel.build_with(spec, quality))
            .transpose()
        {
            Ok(scene) => scene,
            Err(e) => return SceneUpdate::error(format!("{name} (print view): {e}")),
        };
        self.last_good = Some(ModelViews {
            assembled: views.assembled,
            print: print_spec,
        });

        let triangles = |scene: &cad_ir::TessellatedScene| scene.indices.len() / 3;
        let print_note = print
            .as_ref()
            .map(|p| format!("; print {}", triangles(p)))
            .unwrap_or_default();
        let message = format!(
            "{name} OK ({} triangles{print_note}, {} mesh, {} ms)",
            triangles(&assembled),
            quality.label().to_lowercase(),
            started.elapsed().as_millis()
        );

        // Check against the configured bed, or else the plates arrange laid out.
        let bed = printer.map(|c| c.bed).or_else(|| {
            let plate = print.as_ref()?.plates.first()?;
            Some(Bed {
                width: plate.size[0] as f64,
                depth: plate.size[1] as f64,
                height: f64::INFINITY,
            })
        });
        let warning = print
            .as_ref()
            .zip(bed)
            .and_then(|(scene, bed)| oversize_warning(scene, bed));

        let mut update = SceneUpdate::ok(SceneViews { assembled, print }, message);
        update.diagnostics.extend(warning.map(Diagnostic::warning));
        update
    }
}

/// Evaluates `model` and writes the chosen view to `out` as 3MF, or by
/// default to [`export_path`] under the model's project root. Returns a
/// one-line summary. Nothing is left at the output on failure.
pub fn export_3mf(
    pipeline: &mut Pipeline,
    model: &Path,
    view: ExportView,
    out: Option<&Path>,
) -> Result<String, String> {
    let update = pipeline.evaluate(model);
    let warnings: String = update
        .diagnostics
        .iter()
        .filter(|d| d.warning)
        .map(|d| format!("\nwarning: {}", d.message))
        .collect();
    let Some(views) = update.views else {
        let messages: Vec<&str> = update.diagnostics.iter().map(|d| d.message.as_str()).collect();
        return Err(messages.join("\n"));
    };
    let (mode, scene) = match (view, views.print) {
        (ExportView::Assembled, _) | (ExportView::Auto, None) => (ViewMode::Assembled, views.assembled),
        (ExportView::Print | ExportView::Auto, Some(print)) => (ViewMode::Print, print),
        (ExportView::Print, None) => {
            return Err(format!(
                "{} has no print view (export {{ assembled, print }} from it, or use --view assembled)",
                model.display()
            ));
        }
    };

    let out = match out {
        Some(out) => out.to_path_buf(),
        None => export_path(&project_root(model), model, mode),
    };
    let objects = write_scene_3mf(&scene, &out)?;
    Ok(format!(
        "wrote {} ({} view, {objects} objects){warnings}",
        out.display(),
        mode.label()
    ))
}

impl Pipeline {
    /// `view` of the last successful [`Pipeline::evaluate`], meshed again at
    /// `quality` without re-running the script. `None` if nothing has built,
    /// or the view is `Print` and the model has none.
    pub fn rebuild(&mut self, view: ViewMode, quality: MeshQuality) -> Option<Result<TessellatedScene, String>> {
        let last = self.last_good.as_ref()?;
        let spec = match view {
            ViewMode::Assembled => last.assembled.clone(),
            ViewMode::Print => last.print.clone()?,
        };
        Some(self.kernel.build_with(&spec, quality).map_err(|e| e.to_string()))
    }
}

pub fn report(update: &SceneUpdate) {
    for d in &update.diagnostics {
        if d.warning {
            eprintln!("warning: {}", d.message);
        } else if d.ok {
            println!("{}", d.message);
        } else {
            eprintln!("{}", d.message);
        }
    }
}

/// What the viewer opens with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Start {
    /// Nothing yet: the user picks a folder in the viewer.
    Empty,
    /// Browse a folder, with no model open.
    Folder(PathBuf),
    /// Open a model, browsing its project root.
    Model(PathBuf),
}

/// What a CLI path opens: a folder or a model file. Resolved against `cwd`
/// and checked to exist, like [`resolve_model`].
pub fn resolve_start(path: &Path, cwd: &Path) -> Result<Start, String> {
    let path = normalize(&cwd.join(path));
    if path.is_dir() {
        Ok(Start::Folder(path))
    } else if path.is_file() {
        Ok(Start::Model(path))
    } else {
        Err(format!("{} not found", path.display()))
    }
}

/// Project root for an opened folder: the folder itself or its nearest
/// ancestor containing [`PROJECT_MARKER`], else the folder. Every file under
/// the folder can then import within the project.
pub fn folder_root(folder: &Path) -> PathBuf {
    folder
        .ancestors()
        .find(|d| d.join(PROJECT_MARKER).is_file())
        .unwrap_or(folder)
        .to_path_buf()
}

/// Model files under `folder` for the file browser: `.ts` files (not `.d.ts`)
/// and the folders that contain some, at any depth. Folders come first, then
/// files, each sorted case-insensitively. Symlinked folders aren't followed.
pub fn list_models(folder: &Path) -> Vec<FileEntry> {
    let Ok(read) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            let children = list_models(&entry.path());
            if !children.is_empty() {
                dirs.push(FileEntry::Dir { name, children });
            }
        } else if name.ends_with(".ts") && !name.ends_with(".d.ts") {
            files.push(FileEntry::File {
                name,
                path: entry.path(),
            });
        }
    }
    let key = |e: &FileEntry| match e {
        FileEntry::Dir { name, .. } | FileEntry::File { name, .. } => name.to_lowercase(),
    };
    dirs.sort_by_key(key);
    files.sort_by_key(key);
    dirs.extend(files);
    dirs
}

/// The DSL a new project gets, as built into this binary.
pub const DSL_SOURCE: &str = include_str!("../../../dsl/cad.ts");

/// `printer.toml` for a new project.
pub const STARTER_PRINTER: &str = r#"# Printer limits for print views (millimeters).
#
# arrange() in a model's print view packs parts onto plates of this size,
# and the viewer warns about parts that don't fit. Edits hot-reload.
# A model can override this with arrange(parts, { bed: { ... } }).

# Build volume of a Bambu Lab P2S. Others: Bambu X1/P1/A1 are also
# 256 x 256 x 256; Prusa MK4 is 250 x 210 x 220.
[bed]
width = 256
depth = 256
height = 256
"#;

/// The model a new project starts with.
pub const STARTER_MODEL: &str = r#"import { cube } from "../dsl/cad.ts";

/** A 20 mm cube. Edit and save: the viewer reloads. Sizes are millimeters. */
export default () => cube(20);
"#;

/// Lets editors resolve `../dsl/cad.ts` imports and check models against
/// the DSL's types. The viewer doesn't read it.
pub const STARTER_TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "allowImportingTsExtensions": true,
    "noEmit": true,
    "strict": true,
    "skipLibCheck": true
  },
  "include": ["**/*.ts"]
}
"#;

/// Where `init` puts [`STARTER_MODEL`], relative to the project folder.
pub const STARTER_MODEL_PATH: &str = "models/cube.ts";

/// What `init` writes, relative to the project folder.
pub const STARTER_FILES: [(&str, &str); 5] = [
    (PROJECT_MARKER, DSL_SOURCE),
    (PRINTER_CONFIG, STARTER_PRINTER),
    (STARTER_MODEL_PATH, STARTER_MODEL),
    ("tsconfig.json", STARTER_TSCONFIG),
    (".gitignore", "/outputs\n"),
];

/// Sets up a project in `dir` (created if missing) with [`STARTER_FILES`].
/// Refuses, writing nothing, if any of them already exists. Returns the
/// files written.
pub fn init_project(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let existing: Vec<String> = STARTER_FILES
        .iter()
        .filter(|(path, _)| dir.join(path).exists())
        .map(|(path, _)| path.to_string())
        .collect();
    if !existing.is_empty() {
        return Err(format!(
            "{} already has {}; not overwriting anything",
            dir.display(),
            existing.join(", ")
        ));
    }
    let mut written = Vec::new();
    for (path, contents) in STARTER_FILES {
        // Join per component so Windows paths don't mix separators.
        let path = path.split('/').fold(dir.to_path_buf(), |p, c| p.join(c));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&path, contents).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

/// Folder under the project root that viewer exports go to.
pub const OUTPUTS_DIR: &str = "outputs";

/// Where the viewer exports `view` of `model`: `<root>/outputs/<model>_<view>.3mf`.
pub fn export_path(root: &Path, model: &Path, view: ViewMode) -> PathBuf {
    let stem = model.file_stem().unwrap_or_default().to_string_lossy();
    root.join(OUTPUTS_DIR)
        .join(format!("{stem}_{}.3mf", view.label()))
}

/// Writes `scene` to `out` as 3MF, creating its folder. Returns the object
/// count. Nothing is left at `out` on failure.
pub fn write_scene_3mf(scene: &TessellatedScene, out: &Path) -> Result<usize, String> {
    let write = || -> Result<usize, String> {
        if let Some(dir) = out.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        let file = std::fs::File::create(out)
            .map_err(|e| format!("cannot create {}: {e}", out.display()))?;
        cad_export::write_3mf(scene, std::io::BufWriter::new(file)).map_err(|e| e.to_string())
    };
    write().inspect_err(|_| {
        let _ = std::fs::remove_file(out);
    })
}

/// File in [`plate_home`] that [`AppState`] is saved to.
pub const SETTINGS_FILE: &str = "settings.toml";

/// Folder for per-user state: `$PLATE_HOME` if set, else `.plate` in the
/// home folder. `None` if there's no home folder to put it in.
pub fn plate_home() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("PLATE_HOME").or_else(|| Some(var(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?.join(".plate")))
}

/// What the viewer remembers between runs, as saved in [`SETTINGS_FILE`].
/// Missing keys take their defaults, so older files keep loading.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AppState {
    /// The folder opened last, reopened when `plate` starts without a path.
    pub last_folder: Option<PathBuf>,
    pub settings: Settings,
}

/// [`AppState`] plus the file it's saved to (none: kept in memory only).
#[derive(Debug, Clone, Default)]
pub struct Store {
    path: Option<PathBuf>,
    pub state: AppState,
}

impl Store {
    /// Reads `path`. A missing file gives the defaults; an unreadable or
    /// malformed one gives the defaults and a warning naming the file, which
    /// the next save replaces.
    pub fn load(path: PathBuf) -> (Self, Option<String>) {
        let (state, warning) = match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(state) => (state, None),
                Err(e) => (
                    AppState::default(),
                    Some(format!("{}: {}; using default settings", path.display(), e.message())),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (AppState::default(), None),
            Err(e) => (
                AppState::default(),
                Some(format!("{}: {e}; using default settings", path.display())),
            ),
        };
        (Self { path: Some(path), state }, warning)
    }

    /// A store that never touches the disk.
    pub fn in_memory(state: AppState) -> Self {
        Self { path: None, state }
    }

    /// Writes the state to its file, creating the folder.
    pub fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else { return Ok(()) };
        let text = toml::to_string(&self.state).map_err(|e| format!("cannot save settings: {e}"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }
}

/// Runs the app side of the viewer until the viewer goes away: opens `start`,
/// then handles [`ViewerCommand`]s and file changes, sending results on `tx`.
///
/// The opened folder is watched recursively so the file list stays current;
/// the model rebuilds only when one of its [`Pipeline::dependencies`] changes
/// (which may be outside the folder, like a project's `dsl/cad.ts`). Change
/// bursts are debounced by [`DEBOUNCE`]. Settings changes and each opened
/// folder are saved to `store`. Builds and export re-meshes run on the open
/// model's own worker thread (see [`spawn_worker`]), so commands are handled
/// while a model builds. Returns once `tx`'s receiver or `commands`' sender
/// is dropped.
pub fn run_session(
    start: Start,
    store: Store,
    commands: Receiver<ViewerCommand>,
    tx: Sender<ViewerMessage>,
) -> notify::Result<()> {
    let (input_tx, inputs) = mpsc::channel();
    let fs_tx = input_tx.clone();
    let built_tx = input_tx.clone();
    let watcher = notify::recommended_watcher(move |event| {
        let _ = fs_tx.send(Input::Fs(event));
    })?;
    std::thread::spawn(move || {
        for command in commands {
            if input_tx.send(Input::Command(command)).is_err() {
                return;
            }
        }
        let _ = input_tx.send(Input::ViewerClosed);
    });
    let session = Session {
        store,
        tx,
        built_tx,
        watched: WatchSet::new(watcher),
        folder: None,
        model: None,
        next_model: 0,
    };
    match session.run(start, inputs) {
        Ok(()) | Err(Stop::ViewerGone) => Ok(()),
        Err(Stop::Watch(e)) => Err(e),
    }
}

enum Input {
    Fs(notify::Result<notify::Event>),
    Command(ViewerCommand),
    ViewerClosed,
    /// A build worker finished a job.
    Built(Built),
}

/// What the session asks an open model's build worker to do.
enum Job {
    /// Run the script and build its views at `quality`. A higher `seq` is a
    /// newer request.
    Evaluate { seq: u64, quality: MeshQuality },
    /// Mesh the last good build's `view` again at `quality` and write it to
    /// `out` (shown relative to `root`).
    Export {
        view: ViewMode,
        quality: MeshQuality,
        out: PathBuf,
        root: PathBuf,
    },
}

/// A finished [`Job`], tagged with the [`OpenModel::id`] it was for.
struct Built {
    model: u64,
    outcome: Outcome,
}

enum Outcome {
    Evaluated {
        seq: u64,
        update: SceneUpdate,
        quality: MeshQuality,
        dependencies: BTreeSet<PathBuf>,
    },
    Exported(Result<String, String>),
}

/// Runs one open model's jobs on their own thread, so the session keeps
/// answering the viewer and the watcher while a model builds. Evaluations
/// queued back to back collapse into the last one. A job that's already
/// running can't be stopped (OpenCascade has no cancel), so the session
/// drops results it no longer wants. The thread ends when the session drops
/// `jobs`, that is when the model is closed, after any job in progress.
fn spawn_worker(id: u64, model: PathBuf, mut pipeline: Pipeline, jobs: Receiver<Job>, done: Sender<Input>) {
    std::thread::spawn(move || {
        let mut queue = VecDeque::new();
        while let Some(job) = queue.pop_front().or_else(|| jobs.recv().ok()) {
            queue.extend(jobs.try_iter());
            if matches!(job, Job::Evaluate { .. }) && matches!(queue.front(), Some(Job::Evaluate { .. })) {
                continue;
            }
            let outcome = run_job(&mut pipeline, &model, job);
            if done.send(Input::Built(Built { model: id, outcome })).is_err() {
                return;
            }
        }
    });
}

fn run_job(pipeline: &mut Pipeline, model: &Path, job: Job) -> Outcome {
    let name = model.file_name().unwrap_or_default().to_string_lossy().into_owned();
    // A panic in the kernel shouldn't leave the viewer waiting forever.
    let caught = |what: &str| format!("{name}: {what} crashed; edit the model to try again");
    match job {
        Job::Evaluate { seq, quality } => {
            pipeline.set_quality(quality);
            let update = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| pipeline.evaluate(model)))
                .unwrap_or_else(|_| SceneUpdate::error(caught("the build")));
            Outcome::Evaluated {
                seq,
                update,
                quality,
                dependencies: pipeline.dependencies().clone(),
            }
        }
        Job::Export {
            view,
            quality,
            out,
            root,
        } => {
            let export = || match pipeline.rebuild(view, quality) {
                None => Err(format!("{name} has no print view.")),
                Some(Err(e)) => Err(format!("{name}: {e}")),
                Some(Ok(scene)) => {
                    let objects = write_scene_3mf(&scene, &out)?;
                    Ok(export_summary(&root, &out, view, quality, objects))
                }
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(export))
                .unwrap_or_else(|_| Err(caught("the export")));
            Outcome::Exported(result)
        }
    }
}

/// The notice for a finished viewer export.
fn export_summary(root: &Path, out: &Path, view: ViewMode, quality: MeshQuality, objects: usize) -> String {
    let shown = out.strip_prefix(root).unwrap_or(out);
    format!(
        "Wrote {} ({} view, {objects} objects, {} mesh)",
        shown.display(),
        view.label(),
        quality.label().to_lowercase()
    )
}

/// Why a session stops.
enum Stop {
    ViewerGone,
    Watch(notify::Error),
}

impl From<notify::Error> for Stop {
    fn from(e: notify::Error) -> Self {
        Stop::Watch(e)
    }
}

struct OpenFolder {
    listing: FolderListing,
    /// Import root and export base: [`folder_root`] of the folder.
    root: PathBuf,
}

struct OpenModel {
    path: PathBuf,
    /// Tags this model's worker results; a closed model's late ones are dropped.
    id: u64,
    /// To this model's build worker, which owns its [`Pipeline`].
    jobs: Sender<Job>,
    /// [`Pipeline::dependencies`] as of the last finished evaluation; just the
    /// model until the first one finishes.
    dependencies: BTreeSet<PathBuf>,
    /// The `seq` of the newest evaluation asked for. Older results are stale.
    requested: u64,
    /// The last good build: what the viewer shows, and what export writes
    /// when the export quality matches.
    views: Option<SceneViews>,
    /// The mesh quality `views` was built at.
    views_quality: MeshQuality,
}

/// What a burst of file events calls for.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Change {
    reload: bool,
    listing: bool,
}

struct Session<W: Watcher> {
    store: Store,
    tx: Sender<ViewerMessage>,
    /// Where build workers report back (the session's own inputs).
    built_tx: Sender<Input>,
    watched: WatchSet<W>,
    folder: Option<OpenFolder>,
    model: Option<OpenModel>,
    next_model: u64,
}

impl<W: Watcher> Session<W> {
    fn run(mut self, start: Start, inputs: Receiver<Input>) -> Result<(), Stop> {
        match start {
            Start::Empty => {}
            Start::Folder(folder) => self.open_folder(&folder)?,
            Start::Model(model) => self.open_model(&model)?,
        }
        // Commands that arrive mid-debounce wait here, in order.
        let mut queued = VecDeque::new();
        loop {
            let input = match queued.pop_front() {
                Some(input) => input,
                None => match inputs.recv() {
                    Ok(input) => input,
                    Err(_) => return Ok(()),
                },
            };
            match input {
                Input::ViewerClosed => return Err(Stop::ViewerGone),
                Input::Command(command) => self.handle(command)?,
                Input::Built(built) => self.built(built)?,
                Input::Fs(event) => {
                    let mut change = self.classify(&event?);
                    if change == Change::default() {
                        continue;
                    }
                    // Swallow the rest of the burst, noting what it touched.
                    loop {
                        match inputs.recv_timeout(DEBOUNCE) {
                            Ok(Input::Fs(event)) => {
                                let more = self.classify(&event?);
                                change.reload |= more.reload;
                                change.listing |= more.listing;
                            }
                            Ok(other) => queued.push_back(other),
                            Err(RecvTimeoutError::Timeout) => break,
                            Err(RecvTimeoutError::Disconnected) => return Ok(()),
                        }
                    }
                    if change.listing {
                        self.refresh_listing()?;
                    }
                    if change.reload {
                        self.evaluate("Rebuilding")?;
                    }
                }
            }
        }
    }

    fn send(&self, message: impl Into<ViewerMessage>) -> Result<(), Stop> {
        self.tx.send(message.into()).map_err(|_| Stop::ViewerGone)
    }

    fn notice(&self, notice: Diagnostic) -> Result<(), Stop> {
        self.send(ViewerMessage::Notice(notice))
    }

    fn handle(&mut self, command: ViewerCommand) -> Result<(), Stop> {
        match command {
            ViewerCommand::OpenFolder(folder) => self.open_folder(&folder),
            ViewerCommand::NewProject(folder) => self.new_project(&folder),
            ViewerCommand::OpenModel(model) => self.open_model(&model),
            ViewerCommand::Export(view) => match self.export(view) {
                Ok(Some(summary)) => self.notice(Diagnostic::ok(summary)),
                // Handed to the worker; its notice comes back through `built`.
                Ok(None) => Ok(()),
                Err(e) => self.notice(Diagnostic::error(e)),
            },
            ViewerCommand::Settings(settings) => self.change_settings(settings),
        }
    }

    /// Saves `settings`, and rebuilds the open model if its quality changed.
    fn change_settings(&mut self, settings: Settings) -> Result<(), Stop> {
        let old = std::mem::replace(&mut self.store.state.settings, settings);
        self.save()?;
        if old.view_quality == settings.view_quality {
            return Ok(());
        }
        self.evaluate("Rebuilding")
    }

    /// Saves the store, telling the viewer if that fails.
    fn save(&self) -> Result<(), Stop> {
        match self.store.save() {
            Ok(()) => Ok(()),
            Err(e) => self.notice(Diagnostic::warning(e)),
        }
    }

    /// [`init_project`] in `folder`, then opens it on the starter model. On
    /// failure (say, the folder is already a project) only a notice is sent.
    fn new_project(&mut self, folder: &Path) -> Result<(), Stop> {
        if let Err(e) = init_project(folder) {
            return self.notice(Diagnostic::error(e));
        }
        self.open_folder(folder)?;
        self.open_model(&folder.join(STARTER_MODEL_PATH))?;
        // After the open, which clears notices in the viewer.
        self.notice(Diagnostic::ok(format!(
            "Created a project in {}. Edit {STARTER_MODEL_PATH} and save to see it reload.",
            folder.display()
        )))
    }

    fn open_folder(&mut self, folder: &Path) -> Result<(), Stop> {
        let path = match std::path::absolute(folder) {
            Ok(path) if path.is_dir() => normalize(&path),
            _ => return self.notice(Diagnostic::error(format!("{} is not a folder", folder.display()))),
        };
        let listing = FolderListing {
            entries: list_models(&path),
            path: path.clone(),
        };
        self.folder = Some(OpenFolder {
            root: folder_root(&path),
            listing: listing.clone(),
        });
        self.model = None;
        self.send(ViewerMessage::Folder(listing))?;
        self.send(ViewerMessage::Model(None))?;
        self.sync_watches()?;
        if self.store.state.last_folder.as_ref() != Some(&path) {
            self.store.state.last_folder = Some(path);
            self.save()?;
        }
        Ok(())
    }

    /// Opens `model`, first opening its project root if it's outside the
    /// opened folder.
    fn open_model(&mut self, model: &Path) -> Result<(), Stop> {
        let path = match std::path::absolute(model) {
            Ok(path) => normalize(&path),
            Err(e) => return self.notice(Diagnostic::error(format!("{}: {e}", model.display()))),
        };
        let inside = self
            .folder
            .as_ref()
            .is_some_and(|f| path.starts_with(&f.listing.path));
        if !inside {
            self.open_folder(&project_root(&path))?;
        }
        let root = match &self.folder {
            Some(folder) => folder.root.clone(),
            None => project_root(&path),
        };
        let pipeline = match Pipeline::new(&root) {
            Ok(pipeline) => pipeline,
            Err(e) => return self.notice(Diagnostic::error(e)),
        };
        // Replacing the old model drops its job sender, which ends its worker.
        let id = self.next_model;
        self.next_model += 1;
        let (jobs, jobs_rx) = mpsc::channel();
        spawn_worker(id, path.clone(), pipeline, jobs_rx, self.built_tx.clone());
        self.model = Some(OpenModel {
            path: path.clone(),
            id,
            jobs,
            dependencies: BTreeSet::from([path.clone()]),
            requested: 0,
            views: None,
            views_quality: self.store.state.settings.view_quality,
        });
        self.send(ViewerMessage::Model(Some(path)))?;
        self.sync_watches()?;
        self.evaluate("Loading")
    }

    /// Announces a build of the open model at the view quality and hands it
    /// to the worker; [`Session::built`] sends the result.
    fn evaluate(&mut self, verb: &str) -> Result<(), Stop> {
        let quality = self.store.state.settings.view_quality;
        let Some(model) = &mut self.model else {
            return Ok(());
        };
        let name = model.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        model.requested += 1;
        let job = Job::Evaluate {
            seq: model.requested,
            quality,
        };
        if model.jobs.send(job).is_err() {
            return self.send(SceneUpdate::error(format!("{name}: the build thread stopped; reopen the model")));
        }
        self.send(SceneUpdate::loading(format!("{verb} {name}…")))
    }

    /// A worker's result: sent on if it's for the open model and still
    /// wanted, else dropped.
    fn built(&mut self, built: Built) -> Result<(), Stop> {
        let Some(model) = self.model.as_mut().filter(|m| m.id == built.model) else {
            return Ok(());
        };
        match built.outcome {
            Outcome::Evaluated {
                seq,
                update,
                quality,
                dependencies,
            } => {
                if seq < model.requested {
                    return Ok(()); // A newer build is on its way.
                }
                report(&update);
                if let Some(views) = &update.views {
                    model.views = Some(views.clone());
                    model.views_quality = quality;
                }
                model.dependencies = dependencies;
                self.send(update)?;
                self.sync_watches()
            }
            Outcome::Exported(result) => self.notice(match result {
                Ok(summary) => Diagnostic::ok(summary),
                Err(e) => Diagnostic::error(e),
            }),
        }
    }

    fn sync_watches(&mut self) -> Result<(), Stop> {
        let empty = BTreeSet::new();
        let dependencies = self.model.as_ref().map_or(&empty, |m| &m.dependencies);
        let folder = self.folder.as_ref().map(|f| f.listing.path.as_path());
        self.watched.sync(dependencies, folder)?;
        Ok(())
    }

    fn classify(&self, event: &notify::Event) -> Change {
        Change {
            reload: self
                .model
                .as_ref()
                .is_some_and(|m| is_relevant(event, &m.dependencies)),
            listing: self
                .folder
                .as_ref()
                .is_some_and(|f| may_change_listing(event, &f.listing.path)),
        }
    }

    /// Re-lists the folder, telling the viewer only if something changed.
    fn refresh_listing(&mut self) -> Result<(), Stop> {
        let Some(folder) = &mut self.folder else {
            return Ok(());
        };
        let entries = list_models(&folder.listing.path);
        if entries == folder.listing.entries {
            return Ok(());
        }
        folder.listing.entries = entries;
        let listing = folder.listing.clone();
        self.send(ViewerMessage::Folder(listing))
    }

    /// Writes the last good build's `view` to [`export_path`], meshed at the
    /// export quality, and returns the notice. A build at another quality is
    /// meshed again on the worker, without re-running the script: that's
    /// announced with a progress notice, returns `None`, and the worker's
    /// notice follows.
    fn export(&mut self, view: ViewMode) -> Result<Option<String>, String> {
        let quality = self.store.state.settings.export_quality;
        let root = match (&self.folder, &self.model) {
            (Some(folder), _) => folder.root.clone(),
            (None, Some(model)) => project_root(&model.path),
            (None, None) => PathBuf::new(),
        };
        let model = self.model.as_mut().ok_or("Open a model to export it.")?;
        let name = model.path.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let views = model
            .views
            .as_ref()
            .ok_or_else(|| format!("{name} hasn't built yet, so there's nothing to export."))?;
        let no_print = || format!("{name} has no print view.");
        let scene = match view {
            ViewMode::Assembled => &views.assembled,
            ViewMode::Print => views.print.as_ref().ok_or_else(no_print)?,
        };
        let out = export_path(&root, &model.path, view);
        if model.views_quality == quality {
            let objects = write_scene_3mf(scene, &out)?;
            return Ok(Some(export_summary(&root, &out, view, quality, objects)));
        }
        let job = Job::Export {
            view,
            quality,
            out,
            root,
        };
        model
            .jobs
            .send(job)
            .map_err(|_| format!("{name}: the build thread stopped; reopen the model"))?;
        let progress = format!("Exporting {name} with a {} mesh…", quality.label().to_lowercase());
        let _ = self.tx.send(ViewerMessage::Notice(Diagnostic::ok(progress)));
        Ok(None)
    }
}

/// A watcher plus the directories it currently watches.
struct WatchSet<W: Watcher> {
    watcher: W,
    current: BTreeMap<PathBuf, bool>,
}

impl<W: Watcher> WatchSet<W> {
    fn new(watcher: W) -> Self {
        Self {
            watcher,
            current: BTreeMap::new(),
        }
    }

    /// Updates the watches to [`watch_targets`].
    fn sync(&mut self, dependencies: &BTreeSet<PathBuf>, folder: Option<&Path>) -> notify::Result<()> {
        let wanted = watch_targets(dependencies, folder);
        for (dir, recursive) in &self.current {
            if wanted.get(dir) != Some(recursive) {
                // The directory may already be gone; nothing to clean up then.
                let _ = self.watcher.unwatch(dir);
            }
        }
        for (dir, recursive) in &wanted {
            if self.current.get(dir) != Some(recursive) {
                let mode = if *recursive {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                };
                self.watcher.watch(dir, mode)?;
            }
        }
        self.current = wanted;
        Ok(())
    }
}

/// A create/modify/remove (renames count as modify) touching a dependency.
pub fn is_relevant(event: &notify::Event, dependencies: &BTreeSet<PathBuf>) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) && event.paths.iter().any(|p| dependencies.contains(p))
}

/// A create, remove or rename under `folder`, which may add or drop entries
/// in [`list_models`]. Content edits never do.
pub fn may_change_listing(event: &notify::Event, folder: &Path) -> bool {
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
    ) && event.paths.iter().any(|p| p.starts_with(folder))
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
            Pipeline::new(self.root()).unwrap()
        }

        fn write(&self, relative: &str, source: &str) {
            let path = self.root().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        }

        /// Opens the model in a session and waits until it is built and watched.
        fn watch(&self) -> (Receiver<ViewerMessage>, Running) {
            watch_model(self.model())
        }
    }

    /// A session thread, plus the viewer's command channel keeping it alive.
    struct Running {
        commands: Sender<ViewerCommand>,
        handle: JoinHandle<notify::Result<()>>,
    }

    impl Running {
        fn send(&self, command: ViewerCommand) {
            self.commands.send(command).unwrap();
        }
    }

    fn start_session(start: Start) -> (Receiver<ViewerMessage>, Running) {
        start_session_with(start, Store::default())
    }

    fn start_session_with(start: Start, store: Store) -> (Receiver<ViewerMessage>, Running) {
        let (tx, rx) = mpsc::channel();
        let (commands, commands_rx) = mpsc::channel();
        let handle = thread::spawn(move || run_session(start, store, commands_rx, tx));
        (rx, Running { commands, handle })
    }

    /// Opens `model` (as main() does), waits for its first build, then gives
    /// the watcher a moment to register before the test edits anything.
    fn watch_model(model: PathBuf) -> (Receiver<ViewerMessage>, Running) {
        let (rx, running) = start_session(Start::Model(model));
        next_result(&rx).expect("first build");
        thread::sleep(Duration::from_millis(300));
        (rx, running)
    }

    fn cube_source(size: u32) -> String {
        format!(
            "import {{ cube }} from \"../dsl/cad.ts\";\nexport default () => cube({size});\n"
        )
    }

    /// The next evaluation result, skipping "loading" announcements and
    /// other messages.
    fn next_result(rx: &Receiver<ViewerMessage>) -> Result<SceneUpdate, RecvTimeoutError> {
        let deadline = Instant::now() + RELOAD_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if let ViewerMessage::Scene(update) = rx.recv_timeout(left)? {
                if update.loading.is_none() {
                    return Ok(update);
                }
            }
        }
    }

    /// The next message, within [`RELOAD_TIMEOUT`].
    fn next_message(rx: &Receiver<ViewerMessage>) -> ViewerMessage {
        rx.recv_timeout(RELOAD_TIMEOUT).expect("message")
    }

    /// The next message that `pick` accepts, skipping others.
    fn next_matching<T>(rx: &Receiver<ViewerMessage>, pick: impl Fn(ViewerMessage) -> Option<T>) -> T {
        let deadline = Instant::now() + RELOAD_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if let Some(found) = pick(rx.recv_timeout(left).expect("message")) {
                return found;
            }
        }
    }

    fn next_listing(rx: &Receiver<ViewerMessage>) -> FolderListing {
        next_matching(rx, |m| match m {
            ViewerMessage::Folder(listing) => Some(listing),
            _ => None,
        })
    }

    /// The next notice, skipping progress ones ("Exporting…").
    fn next_notice(rx: &Receiver<ViewerMessage>) -> Diagnostic {
        next_matching(rx, |m| match m {
            ViewerMessage::Notice(notice) if !notice.message.ends_with('…') => Some(notice),
            _ => None,
        })
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
    fn evaluates_model() {
        let project = Project::new(&cube_source(10));
        let update = project.pipeline().evaluate(&project.model());

        assert_eq!(update.diagnostics.len(), 1);
        let d = &update.diagnostics[0];
        assert!(d.ok, "{}", d.message);
        assert!(d.message.contains("model.ts OK (12 triangles"), "{}", d.message);
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
    fn kernel_errors_explain() {
        // A fillet radius bigger than the part.
        let project = Project::new(
            "import { cube, fillet } from \"../dsl/cad.ts\";
export default () => fillet(cube(2), 5);
",
        );
        let mut pipeline = Pipeline::new(project.root()).unwrap();
        let update = pipeline.evaluate(&project.model());

        assert!(update.views.is_none());
        let message = &update.diagnostics[0].message;
        assert!(message.starts_with("model.ts: ") && message.contains("too large"), "{message}");
    }

    #[test]
    fn missing_model_file_is_an_error() {
        let project = Project::new(&cube_source(10));
        let update = project.pipeline().evaluate(&project.root().join("models/nope.ts"));

        assert!(update.views.is_none());
        assert!(update.diagnostics[0].message.starts_with("nope.ts:"));
    }

    // --- CLI -----------------------------------------------------------------

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(std::iter::once("plate").chain(args.iter().copied())).map(Cli::into_command)
    }

    fn parse_open(args: &[&str]) -> OpenArgs {
        match parse(args).unwrap() {
            Command::Open(open) => open,
            other => panic!("expected open, got {other:?}"),
        }
    }

    fn parse_export(args: &[&str]) -> ExportArgs {
        match parse(args).unwrap() {
            Command::Export(export) => export,
            other => panic!("expected export, got {other:?}"),
        }
    }

    #[test]
    fn cli_path_is_optional() {
        assert_eq!(parse_open(&[]).path, None);
    }

    #[test]
    fn bare_profiles_show_as_thin_plates() {
        let project =
            Project::new("import { rect } from \"../dsl/cad.ts\";\nexport default () => rect({ width: 3, height: 2 });\n");
        let update = project.pipeline().evaluate(&project.model());
        assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
        assert_eq!(max_extent(&update), 3.0);
    }

    #[test]
    fn cli_open_is_the_default_subcommand() {
        for args in [
            &["models/bookshelf.ts"][..],
            &["open", "models/bookshelf.ts"][..],
        ] {
            assert_eq!(parse_open(args).path, Some(PathBuf::from("models/bookshelf.ts")));
        }
    }

    #[test]
    fn cli_rejects_unknown_flags_and_extra_args() {
        // There's one kernel, so no --kernel any more.
        let err = parse(&["--kernel", "occ"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);

        // clap reads a second path as a misplaced subcommand.
        let err = parse(&["a.ts", "b.ts"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn cli_help_documents_subcommands() {
        let help = |args: &[&str]| {
            let err = parse(args).unwrap_err();
            assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
            err.to_string()
        };
        let top = help(&["--help"]);
        for needle in ["plate", "open", "export", "init", "[PATH]"] {
            assert!(top.contains(needle), "help is missing {needle}:\n{top}");
        }
        let export = help(&["export", "--help"]);
        for needle in ["<MODEL>", "--output", "outputs/<model>_<view>.3mf", "--view", "--quality"] {
            assert!(export.contains(needle), "export help is missing {needle}:\n{export}");
        }
    }

    #[test]
    fn cli_export_flags() {
        let args = parse_export(&["export", "models/bookshelf.ts"]);
        assert_eq!(args.model, PathBuf::from("models/bookshelf.ts"));
        assert_eq!(args.output, None);
        assert_eq!(args.view, ExportView::Auto);

        let args = parse_export(&["export", "m.ts", "-o", "out.3mf", "--view", "assembled"]);
        assert_eq!(args.output, Some(PathBuf::from("out.3mf")));
        assert_eq!(args.view, ExportView::Assembled);

        // Export needs a model, and --view belongs to export.
        let err = parse(&["export"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
        assert!(parse(&["m.ts", "--view", "print"]).is_err());
    }

    #[test]
    fn cli_init_defaults_to_the_current_folder() {
        let Command::Init(args) = parse(&["init"]).unwrap() else { panic!("init") };
        assert_eq!(args.dir, PathBuf::from("."));
        let Command::Init(args) = parse(&["init", "shelves"]).unwrap() else { panic!("init") };
        assert_eq!(args.dir, PathBuf::from("shelves"));
    }

    // --- init ---------------------------------------------------------------

    #[test]
    fn init_writes_a_project_that_opens_builds_and_exports() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("new/shelves");
        let written = init_project(&project).unwrap();
        assert_eq!(
            written,
            ["dsl/cad.ts", "printer.toml", "models/cube.ts", "tsconfig.json", ".gitignore"].map(|p| project.join(p))
        );

        // A project root, with the binary's DSL and a P2S bed.
        let model = project.join("models/cube.ts");
        assert_eq!(project_root(&model), project);
        assert_eq!(std::fs::read_to_string(project.join("dsl/cad.ts")).unwrap(), DSL_SOURCE);
        let bed = load_printer_config(&project).unwrap().unwrap().bed;
        assert_eq!((bed.width, bed.depth, bed.height), (256.0, 256.0, 256.0));
        assert!(STARTER_PRINTER.contains("P2S"));

        // The starter cube builds, and exports to outputs/ by default.
        let mut pipeline = Pipeline::new(&project).unwrap();
        let update = pipeline.evaluate(&model);
        assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
        assert_eq!(max_extent(&update), 20.0);
        let summary = export_3mf(&mut pipeline, &model, ExportView::Auto, None).unwrap();
        assert!(project.join("outputs/cube_assembled.3mf").is_file(), "{summary}");
        assert_eq!(std::fs::read_to_string(project.join(".gitignore")).unwrap(), "/outputs\n");
    }

    #[test]
    fn init_refuses_to_overwrite_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("printer.toml"), "mine").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "unrelated").unwrap();

        let err = init_project(dir.path()).unwrap_err();
        assert!(err.contains("already has printer.toml"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.path().join("printer.toml")).unwrap(), "mine");
        assert!(!dir.path().join("dsl").exists() && !dir.path().join("models").exists());

        // Unrelated files are fine.
        std::fs::remove_file(dir.path().join("printer.toml")).unwrap();
        init_project(dir.path()).unwrap();
    }

    /// Object names in an exported 3MF, in order.
    fn exported_objects(path: &Path) -> Vec<String> {
        use std::io::Read;
        let mut archive = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
        let mut model = String::new();
        archive
            .by_name("3D/3dmodel.model")
            .unwrap()
            .read_to_string(&mut model)
            .unwrap();
        model
            .split("<object ")
            .skip(1)
            .map(|o| {
                let name = o.split("name=\"").nth(1).unwrap();
                name[..name.find('"').unwrap()].to_string()
            })
            .collect()
    }

    #[test]
    fn exports_bookshelf_print_view_by_default() {
        let out_dir = tempfile::tempdir().unwrap();
        let out = out_dir.path().join("shelf.3mf");
        let mut pipeline = Pipeline::new(&repo_root()).unwrap();
        let model = repo_root().join("models/bookshelf.ts");

        let summary = export_3mf(&mut pipeline, &model, ExportView::Auto, Some(&out)).unwrap();
        assert!(summary.contains("print view"), "{summary}");

        let names = exported_objects(&out);
        assert_eq!(names.first().map(String::as_str), Some("left side"));
        assert_eq!(names.last().map(String::as_str), Some("back panel"));
        assert!(names.iter().all(|n| n != cad_export::UNNAMED), "{names:?}");
        assert!(summary.contains(&format!("{} objects", names.len())), "{summary}");
    }

    #[test]
    fn export_view_selection() {
        let out_dir = tempfile::tempdir().unwrap();
        let mut pipeline = Pipeline::new(&repo_root()).unwrap();
        let cube = repo_root().join("models/cube.ts");

        // A bare-shape model exports its assembled view under auto...
        let out = out_dir.path().join("cube.3mf");
        let summary = export_3mf(&mut pipeline, &cube, ExportView::Auto, Some(&out)).unwrap();
        assert!(summary.contains("assembled view, 1 objects"), "{summary}");
        assert_eq!(exported_objects(&out), [cad_export::UNNAMED]);

        // ...but asking for print is an error, and nothing is written.
        let missing = out_dir.path().join("cube-print.3mf");
        let err = export_3mf(&mut pipeline, &cube, ExportView::Print, Some(&missing)).unwrap_err();
        assert!(err.contains("no print view"), "{err}");
        assert!(!missing.exists());
    }

    #[test]
    fn export_reports_model_errors_without_writing() {
        let project = Project::new(&cube_source(0));
        let out = project.root().join("broken.3mf");
        let err = export_3mf(&mut project.pipeline(), &project.model(), ExportView::Auto, Some(&out))
            .unwrap_err();
        assert!(err.contains("must be positive"), "{err}");
        assert!(!out.exists());
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

        let update = Pipeline::new(&root)
            .unwrap()
            .evaluate(&model);
        assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
    }

    #[test]
    fn first_eval_seeds_dependencies() {
        let project = Project::new(&cube_source(10));
        let mut pipeline = project.pipeline();
        assert!(pipeline.dependencies().is_empty());
        pipeline.evaluate(&project.model());
        assert!(pipeline.dependencies().contains(&project.model()));
    }

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn every_repo_model_builds_closed_solids() {
        let mut pipeline = Pipeline::new(&repo_root()).unwrap();
        for entry in std::fs::read_dir(repo_root().join("models")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "ts") {
                continue;
            }
            let update = pipeline.evaluate(&path);
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            assert!(update.diagnostics[0].ok, "{name}: {:?}", update.diagnostics);
            // Every view is closed solids; square.ts, a bare rect, is a thin plate.
            let views = update.views.unwrap();
            for scene in std::iter::once(&views.assembled).chain(views.print.as_ref()) {
                if let Some(print) = &views.print {
                    assert!(print.aabb().unwrap().min[2].abs() < 1e-3, "{name}: print floats");
                }
                for object in cad_export::objects(scene) {
                    assert!(object.is_closed(), "{name}: {} not closed", object.name);
                    assert!(object.signed_volume() > 0.0, "{name}: {} inside-out", object.name);
                }
            }
        }
    }

    /// Bookshelf views through the real script host and kernel.
    fn build_bookshelf_views(params: &str) -> SceneViews {
        let mut script = RustyScriptHost::new(repo_root()).unwrap();
        let source = format!(
            "import {{ bookshelf }} from \"./models/bookshelf.ts\";
             export default () => bookshelf({params});"
        );
        let views = script.eval_source("bookshelf_test.ts", &source).unwrap();
        SceneViews {
            assembled: OccKernel::default().build(&views.assembled).unwrap(),
            print: Some(OccKernel::default().build(&views.print.expect("print view")).unwrap()),
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

        let mut footprints: Vec<([f32; 3], [f32; 3])> = Vec::new();
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
            // ...and not overlapping any other board.
            for (min, max) in &footprints {
                let apart = aabb.max[0] <= min[0] + 1e-3
                    || max[0] <= aabb.min[0] + 1e-3
                    || aabb.max[1] <= min[1] + 1e-3
                    || max[1] <= aabb.min[1] + 1e-3;
                assert!(apart, "{aabb:?} overlaps {min:?}..{max:?}");
            }
            footprints.push((aabb.min, aabb.max));
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
    fn bookshelf_names_every_board_in_both_views() {
        let views = build_bookshelf_views("{ shelves: 2 }");
        let names = |scene: &cad_ir::TessellatedScene| -> Vec<String> {
            scene.parts.iter().map(|p| p.name.clone()).collect()
        };
        let expected = [
            "left side", "right side", "bottom", "top", "shelf 1", "shelf 2", "back panel",
        ];
        assert_eq!(names(&views.assembled), expected);
        assert_eq!(names(views.print.as_ref().unwrap()), expected);

        // Parts cover all geometry: every vertex belongs to exactly one board.
        for scene in [&views.assembled, views.print.as_ref().unwrap()] {
            let covered: u32 = scene.parts.iter().map(|p| p.vertices.len() as u32).sum();
            assert_eq!(covered as usize, scene.positions.len());
        }
    }

    // --- Printer config and bed-aware print views --------------------------------

    fn print_model(parts: &str) -> String {
        format!(
            "import {{ arrange, box, part }} from \"../dsl/cad.ts\";\n\
             export default () => ({{ assembled: box({{ width: 1, depth: 1, height: 1 }}), \
             print: arrange([{parts}]) }});\n"
        )
    }

    fn coaster(name: &str, size: u32) -> String {
        format!("part(\"{name}\", box({{ width: {size}, depth: {size}, height: 4 }}))")
    }

    fn bed_toml(width: u32, depth: u32, height: u32) -> String {
        format!("[bed]\nwidth = {width}\ndepth = {depth}\nheight = {height}\n")
    }

    #[test]
    fn printer_config_is_optional_and_validated() {
        let project = Project::new(&cube_source(1));
        assert_eq!(load_printer_config(project.root()), Ok(None));

        project.write("printer.toml", &bed_toml(256, 250, 200));
        let config = load_printer_config(project.root()).unwrap().unwrap();
        assert_eq!(
            config.bed,
            Bed {
                width: 256.0,
                depth: 250.0,
                height: 200.0
            }
        );

        for bad in [
            "[bed]\nwidth = 256\n",                              // missing fields
            "[bed]\nwidth = 0\ndepth = 1\nheight = 1\n",       // non-positive
            "[bed]\nwidth = 1\ndepth = 1\nheight = 1\nfoo = 2\n", // typo'd key
            "bed = ",                                             // not TOML
        ] {
            project.write("printer.toml", bad);
            let err = load_printer_config(project.root()).unwrap_err();
            assert!(err.starts_with("printer.toml:"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn default_bed_fills_only_unset_arranges() {
        let leaf = || ModelSpec::Rect {
            width: 1.0,
            height: 1.0,
        };
        let own = Bed {
            width: 1.0,
            depth: 2.0,
            height: 3.0,
        };
        let config = Bed {
            width: 9.0,
            depth: 9.0,
            height: 9.0,
        };
        let spec = ModelSpec::Group {
            children: vec![
                ModelSpec::Arrange {
                    gap: 1.0,
                    children: vec![leaf()],
                    bed: None,
                },
                ModelSpec::Color {
                    rgb: [0; 3],
                    shape: Box::new(ModelSpec::Arrange {
                        gap: 1.0,
                        children: vec![leaf()],
                        bed: Some(own),
                    }),
                },
            ],
        };
        let ModelSpec::Group { children } = with_default_bed(spec, config) else {
            panic!()
        };
        assert!(matches!(&children[0], ModelSpec::Arrange { bed: Some(b), .. } if *b == config));
        let ModelSpec::Color { shape, .. } = &children[1] else { panic!() };
        assert!(matches!(&**shape, ModelSpec::Arrange { bed: Some(b), .. } if *b == own));
    }

    #[test]
    fn print_view_packs_onto_configured_bed() {
        let parts: Vec<String> = (1..=5).map(|i| coaster(&format!("c{i}"), 90)).collect();
        let project = Project::new(&print_model(&parts.join(", ")));
        project.write("printer.toml", &bed_toml(256, 256, 256));

        let update = project.pipeline().evaluate(&project.model());
        assert!(update.diagnostics.iter().all(|d| !d.warning), "{:?}", update.diagnostics);
        let print = update.views.unwrap().print.unwrap();
        // 90 mm coasters, 10 mm gap: 4 per 256 mm plate.
        assert_eq!(print.plates.len(), 2);
        assert_eq!(print.plates[0].size, [256.0, 256.0]);
    }

    #[test]
    fn without_printer_config_print_view_has_no_plates() {
        let project = Project::new(&print_model(&coaster("c", 90)));
        let update = project.pipeline().evaluate(&project.model());
        assert!(update.views.unwrap().print.unwrap().plates.is_empty());
    }

    #[test]
    fn oversize_parts_get_one_warning_naming_them() {
        let project = Project::new(&print_model(&[
            coaster("small", 50),
            coaster("huge", 300),
            coaster("wide", 260),
        ]
        .join(", ")));
        project.write("printer.toml", &bed_toml(256, 256, 256));

        let update = project.pipeline().evaluate(&project.model());
        assert!(update.diagnostics[0].ok && !update.diagnostics[0].warning);
        let warnings: Vec<&str> = update
            .diagnostics
            .iter()
            .filter(|d| d.warning)
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let warning = warnings[0];
        assert!(warning.starts_with("2 parts don't fit the 256 × 256 × 256 mm bed"), "{warning}");
        assert!(warning.contains("huge (300 × 300 × 4)") && warning.contains("wide (260"), "{warning}");
        assert!(!warning.contains("small"), "{warning}");
        // A warning still shows the meshes.
        assert!(update.views.is_some());
    }

    #[test]
    fn bed_height_is_checked_too() {
        let project = Project::new(&print_model(
            "part(\"tower\", box({ width: 10, depth: 10, height: 300 }))",
        ));
        project.write("printer.toml", &bed_toml(256, 256, 256));
        let update = project.pipeline().evaluate(&project.model());
        assert!(
            update.diagnostics.iter().any(|d| d.warning && d.message.contains("tower")),
            "{:?}",
            update.diagnostics
        );
    }

    #[test]
    fn broken_printer_config_is_an_error_naming_the_file() {
        let project = Project::new(&print_model(&coaster("c", 90)));
        project.write("printer.toml", "[bed]\nwidth = \"wide\"\n");
        let update = project.pipeline().evaluate(&project.model());
        assert!(update.views.is_none());
        assert!(update.diagnostics[0].message.starts_with("printer.toml:"));
    }

    #[test]
    fn printer_config_is_a_dependency_even_before_it_exists() {
        let project = Project::new(&cube_source(1));
        let mut pipeline = project.pipeline();
        pipeline.evaluate(&project.model());
        assert!(pipeline.dependencies().contains(&project.root().join("printer.toml")));
    }

    #[test]
    fn editing_printer_config_relayouts_print_view() {
        let parts: Vec<String> = (1..=4).map(|i| coaster(&format!("c{i}"), 90)).collect();
        let project = Project::new(&print_model(&parts.join(", ")));
        project.write("printer.toml", &bed_toml(256, 256, 256));
        let (rx, _handle) = project.watch();

        // A smaller bed fits one coaster per plate.
        project.write("printer.toml", &bed_toml(120, 120, 120));
        let update = next_result(&rx).expect("reload");
        assert_eq!(update.views.unwrap().print.unwrap().plates.len(), 4);
    }

    #[test]
    fn export_summary_includes_warnings() {
        let project = Project::new(&print_model(&coaster("huge", 300)));
        project.write("printer.toml", &bed_toml(256, 256, 256));
        let out = project.root().join("out.3mf");
        let summary =
            export_3mf(&mut project.pipeline(), &project.model(), ExportView::Auto, Some(&out)).unwrap();
        assert!(summary.contains("\nwarning: 1 part doesn't fit"), "{summary}");
        assert!(out.exists());
    }

    // --- Coaster set (the printable example) ----------------------------------

    /// Temp project with the repo's coasters.ts, a 256 mm bed, and a model
    /// calling `coasters(params)`.
    fn coaster_project(params: &str) -> Project {
        let project = Project::new(&format!(
            "import {{ coasters }} from \"./coasters.ts\";\nexport default () => coasters({params});\n"
        ));
        let source = std::fs::read_to_string(repo_root().join("models/coasters.ts")).unwrap();
        project.write("models/coasters.ts", &source);
        project.write("printer.toml", &bed_toml(256, 256, 256));
        project
    }

    fn coaster_views(params: &str) -> (SceneViews, Vec<Diagnostic>) {
        let project = coaster_project(params);
        let update = project.pipeline().evaluate(&project.model());
        (update.views.expect("coasters build"), update.diagnostics)
    }

    fn part_bounds(scene: &TessellatedScene, name: &str) -> ([f32; 3], [f32; 3]) {
        let mut min = [f32::MAX; 3];
        let mut max = [f32::MIN; 3];
        let mut found = false;
        for part in scene.parts.iter().filter(|p| p.name == name) {
            found = true;
            for p in &scene.positions[part.vertices.start as usize..part.vertices.end as usize] {
                for i in 0..3 {
                    min[i] = min[i].min(p[i]);
                    max[i] = max[i].max(p[i]);
                }
            }
        }
        assert!(found, "no part {name:?}");
        (min, max)
    }

    #[test]
    fn coasters_fit_four_to_a_plate_without_warnings() {
        for (count, plates) in [(4, 1), (6, 2), (9, 3)] {
            let (views, diagnostics) = coaster_views(&format!("{{ count: {count}, holder: false }}"));
            assert!(diagnostics.iter().all(|d| d.ok && !d.warning), "{diagnostics:?}");
            assert_eq!(views.print.unwrap().plates.len(), plates, "{count} coasters");
        }
    }

    #[test]
    fn coaster_inlay_sits_on_its_base_inside_the_margin() {
        let (views, _) = coaster_views("{ count: 2, size: 95, base: 4, inlay: 1, margin: 8 }");
        for scene in [&views.assembled, views.print.as_ref().unwrap()] {
            for n in 1..=2 {
                let (base_min, base_max) = part_bounds(scene, &format!("coaster {n} base"));
                let (inlay_min, inlay_max) = part_bounds(scene, &format!("coaster {n} inlay"));
                assert!((inlay_min[2] - base_max[2]).abs() < 1e-3, "inlay {n} not on its base");
                assert!((inlay_max[2] - inlay_min[2] - 1.0).abs() < 1e-3);
                for i in 0..2 {
                    assert!(inlay_min[i] >= base_min[i] + 8.0 - 1e-3, "coaster {n} margin");
                    assert!(inlay_max[i] <= base_max[i] - 8.0 + 1e-3, "coaster {n} margin");
                }
            }
        }
    }

    #[test]
    fn coasters_stack_when_assembled_and_lie_flat_for_print() {
        let (views, _) = coaster_views("{ count: 4, base: 4, inlay: 1, holder: false }");
        let assembled = views.assembled.aabb().unwrap();
        assert!((assembled.max[2] - 4.0 * 5.0).abs() < 1e-3, "{assembled:?}");
        let print = views.print.unwrap().aabb().unwrap();
        assert!((print.max[2] - 5.0).abs() < 1e-3 && print.min[2].abs() < 1e-3, "{print:?}");
    }

    #[test]
    fn coasters_export_as_closed_multi_material_objects() {
        let (views, _) =
            coaster_views("{ count: 4, holder: false, inlayColors: [\"#e8b04f\", \"#4fa3a5\"] }");
        let objects = cad_export::objects(views.print.as_ref().unwrap());
        assert_eq!(objects.len(), 8, "a base and an inlay per coaster");
        for object in &objects {
            assert!(object.is_closed(), "{} is not watertight", object.name);
            assert_ne!(object.name, cad_export::UNNAMED);
        }
        let mut colors: Vec<_> = objects.iter().flat_map(|o| o.triangle_colors.clone()).collect();
        colors.sort();
        colors.dedup();
        assert_eq!(colors.len(), 3, "base + two inlay colors = three filaments");
    }

    #[test]
    fn holder_surrounds_the_stack_with_clearance_and_an_open_front() {
        let (views, diagnostics) =
            coaster_views("{ count: 4, size: 95, base: 4, inlay: 1, wall: 3, floor: 2, clearance: 1 }");
        assert!(diagnostics.iter().all(|d| d.ok && !d.warning), "{diagnostics:?}");
        let scene = &views.assembled;
        let (h_min, h_max) = part_bounds(scene, "holder");

        // 95 + 2 x 1 clearance + 2 x 3 walls wide; one back wall deep; floor + stack tall.
        for (got, want) in h_min.iter().chain(&h_max).zip([0.0, 0.0, 0.0, 103.0, 100.0, 22.0]) {
            assert!((got - want).abs() < 1e-3, "holder bounds {h_min:?}..{h_max:?}");
        }

        for n in 1..=4 {
            let (c_min, c_max) = part_bounds(scene, &format!("coaster {n} base"));
            // Inside the walls with exactly the clearance on the sides and back.
            assert!((c_min[0] - 4.0).abs() < 1e-3 && (c_max[0] - 99.0).abs() < 1e-3, "coaster {n}");
            assert!((c_max[1] - 96.0).abs() < 1e-3, "coaster {n} back clearance");
        }
        // The stack rests on the floor.
        let (bottom, _) = part_bounds(scene, "coaster 1 base");
        assert!((bottom[2] - 2.0).abs() < 1e-3, "{bottom:?}");

        // Open front: above the floor, no holder vertex inside the opening
        // (x 3..100, y 0..97), so the coasters slide out.
        let holder = scene.parts.iter().find(|p| p.name == "holder").unwrap();
        let vertices = &scene.positions[holder.vertices.start as usize..holder.vertices.end as usize];
        for p in vertices {
            let in_opening = p[0] > 3.0 + 1e-3 && p[0] < 100.0 - 1e-3 && p[1] < 97.0 - 1e-3;
            assert!(!in_opening || p[2] <= 2.0 + 1e-3, "holder geometry at {p:?} blocks the opening");
        }
        // ...and there is a floor: its top face's corners at z = 2 span the opening.
        let floor_corners = [[3.0, 0.0], [100.0, 0.0], [3.0, 97.0], [100.0, 97.0]];
        for [x, y] in floor_corners {
            let found = vertices
                .iter()
                .any(|p| (p[0] - x).abs() < 1e-3 && (p[1] - y).abs() < 1e-3 && (p[2] - 2.0).abs() < 1e-3);
            assert!(found, "floor corner ({x}, {y}, 2) missing");
        }
    }

    #[test]
    fn holder_prints_closed_with_the_coasters() {
        let (views, diagnostics) = coaster_views("{ count: 4 }");
        assert!(diagnostics.iter().all(|d| d.ok && !d.warning), "{diagnostics:?}");
        let print = views.print.unwrap();
        // Holder + 4 coasters at ~100 mm each: two plates on a 256 mm bed.
        assert_eq!(print.plates.len(), 2);

        let objects = cad_export::objects(&print);
        let holder = objects.iter().find(|o| o.name == "holder").expect("holder object");
        assert!(holder.is_closed(), "holder must be watertight");
        assert_eq!(objects.len(), 9);
        // Holder uses the base color: still three filaments.
        let mut colors: Vec<_> = objects.iter().flat_map(|o| o.triangle_colors.clone()).collect();
        colors.sort();
        colors.dedup();
        assert_eq!(colors.len(), 3);
    }

    #[test]
    fn holder_can_be_left_out() {
        let (views, _) = coaster_views("{ holder: false }");
        assert!(views.assembled.parts.iter().all(|p| p.name != "holder"));
        assert!(views.print.unwrap().parts.iter().all(|p| p.name != "holder"));
    }

    #[test]
    fn coaster_params_are_validated() {
        for (params, message) in [
            ("{ count: 0 }", "count must be"),
            ("{ stripes: 2.5 }", "stripes must be"),
            ("{ size: 10, margin: 8 }", "leaves no room"),
            ("{ inlayColors: [] }", "at least one color"),
            ("{ wall: 0 }", "wall and floor must be positive"),
            ("{ floor: 0 }", "wall and floor must be positive"),
        ] {
            let project = coaster_project(params);
            let update = project.pipeline().evaluate(&project.model());
            assert!(update.views.is_none(), "{params} accepted");
            assert!(update.diagnostics[0].message.contains(message), "{params}: {:?}", update.diagnostics);
        }
    }

    // --- Desk organizer (shows booleans, revolve, cutout, slanted extrude) -----

    fn organizer_views() -> (SceneViews, Vec<Diagnostic>) {
        let project = Project::new("import { organizer } from \"./desk_organizer.ts\";
export default () => organizer();
");
        let source = std::fs::read_to_string(repo_root().join("models/desk_organizer.ts")).unwrap();
        project.write("models/desk_organizer.ts", &source);
        project.write("printer.toml", &bed_toml(256, 256, 256));
        let mut pipeline = Pipeline::new(project.root()).unwrap();
        let update = pipeline.evaluate(&project.model());
        (update.views.expect("organizer builds"), update.diagnostics)
    }

    fn part_vertices<'a>(scene: &'a cad_ir::TessellatedScene, name: &str) -> Vec<&'a [f32; 3]> {
        scene
            .parts
            .iter()
            .filter(|p| p.name == name)
            .flat_map(|p| &scene.positions[p.vertices.start as usize..p.vertices.end as usize])
            .collect()
    }

    #[test]
    fn organizer_corners_are_rounded_and_cup_is_chamfered() {
        let (views, _) = organizer_views();
        let scene = &views.assembled;

        // Plate and holder: no vertex sits on their bounding-box corners any more.
        for name in ["base plate", "card holder"] {
            let (min, max) = part_bounds(scene, name);
            for [x, y] in [[min[0], min[1]], [max[0], min[1]], [min[0], max[1]]] {
                let sharp = part_vertices(scene, name)
                    .iter()
                    .any(|p| (p[0] - x).abs() < 1e-3 && (p[1] - y).abs() < 1e-3);
                assert!(!sharp, "{name} still has a sharp corner at ({x}, {y})");
            }
        }

        // Cup: 0.8 mm first-layer chamfer, so its footprint is 34.2 mm in radius
        // while its wall is the full 35 mm.
        let cup = part_vertices(scene, "pen cup");
        let (min, max) = part_bounds(scene, "pen cup");
        let center = [(min[0] + max[0]) / 2.0, (min[1] + max[1]) / 2.0];
        let radius_at = |z: f32| {
            cup.iter()
                .filter(|p| (p[2] - z).abs() < 1e-3)
                .map(|p| ((p[0] - center[0]).powi(2) + (p[1] - center[1]).powi(2)).sqrt())
                .fold(0.0f32, f32::max)
        };
        assert!((radius_at(min[2]) - 34.2).abs() < 0.02, "footprint radius {}", radius_at(min[2]));
        assert!((radius_at(max[2]) - 35.0).abs() < 0.02, "rim radius {}", radius_at(max[2]));
    }

    #[test]
    fn organizer_prints_on_one_plate_as_three_closed_parts() {
        let (views, diagnostics) = organizer_views();
        assert!(diagnostics.iter().all(|d| d.ok && !d.warning), "{diagnostics:?}");
        let print = views.print.unwrap();
        assert_eq!(print.plates.len(), 1);
        let objects = cad_export::objects(&print);
        let names: Vec<&str> = objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["base plate", "pen cup", "card holder"]);
        for object in &objects {
            assert!(object.is_closed(), "{} is not watertight", object.name);
        }
    }

    #[test]
    fn organizer_pieces_sit_on_the_plate_when_assembled() {
        let (views, _) = organizer_views();
        let scene = &views.assembled;
        let (_, plate_max) = part_bounds(scene, "base plate");
        for piece in ["pen cup", "card holder"] {
            let (min, _) = part_bounds(scene, piece);
            assert!((min[2] - plate_max[2]).abs() < 1e-3, "{piece} floats or sinks: {min:?}");
        }
        // The cup is round: as wide as it is deep, 70 mm across.
        let (min, max) = part_bounds(scene, "pen cup");
        assert!(((max[0] - min[0]) - 70.0).abs() < 0.05 && ((max[1] - min[1]) - 70.0).abs() < 0.05);
    }

    #[test]
    fn rounded_box_has_even_3mm_walls_and_rounded_corners() {
        use std::f64::consts::PI;
        let project = Project::new(
            "import { roundedBox } from \"./rounded_box.ts\";
             export default () => roundedBox({ width: 60, depth: 40, height: 30, wall: 3, radius: 6 });
",
        );
        let source = std::fs::read_to_string(repo_root().join("models/rounded_box.ts")).unwrap();
        project.write("models/rounded_box.ts", &source);
        let mut pipeline = Pipeline::new(project.root()).unwrap();
        let update = pipeline.evaluate(&project.model());
        let scene = update.views.expect("builds").assembled;

        // Exact volume: rounded outer block minus the rounded cavity above the floor.
        let rounded_rect = |w: f64, d: f64, r: f64| w * d - (4.0 - PI) * r * r;
        let volume = rounded_rect(60.0, 40.0, 6.0) * 30.0 - rounded_rect(54.0, 34.0, 3.0) * 27.0;
        let object = &cad_export::objects(&scene)[0];
        assert!(object.is_closed());
        assert!((object.signed_volume() - volume).abs() < volume * 0.005, "{} vs {volume}", object.signed_volume());

        // The cavity's flat walls sit exactly 3 mm in from each outside face,
        // and its floor is 3 mm up.
        let has = |pred: &dyn Fn(&[f32; 3]) -> bool| scene.positions.iter().any(|p| pred(p));
        assert!(has(&|p| (p[0] - 3.0).abs() < 1e-3 && p[2] > 3.5), "inner wall at x = 3");
        assert!(has(&|p| (p[0] - 57.0).abs() < 1e-3 && p[2] > 3.5), "inner wall at x = 57");
        assert!(has(&|p| (p[1] - 3.0).abs() < 1e-3 && p[2] > 3.5), "inner wall at y = 3");
        assert!(has(&|p| (p[1] - 37.0).abs() < 1e-3 && p[2] > 3.5), "inner wall at y = 37");
        // Floor top at z = 3: its straight edges run x = 6..54 (inside the
        // 3 mm inner corner radius); flat faces only have boundary vertices.
        assert!(has(&|p| (p[2] - 3.0).abs() < 1e-3 && (p[0] - 6.0).abs() < 1e-3), "floor top at z = 3");
        // Rounded: no outer corner is sharp.
        assert!(!has(&|p| p[0].abs() < 1e-3 && p[1].abs() < 1e-3), "outer corner still sharp");

        // A radius no bigger than the wall can't round the inside.
        project.write_model(
            "import { roundedBox } from \"./rounded_box.ts\";
export default () => roundedBox({ wall: 3, radius: 3 });
",
        );
        let update = pipeline.evaluate(&project.model());
        assert!(update.diagnostics[0].message.contains("must be larger than the wall"), "{:?}", update.diagnostics);
    }

    #[test]
    fn bookshelf_defaults_are_millimeters() {
        // A real bookshelf is around a meter wide; a cm-scale model would be ~100.
        let aabb = build_bookshelf("{}").aabb().unwrap();
        assert!(aabb.max[0] >= 500.0, "{:?}", aabb.max);
    }

    #[test]
    fn pipeline_reports_print_view() {
        let mut pipeline = Pipeline::new(&repo_root()).unwrap();
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

    // --- Dependencies and event filtering -------------------------------------

    fn deps(paths: &[&str]) -> BTreeSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn dependency_changes_are_relevant() {
        let deps = deps(&["models/cube.ts", "dsl/cad.ts"]);
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        assert!(is_relevant(&event(modify, "models/cube.ts"), &deps));
        assert!(is_relevant(&event(EventKind::Create(CreateKind::File), "dsl/cad.ts"), &deps));
        assert!(is_relevant(
            &event(EventKind::Remove(notify::event::RemoveKind::File), "dsl/cad.ts"),
            &deps
        ));
        let rename_onto = EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::To));
        assert!(is_relevant(&event(rename_onto, "models/cube.ts"), &deps));
    }

    #[test]
    fn non_dependencies_and_reads_are_ignored() {
        let deps = deps(&["models/cube.ts", "dsl/cad.ts"]);
        let modify = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        assert!(!is_relevant(&event(modify, "models/other.ts"), &deps));
        assert!(!is_relevant(&event(modify, "models/notes.md"), &deps));
        assert!(!is_relevant(&event(EventKind::Access(AccessKind::Any), "models/cube.ts"), &deps));
    }

    #[test]
    fn watch_targets_are_dependency_folders() {
        let project = Project::new(&cube_source(10));
        let root = project.root();
        project.write("lib/a.ts", "");
        project.write("lib/b.ts", "");

        let targets = watch_targets(
            &BTreeSet::from([
                project.model(),
                root.join("dsl/cad.ts"),
                root.join("lib/a.ts"),
                root.join("lib/b.ts"),
            ]),
            None,
        );
        assert_eq!(
            targets,
            BTreeMap::from([
                (root.join("dsl"), false),
                (root.join("lib"), false),
                (root.join("models"), false),
            ])
        );
    }

    #[test]
    fn missing_folders_are_watched_from_nearest_existing_ancestor() {
        let project = Project::new(&cube_source(10));
        let root = project.root();
        let targets = watch_targets(
            &BTreeSet::from([project.model(), root.join("parts/new/thing.ts")]),
            None,
        );
        assert_eq!(
            targets,
            BTreeMap::from([(root.to_path_buf(), true), (root.join("models"), false)])
        );
    }

    #[test]
    fn pipeline_tracks_imports_and_keeps_them_across_failures() {
        let project = Project::new(
            "import { box } from \"../lib/parts.ts\";\nexport default () => box();\n",
        );
        project.write(
            "lib/parts.ts",
            "import { cube } from \"../dsl/cad.ts\";\nexport const box = () => cube(2);\n",
        );
        let mut pipeline = project.pipeline();
        let all = BTreeSet::from([
            project.model(),
            project.root().join("lib/parts.ts"),
            project.root().join("dsl/cad.ts"),
            project.root().join("printer.toml"),
        ]);

        assert!(pipeline.evaluate(&project.model()).diagnostics[0].ok);
        assert_eq!(pipeline.dependencies(), &all);

        // Broken lib: the eval stops before reaching dsl/cad.ts, but the old
        // set is kept so fixing either file triggers a reload.
        project.write("lib/parts.ts", "export const box = () => {{;\n");
        assert!(!pipeline.evaluate(&project.model()).diagnostics[0].ok);
        assert_eq!(pipeline.dependencies(), &all);

        // Success replaces the set: the lib import is gone.
        project.write_model(&cube_source(1));
        assert!(pipeline.evaluate(&project.model()).diagnostics[0].ok);
        assert_eq!(
            pipeline.dependencies(),
            &BTreeSet::from([
                project.model(),
                project.root().join("dsl/cad.ts"),
                project.root().join("printer.toml"),
            ])
        );
    }

    // --- Hot reload (real file watcher, no window) ---------------------------

    #[test]
    fn reloads_when_model_changes() {
        let project = Project::new(&cube_source(10));
        let (rx, _handle) = project.watch();

        project.write_model(&cube_source(20));

        let update = next_result(&rx).expect("reload");
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

        let update = next_result(&rx).expect("reload");
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

        let update = next_result(&rx).expect("reload");
        assert_eq!(max_extent(&update), 15.0);
        assert!(rx.recv_timeout(QUIET).is_err(), "burst should debounce to one reload");
    }

    #[test]
    fn recovers_after_script_error() {
        let project = Project::new(&cube_source(10));
        let (rx, _handle) = project.watch();

        project.write_model(&cube_source(0));
        let broken = next_result(&rx).expect("error update");
        assert!(broken.views.is_none());
        assert!(!broken.diagnostics[0].ok);

        project.write_model(&cube_source(7));
        let fixed = next_result(&rx).expect("recovery");
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

        let (rx, _handle) = watch_model(model.clone());

        std::fs::write(&model, cube_source(4)).unwrap();

        let update = next_result(&rx).expect("reload");
        assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
        assert_eq!(max_extent(&update), 4.0);
    }

    /// Model importing `../lib/parts.ts`, which builds a cube of `size`.
    fn project_with_lib(size: u32) -> Project {
        let project = Project::new(
            "import { part } from \"../lib/parts.ts\";\nexport default () => part();\n",
        );
        project.write("lib/parts.ts", &lib_source(size));
        project
    }

    fn lib_source(size: u32) -> String {
        format!("import {{ cube }} from \"../dsl/cad.ts\";\nexport const part = () => cube({size});\n")
    }

    #[test]
    fn reloads_when_imported_lib_changes() {
        // lib/ is neither models/ nor dsl/: only import tracking can see it.
        let project = project_with_lib(2);
        let (rx, _handle) = project.watch();

        project.write("lib/parts.ts", &lib_source(5));

        let update = next_result(&rx).expect("reload");
        assert_eq!(max_extent(&update), 5.0);
    }

    #[test]
    fn newly_added_import_is_watched() {
        let project = Project::new(&cube_source(1));
        project.write("lib/parts.ts", &lib_source(3));
        let (rx, _handle) = project.watch();

        project.write_model(
            "import { part } from \"../lib/parts.ts\";\nexport default () => part();\n",
        );
        assert_eq!(max_extent(&next_result(&rx).expect("reload")), 3.0);

        project.write("lib/parts.ts", &lib_source(4));
        assert_eq!(max_extent(&next_result(&rx).expect("lib reload")), 4.0);
    }

    #[test]
    fn unrelated_model_edit_does_not_reload() {
        let project = Project::new(&cube_source(1));
        project.write("models/other.ts", &cube_source(2));
        let (rx, _handle) = project.watch();

        project.write("models/other.ts", &cube_source(3));

        assert!(rx.recv_timeout(QUIET).is_err(), "edit to a non-dependency reloaded");
    }

    #[test]
    fn recovers_when_broken_import_is_fixed() {
        let project = project_with_lib(2);
        let (rx, _handle) = project.watch();

        project.write("lib/parts.ts", "export const part = ( => ;\n");
        let broken = next_result(&rx).expect("error update");
        assert!(!broken.diagnostics[0].ok);

        project.write("lib/parts.ts", &lib_source(6));
        let fixed = next_result(&rx).expect("recovery");
        assert!(fixed.diagnostics[0].ok, "{:?}", fixed.diagnostics);
        assert_eq!(max_extent(&fixed), 6.0);
    }

    #[test]
    fn reloads_when_missing_import_is_created() {
        let project = Project::new(
            "import { part } from \"../parts/new/thing.ts\";\nexport default () => part();\n",
        );
        let (rx, _handle) = project.watch();

        project.write("parts/new/thing.ts", &lib_source(7).replace("../dsl", "../../dsl"));

        let update = next_result(&rx).expect("reload");
        assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
        assert_eq!(max_extent(&update), 7.0);
    }

    #[test]
    fn reloads_on_atomic_rename_save() {
        let project = Project::new(&cube_source(1));
        let (rx, _handle) = project.watch();

        // How many editors save: write a temp file, then rename it over the original.
        let temp = project.root().join("models/.model.ts.tmp");
        std::fs::write(&temp, cube_source(8)).unwrap();
        std::fs::rename(&temp, project.model()).unwrap();

        assert_eq!(max_extent(&next_result(&rx).expect("reload")), 8.0);

        // The watch survived the replace: a second save still reloads.
        std::fs::write(&temp, cube_source(9)).unwrap();
        std::fs::rename(&temp, project.model()).unwrap();
        assert_eq!(max_extent(&next_result(&rx).expect("second reload")), 9.0);
    }

    #[test]
    fn opening_a_model_lists_its_project_then_announces_delivers_and_watches() {
        let project = Project::new(&cube_source(2));
        let (rx, _running) = start_session(Start::Model(project.model()));

        let ViewerMessage::Folder(listing) = next_message(&rx) else { panic!("folder first") };
        assert_eq!(listing.path, project.root());
        assert!(matches!(next_message(&rx), ViewerMessage::Model(None)));
        let ViewerMessage::Model(Some(open)) = next_message(&rx) else { panic!("then the model") };
        assert_eq!(open, project.model());

        let ViewerMessage::Scene(first) = next_message(&rx) else { panic!("loading") };
        assert_eq!(first.loading.as_deref(), Some("Loading model.ts…"));
        let ViewerMessage::Scene(result) = next_message(&rx) else { panic!("result") };
        assert!(result.loading.is_none());
        assert_eq!(max_extent(&result), 2.0);

        // Then it's watching: an edit is announced as a rebuild before its result.
        thread::sleep(Duration::from_millis(300));
        project.write_model(&cube_source(3));
        let ViewerMessage::Scene(announce) = next_message(&rx) else { panic!("announce") };
        assert_eq!(announce.loading.as_deref(), Some("Rebuilding model.ts…"));
        let ViewerMessage::Scene(rebuilt) = next_message(&rx) else { panic!("rebuilt") };
        assert_eq!(max_extent(&rebuilt), 3.0);
    }

        #[test]
    fn stops_once_viewer_is_gone() {
        let project = Project::new(&cube_source(10));
        let (rx, running) = project.watch();
        drop(rx);

        project.write_model(&cube_source(11));
        assert_stops(running.handle);
    }

    fn assert_stops(handle: JoinHandle<notify::Result<()>>) {
        let deadline = Instant::now() + RELOAD_TIMEOUT;
        while !handle.is_finished() {
            assert!(Instant::now() < deadline, "session thread kept running");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn stops_when_viewer_drops_its_command_channel() {
        // Closing the window drops both ends; this one alone is enough, even
        // with no file changes to notice the dead message channel by.
        let project = Project::new(&cube_source(10));
        let (_rx, running) = project.watch();
        drop(running.commands);
        assert_stops(running.handle);
    }

    // --- Folders, file list and export from the viewer ----------------------

    fn names(entries: &[FileEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| match e {
                FileEntry::Dir { name, children } => format!("{name}/[{}]", names(children).join(", ")),
                FileEntry::File { name, .. } => name.clone(),
            })
            .collect()
    }

    #[test]
    fn list_models_shows_folders_then_ts_files_sorted() {
        let project = Project::new(&cube_source(1));
        project.write("models/Bracket.ts", "");
        project.write("models/parts/leg.ts", "");
        project.write("models/parts/deep/er/nut.ts", "");
        project.write("models/parts/notes.md", "");
        project.write("models/types.d.ts", "");
        project.write("outputs/model_print.3mf", "");
        project.write("zebra.ts", "");
        std::fs::create_dir_all(project.root().join("empty")).unwrap();

        assert_eq!(
            names(&list_models(project.root())),
            [
                "dsl/[cad.ts]",
                "models/[parts/[deep/[er/[nut.ts]], leg.ts], Bracket.ts, model.ts]",
                "zebra.ts",
            ]
        );
        let FileEntry::File { path, .. } = &list_models(&project.root().join("models"))[1] else {
            panic!("file")
        };
        assert_eq!(path, &project.root().join("models/Bracket.ts"));
    }

    #[test]
    fn folder_root_is_the_enclosing_project() {
        let project = Project::new(&cube_source(1));
        assert_eq!(folder_root(project.root()), project.root());
        assert_eq!(folder_root(&project.root().join("models")), project.root());
        let loose = tempfile::tempdir().unwrap();
        assert_eq!(folder_root(loose.path()), loose.path());
    }

    #[test]
    fn resolve_start_tells_folders_from_models() {
        let project = Project::new(&cube_source(1));
        assert_eq!(
            resolve_start(Path::new("models"), project.root()),
            Ok(Start::Folder(project.root().join("models")))
        );
        assert_eq!(
            resolve_start(Path::new("models/../models/model.ts"), project.root()),
            Ok(Start::Model(project.model()))
        );
        assert!(resolve_start(Path::new("nope.ts"), project.root()).unwrap_err().contains("not found"));
    }

    #[test]
    fn export_path_is_outputs_model_view() {
        let root = Path::new("/p");
        assert_eq!(
            export_path(root, Path::new("/p/models/shelf.ts"), ViewMode::Print),
            Path::new("/p/outputs/shelf_print.3mf")
        );
        assert_eq!(
            export_path(root, Path::new("/p/a/b.c.ts"), ViewMode::Assembled),
            Path::new("/p/outputs/b.c_assembled.3mf")
        );
    }

    #[test]
    fn opened_folder_covers_watches_inside_it() {
        let project = Project::new(&cube_source(10));
        let root = project.root();
        let deps = BTreeSet::from([project.model(), root.join("dsl/cad.ts"), root.join("lib/a.ts")]);
        project.write("lib/a.ts", "");

        assert_eq!(
            watch_targets(&deps, Some(root)),
            BTreeMap::from([(root.to_path_buf(), true)])
        );
        // A subfolder: dependencies outside it still need their own watches.
        assert_eq!(
            watch_targets(&deps, Some(&root.join("models"))),
            BTreeMap::from([
                (root.join("dsl"), false),
                (root.join("lib"), false),
                (root.join("models"), true),
            ])
        );
    }

    #[test]
    fn only_structural_changes_in_the_folder_can_change_the_listing() {
        let folder = Path::new("/p");
        let rename = EventKind::Modify(ModifyKind::Name(notify::event::RenameMode::To));
        let edit = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        assert!(may_change_listing(&event(EventKind::Create(CreateKind::File), "/p/a.ts"), folder));
        assert!(may_change_listing(&event(EventKind::Remove(notify::event::RemoveKind::Any), "/p/x"), folder));
        assert!(may_change_listing(&event(rename, "/p/m/b.ts"), folder));
        assert!(!may_change_listing(&event(edit, "/p/a.ts"), folder));
        assert!(!may_change_listing(&event(EventKind::Create(CreateKind::File), "/q/a.ts"), folder));
    }

    #[test]
    fn opening_a_folder_lists_it_with_no_model() {
        let project = Project::new(&cube_source(1));
        let (rx, running) = start_session(Start::Empty);
        running.send(ViewerCommand::OpenFolder(project.root().join("models")));

        let ViewerMessage::Folder(listing) = next_message(&rx) else { panic!("folder") };
        assert_eq!(listing.path, project.root().join("models"));
        assert_eq!(names(&listing.entries), ["model.ts"]);
        assert!(matches!(next_message(&rx), ViewerMessage::Model(None)));
        // Nothing is built until a model is picked.
        assert!(rx.recv_timeout(QUIET).is_err());
    }

    /// A cube whose script spins for `ms` first, standing in for a slow build.
    fn slow_cube_source(size: u32, ms: u32) -> String {
        format!(
            "import {{ cube }} from \"../dsl/cad.ts\";
export default () => {{
  const end = Date.now() + {ms};
  while (Date.now() < end) {{}}
  return cube({size});
}};
"
        )
    }

    /// Waits for the "Loading…"/"Rebuilding…" announcement.
    fn next_loading(rx: &Receiver<ViewerMessage>) {
        next_matching(rx, |m| match m {
            ViewerMessage::Scene(update) => update.loading,
            _ => None,
        });
    }

    #[test]
    fn commands_are_handled_while_a_model_builds() {
        let project = Project::new(&slow_cube_source(5, 3000));
        project.write("models/fast.ts", &cube_source(20));
        let (rx, running) = start_session(Start::Model(project.model()));
        next_loading(&rx);

        // Opening another model doesn't wait for the slow build...
        let asked = Instant::now();
        running.send(ViewerCommand::OpenModel(project.root().join("models/fast.ts")));
        let update = next_result(&rx).unwrap();
        assert_eq!(max_extent(&update), 20.0, "{:?}", update.diagnostics);
        assert!(asked.elapsed() < Duration::from_millis(2500), "waited {:?}", asked.elapsed());

        // ...and the slow build's result, when it lands, is dropped.
        let deadline = Instant::now() + Duration::from_secs(3) + QUIET;
        while let Ok(message) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if let ViewerMessage::Scene(update) = message {
                assert!(update.views.is_none(), "a closed model's build was shown");
            }
        }
    }

    #[test]
    fn builds_asked_for_during_a_build_collapse_into_the_newest() {
        let project = Project::new(&slow_cube_source(5, 1500));
        let (rx, running) = start_session(Start::Model(project.model()));
        next_loading(&rx);

        let started = Instant::now();
        let mut settings = Settings::default();
        for quality in [MeshQuality::Draft, MeshQuality::Fine] {
            settings.view_quality = quality;
            running.send(ViewerCommand::Settings(settings));
        }
        // The first build is overtaken, and Draft is skipped for Fine: one
        // result, from the second build to run (three would take ~4.5 s).
        let update = next_matching(&rx, |m| match m {
            ViewerMessage::Scene(update) if update.loading.is_none() => Some(update),
            _ => None,
        });
        assert!(update.diagnostics[0].message.contains("fine mesh"), "{:?}", update.diagnostics);
        assert!(started.elapsed() < Duration::from_millis(4200), "took {:?}", started.elapsed());
        assert!(
            !matches!(rx.recv_timeout(QUIET), Ok(ViewerMessage::Scene(_))),
            "a superseded build was shown"
        );
    }

    #[test]
    fn new_project_sets_up_the_folder_and_opens_its_starter_model() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("shelf");
        let (rx, running) = start_session(Start::Empty);
        running.send(ViewerCommand::NewProject(folder.clone()));

        let listing = next_listing(&rx);
        assert_eq!(listing.path, folder);
        assert_eq!(names(&listing.entries), ["dsl/[cad.ts]", "models/[cube.ts]"]);
        let model = next_matching(&rx, |m| match m {
            ViewerMessage::Model(Some(model)) => Some(model),
            _ => None,
        });
        assert_eq!(model, folder.join(STARTER_MODEL_PATH));
        // The notice comes straight after the open; the build finishes later.
        let notice = next_notice(&rx);
        assert!(notice.ok && notice.message.starts_with("Created a project in"), "{notice:?}");
        let update = next_result(&rx).unwrap();
        assert!(update.diagnostics[0].ok, "{:?}", update.diagnostics);
        assert_eq!(max_extent(&update), 20.0);
        assert!(folder.join(PRINTER_CONFIG).is_file());
    }

    #[test]
    fn new_project_in_an_existing_project_only_explains() {
        let project = Project::new(&cube_source(1));
        let (rx, running) = start_session(Start::Folder(project.root().to_path_buf()));
        next_listing(&rx);

        running.send(ViewerCommand::NewProject(project.root().to_path_buf()));
        let notice = next_notice(&rx);
        assert!(!notice.ok && notice.message.contains("already has dsl/cad.ts"), "{notice:?}");
        assert!(!project.root().join(STARTER_MODEL_PATH).exists());
        assert!(!project.root().join(PRINTER_CONFIG).exists());
    }

    #[test]
    fn opening_a_missing_folder_is_a_notice() {
        let project = Project::new(&cube_source(1));
        let (rx, running) = start_session(Start::Empty);
        running.send(ViewerCommand::OpenFolder(project.root().join("nope")));
        let notice = next_notice(&rx);
        assert!(!notice.ok && notice.message.contains("is not a folder"), "{notice:?}");
    }

    #[test]
    fn file_list_follows_files_added_and_removed_but_not_edits() {
        let project = Project::new(&cube_source(1));
        let (rx, _running) = start_session(Start::Folder(project.root().to_path_buf()));
        next_listing(&rx);
        thread::sleep(Duration::from_millis(300));

        project.write("models/parts/new.ts", &cube_source(2));
        assert_eq!(
            names(&next_listing(&rx).entries),
            ["dsl/[cad.ts]", "models/[parts/[new.ts], model.ts]"]
        );

        // Editing a file, or adding a non-model file, changes nothing listed.
        project.write("models/parts/new.ts", &cube_source(3));
        project.write("models/notes.md", "hi");
        assert!(rx.recv_timeout(QUIET).is_err(), "listing resent without a change");

        std::fs::remove_file(project.root().join("models/parts/new.ts")).unwrap();
        assert_eq!(names(&next_listing(&rx).entries), ["dsl/[cad.ts]", "models/[model.ts]"]);
    }

    #[test]
    fn picking_a_model_builds_it_and_switching_stops_the_old_ones_reloads() {
        let project = Project::new(&cube_source(1));
        project.write("models/other.ts", &cube_source(2));
        let (rx, running) = start_session(Start::Folder(project.root().to_path_buf()));

        running.send(ViewerCommand::OpenModel(project.model()));
        assert_eq!(max_extent(&next_result(&rx).unwrap()), 1.0);
        running.send(ViewerCommand::OpenModel(project.root().join("models/other.ts")));
        assert_eq!(max_extent(&next_result(&rx).unwrap()), 2.0);
        thread::sleep(Duration::from_millis(300));

        project.write_model(&cube_source(5));
        assert!(rx.recv_timeout(QUIET).is_err(), "closed model still reloads");
        project.write("models/other.ts", &cube_source(6));
        assert_eq!(max_extent(&next_result(&rx).unwrap()), 6.0);
    }

    #[test]
    fn dependencies_outside_an_opened_subfolder_still_reload() {
        let project = Project::new(&cube_source(10));
        let (rx, running) = start_session(Start::Folder(project.root().join("models")));
        running.send(ViewerCommand::OpenModel(project.model()));
        assert_eq!(max_extent(&next_result(&rx).unwrap()), 10.0);
        thread::sleep(Duration::from_millis(300));

        let dsl = project.root().join("dsl/cad.ts");
        let source = std::fs::read_to_string(&dsl).unwrap();
        std::fs::write(&dsl, format!("{source}
// touched
")).unwrap();
        assert_eq!(max_extent(&next_result(&rx).unwrap()), 10.0);
    }

    #[test]
    fn opening_a_model_outside_the_folder_opens_its_project() {
        let project = Project::new(&cube_source(1));
        let elsewhere = tempfile::tempdir().unwrap();
        let (rx, running) = start_session(Start::Folder(elsewhere.path().to_path_buf()));
        next_listing(&rx);

        running.send(ViewerCommand::OpenModel(project.model()));
        assert_eq!(next_listing(&rx).path, project.root());
        assert_eq!(max_extent(&next_result(&rx).unwrap()), 1.0);
    }

    #[test]
    fn exports_the_view_on_screen_to_outputs() {
        let project = Project::new(&print_model(&coaster("a", 40)));
        let (rx, running) = start_session(Start::Folder(project.root().join("models")));
        running.send(ViewerCommand::OpenModel(project.model()));
        next_result(&rx).unwrap();

        running.send(ViewerCommand::Export(ViewMode::Print));
        let notice = next_notice(&rx);
        assert!(notice.ok, "{notice:?}");
        let out = project.root().join("outputs/model_print.3mf");
        assert_eq!(
            notice.message,
            format!(
                "Wrote {} (print view, 1 objects, fine mesh)",
                Path::new("outputs").join("model_print.3mf").display()
            )
        );
        assert_eq!(exported_objects(&out), ["a"]);

        running.send(ViewerCommand::Export(ViewMode::Assembled));
        assert!(next_notice(&rx).ok);
        assert!(project.root().join("outputs/model_assembled.3mf").is_file());
    }

    #[test]
    fn export_explains_what_is_missing() {
        let project = Project::new(&cube_source(0));
        let (rx, running) = start_session(Start::Empty);

        running.send(ViewerCommand::Export(ViewMode::Assembled));
        assert_eq!(next_notice(&rx).message, "Open a model to export it.");

        // cube(0) fails to build: nothing on screen to export.
        running.send(ViewerCommand::OpenModel(project.model()));
        assert!(next_result(&rx).unwrap().views.is_none());
        running.send(ViewerCommand::Export(ViewMode::Assembled));
        let notice = next_notice(&rx);
        assert!(!notice.ok && notice.message.contains("hasn't built yet"), "{notice:?}");

        thread::sleep(Duration::from_millis(300));
        project.write_model(&cube_source(3));
        next_result(&rx).unwrap();
        running.send(ViewerCommand::Export(ViewMode::Print));
        assert_eq!(next_notice(&rx).message, "model.ts has no print view.");
        assert!(!project.root().join("outputs").exists());
    }

    #[test]
    fn export_keeps_the_last_good_build_after_an_error() {
        let project = Project::new(&cube_source(4));
        let (rx, running) = project.watch();
        project.write_model(&cube_source(0));
        assert!(next_result(&rx).unwrap().views.is_none());

        running.send(ViewerCommand::Export(ViewMode::Assembled));
        assert!(next_notice(&rx).ok);
        assert!(project.root().join("outputs/model_assembled.3mf").is_file());
    }

    // --- Mesh quality and settings -----------------------------------------

    const DISC_SOURCE: &str =
        "import { circle, extrude } from \"../dsl/cad.ts\";\nexport default () => extrude(circle(40), { distance: 5 });\n";

    fn triangles(update: &SceneUpdate) -> usize {
        update.views.as_ref().expect("views").assembled.indices.len() / 3
    }

    #[test]
    fn pipeline_meshes_at_its_quality_and_rebuilds_the_last_good_specs() {
        let project = Project::new(DISC_SOURCE);
        let mut pipeline = project.pipeline();
        assert_eq!(pipeline.quality(), MeshQuality::Fine);
        assert!(pipeline.rebuild(ViewMode::Assembled, MeshQuality::Fine).is_none(), "nothing built yet");

        pipeline.set_quality(MeshQuality::Draft);
        let draft = pipeline.evaluate(&project.model());
        assert!(draft.diagnostics[0].message.contains("draft mesh"), "{:?}", draft.diagnostics);

        // The script is broken now; the last good spec still rebuilds.
        project.write_model(&cube_source(0));
        assert!(pipeline.evaluate(&project.model()).views.is_none());
        let fine = pipeline.rebuild(ViewMode::Assembled, MeshQuality::Fine).unwrap().unwrap();
        assert!(fine.indices.len() / 3 > triangles(&draft), "{} vs {}", fine.indices.len() / 3, triangles(&draft));
        assert!(pipeline.rebuild(ViewMode::Print, MeshQuality::Fine).is_none(), "no print view");
    }

    #[test]
    fn export_cli_takes_a_quality() {
        assert_eq!(parse_export(&["export", "m.ts"]).quality, QualityArg::Fine);
        let args = parse_export(&["export", "m.ts", "--quality", "draft"]);
        assert_eq!(MeshQuality::from(args.quality), MeshQuality::Draft);
    }

    #[test]
    fn store_round_trips_and_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/settings.toml");

        let (mut store, warning) = Store::load(path.clone());
        assert_eq!((store.state.clone(), warning), (AppState::default(), None), "missing file");
        store.state.last_folder = Some(dir.path().join("models"));
        store.state.settings.view_quality = MeshQuality::Draft;
        store.state.settings.antialiasing = false;
        store.save().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("view_quality = \"draft\""), "{text}");
        assert_eq!(Store::load(path.clone()).0.state, store.state);

        // Older files without some keys load, with defaults for the rest.
        std::fs::write(&path, "[settings]\nshow_edges = false\n").unwrap();
        let (store, warning) = Store::load(path.clone());
        assert_eq!(warning, None);
        assert!(!store.state.settings.show_edges && store.state.last_folder.is_none());
        assert_eq!(store.state.settings.view_quality, Settings::default().view_quality);

        std::fs::write(&path, "[settings]\nview_quality = \"ultra\"\n").unwrap();
        let (store, warning) = Store::load(path.clone());
        assert_eq!(store.state, AppState::default());
        assert!(warning.unwrap().contains("settings.toml"));
    }

    #[test]
    fn opening_a_folder_remembers_it() {
        let project = Project::new(&cube_source(1));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let (rx, running) = start_session_with(Start::Empty, Store::load(path.clone()).0);
        running.send(ViewerCommand::OpenFolder(project.root().join("models")));
        next_listing(&rx);
        // Sent after the save, so the file is written by now.
        running.send(ViewerCommand::Export(ViewMode::Assembled));
        next_notice(&rx);
        assert_eq!(Store::load(path).0.state.last_folder, Some(project.root().join("models")));
    }

    #[test]
    fn changing_the_view_quality_rebuilds_and_saves_it() {
        let project = Project::new(DISC_SOURCE);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.toml");
        let (mut store, _) = Store::load(path.clone());
        store.state.settings.view_quality = MeshQuality::Draft;
        let draft_settings = store.state.settings;
        let (rx, running) = start_session_with(Start::Model(project.model()), store);
        let draft = next_result(&rx).unwrap();

        // Settings that don't affect meshes don't rebuild.
        running.send(ViewerCommand::Settings(Settings {
            show_edges: false,
            ..draft_settings
        }));
        assert!(rx.recv_timeout(QUIET).is_err(), "rebuilt for show_edges");

        let fine_settings = Settings {
            view_quality: MeshQuality::Fine,
            show_edges: false,
            ..draft_settings
        };
        running.send(ViewerCommand::Settings(fine_settings));
        let fine = next_result(&rx).unwrap();
        assert!(fine.diagnostics[0].message.contains("fine mesh"), "{:?}", fine.diagnostics);
        assert!(triangles(&fine) > triangles(&draft));
        assert_eq!(Store::load(path).0.state.settings, fine_settings);
    }
}
