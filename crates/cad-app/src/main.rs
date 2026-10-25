use std::process::ExitCode;
use std::sync::mpsc;

use cad_app::{
    Cli, Command, ExportArgs, InitArgs, OpenArgs, PROJECT_MARKER, Pipeline, Start, export_3mf, init_project,
    project_root, resolve_model, resolve_start, run_session,
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
    let start = match &args.path {
        Some(path) => match resolve_start(path, &cwd) {
            Ok(start) => start,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        },
        None if cwd.join(PROJECT_MARKER).is_file() => Start::Folder(cwd),
        None => Start::Empty,
    };

    // Open the window right away; the model loads on the session thread.
    let initial = match &start {
        Start::Model(model) => {
            let name = model.file_name().unwrap_or_default().to_string_lossy();
            vec![ViewerMessage::Scene(SceneUpdate::loading(format!("Loading {name}…")))]
        }
        Start::Folder(_) | Start::Empty => Vec::new(),
    };

    let (tx, rx) = mpsc::channel();
    let (commands_tx, commands_rx) = mpsc::channel();
    let kernel = args.kernel;
    std::thread::spawn(move || {
        if let Err(e) = run_session(start, kernel, commands_rx, tx.clone()) {
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
            println!("\nOpen it with: caddy {}", args.dir.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
