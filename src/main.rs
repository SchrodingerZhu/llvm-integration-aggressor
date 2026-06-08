mod app;
mod args;
mod worker;

use crossterm::event::{Event, KeyCode, KeyEventKind};
use palc::Parser;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use walkdir::WalkDir;

use app::{App, AppEvent};
use args::Args;
use worker::WorkerStatus;

fn find_integration_tests(test_dir: Option<String>) -> Vec<String> {
    let base_dir = if let Some(dir) = test_dir {
        PathBuf::from(dir)
    } else {
        // Auto-detect build path
        let paths = vec![
            PathBuf::from("build/libc/test/integration"),
            PathBuf::from("llvm-project/build/libc/test/integration"),
        ];
        paths
            .into_iter()
            .find(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from("build/libc/test/integration"))
    };

    let mut binaries = Vec::new();
    if base_dir.exists() {
        for entry in WalkDir::new(base_dir).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let file_name = entry.file_name().to_string_lossy();
                if file_name.ends_with(".__build__") {
                    let path = entry.path();
                    let path_str = path.to_string_lossy();
                    let is_thread_test = path_str.contains("/pthread/")
                        || path_str.contains("/threads/")
                        || path_str.contains("__support/threads");
                    if is_thread_test {
                        // Check if executable
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            if let Ok(metadata) = std::fs::metadata(path)
                                && metadata.permissions().mode() & 0o111 != 0
                            {
                                binaries.push(path_str.into_owned());
                            }
                        }
                        #[cfg(not(unix))]
                        binaries.push(path_str.into_owned());
                    }
                }
            }
        }
    }
    binaries.sort();
    binaries
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let test_binaries = find_integration_tests(args.test_dir.clone());
    if test_binaries.is_empty() {
        eprintln!("Error: No integration test binaries ending with '.__build__' found.");
        std::process::exit(1);
    }

    let (tx_event, mut rx_event) = mpsc::channel(100);

    // Initialize TUI terminal
    let mut terminal = ratatui::init();

    let mut app = App::new(args, test_binaries, tx_event.clone());
    app.log(format!(
        "Discovered {} integration test binaries.",
        app.test_binaries.len()
    ));
    app.log(format!("Spawning {} workers...", app.workers.len()));

    // Input listening thread
    let tx_event_key = tx_event.clone();
    tokio::task::spawn_blocking(move || {
        loop {
            if crossterm::event::poll(Duration::from_millis(50)).unwrap()
                && let Ok(Event::Key(key)) = crossterm::event::read()
                && key.kind == KeyEventKind::Press
                && tx_event_key.blocking_send(AppEvent::Key(key)).is_err()
            {
                break;
            }
        }
    });

    // Tick sender loop
    let tx_event_tick = tx_event.clone();
    let tick_rate = app.args.tick_rate;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(tick_rate)).await;
            if tx_event_tick.send(AppEvent::Tick).await.is_err() {
                break;
            }
        }
    });

    app.schedule_jobs();

    loop {
        if let Some(event) = rx_event.recv().await {
            match event {
                AppEvent::Key(key) => match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Up => app.scroll_table(-1),
                    KeyCode::Down => app.scroll_table(1),
                    KeyCode::PageUp => app.scroll_logs(-5),
                    KeyCode::PageDown => app.scroll_logs(5),
                    _ => {}
                },
                AppEvent::Tick => {
                    app.refresh_sysinfo();
                    app.schedule_jobs();

                    // Check for overall runtime timeout
                    if let Some(limit) = app.args.duration
                        && app.start_time.elapsed().as_secs_f64() >= limit
                    {
                        // Wait for all workers to finish current job, then exit
                        let all_idle = app.workers.iter().all(|w| w.status == WorkerStatus::Idle);
                        if all_idle {
                            app.log("Stress test duration reached. All workers idle. Exiting successfully.".to_string());
                            break;
                        }
                    }
                }
                AppEvent::WorkerStarted {
                    worker_id,
                    test_path,
                    pid,
                } => {
                    app.handle_worker_started(worker_id, test_path, pid);
                }
                AppEvent::WorkerFinished {
                    worker_id,
                    test_path,
                    status,
                    duration,
                    stdout,
                    stderr,
                } => {
                    app.handle_worker_finished(
                        worker_id,
                        test_path,
                        status,
                        duration,
                        stdout,
                        stderr,
                        tx_event.clone(),
                    );
                    app.schedule_jobs();
                }
                AppEvent::WorkerSuspectedHang { worker_id } => {
                    app.handle_suspected_hang(worker_id);
                }
                AppEvent::WorkerConfirmedHang {
                    worker_id,
                    backtrace,
                    core_path,
                } => {
                    app.handle_worker_confirmed_hang(worker_id, backtrace, core_path);
                    break;
                }
                AppEvent::WorkerAborted { worker_id } => {
                    app.handle_worker_aborted(worker_id);
                    app.schedule_jobs();
                }
                AppEvent::LogMessage(msg) => {
                    app.log(msg);
                }
            }
        }
        app.draw(&mut terminal)?;
    }

    ratatui::restore();
    Ok(())
}
