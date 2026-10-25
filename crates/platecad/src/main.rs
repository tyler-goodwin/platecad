use std::process::ExitCode;
use std::sync::mpsc;

use cad_ir::Diagnostic;
use platecad::{
    Cli, Command, ExportArgs, InitArgs, OpenArgs, PROJECT_MARKER, Pipeline, SETTINGS_FILE, Start, Store,
    export_3mf, init_project, plate_home, project_root, resolve_model, resolve_start, run_session,
};
use cad_view::{SceneUpdate, ViewerMessage, ViewportApp};
use cad_view_threed::ThreeDViewport;
use clap::Parser;

fn main() -> ExitCode {
    let cwd = std::env::current_dir().expect("current dir");
    match Cli::parse().into_command() {
        Command::Open(args) => open(args, cwd),
        Command::Export(args) => export(args, cwd),
        Command::Init(args) => init(args, cwd),
    }
}

fn open(args: OpenArgs, cwd: std::path::PathBuf) -> ExitCode {
    let (store, store_warning) = match plate_home() {
        Some(home) => Store::load(home.join(SETTINGS_FILE)),
        None => (Store::default(), None),
    };
    let start = match &args.path {
        Some(path) => match resolve_start(path, &cwd) {
            Ok(start) => start,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        },
        None if cwd.join(PROJECT_MARKER).is_file() => Start::Folder(cwd),
        None => match &store.state.last_folder {
            Some(folder) if folder.is_dir() => Start::Folder(folder.clone()),
            _ => Start::Empty,
        },
    };

    // Open the window right away; the model loads on the session thread.
    let mut initial = vec![ViewerMessage::Settings(store.state.settings)];
    if let Start::Model(model) = &start {
        let name = model.file_name().unwrap_or_default().to_string_lossy();
        initial.push(ViewerMessage::Scene(SceneUpdate::loading(format!("Loading {name}…"))));
    }
    if let Some(warning) = store_warning {
        eprintln!("warning: {warning}");
        initial.push(ViewerMessage::Notice(Diagnostic::warning(warning)));
    }

    let (tx, rx) = mpsc::channel();
    let (commands_tx, commands_rx) = mpsc::channel();
    let kernel = args.kernel;
    std::thread::spawn(move || {
        if let Err(e) = run_session(start, kernel, store, commands_rx, tx.clone()) {
            let _ = tx.send(SceneUpdate::error(format!("file watcher stopped: {e}")).into());
        }
    });

    if let Err(e) = ThreeDViewport::default().run(initial, rx, commands_tx) {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn export(args: ExportArgs, cwd: std::path::PathBuf) -> ExitCode {
    let model = match resolve_model(&args.model, &cwd) {
        Ok(model) => model,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let mut pipeline = match Pipeline::new(&project_root(&model), args.kernel) {
        Ok(pipeline) => pipeline,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    pipeline.set_quality(args.quality.into());
    let out = args.output.map(|out| cwd.join(out));
    match export_3mf(&mut pipeline, &model, args.view, out.as_deref()) {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn init(args: InitArgs, cwd: std::path::PathBuf) -> ExitCode {
    let dir = cwd.join(&args.dir);
    match init_project(&dir) {
        Ok(written) => {
            for path in written {
                println!("created {}", path.strip_prefix(&cwd).unwrap_or(&path).display());
            }
            println!("\nOpen it with: plate {}", args.dir.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
