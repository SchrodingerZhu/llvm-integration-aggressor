use crate::app::AppEvent;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, mpsc};

#[derive(Clone, Debug, PartialEq)]
pub enum WorkerStatus {
    Idle,
    Running {
        test_path: String,
        pid: u32,
        start_time: Instant,
    },
    Isolating {
        test_path: String,
        pid: u32,
        start_time: Instant,
    },
}

pub struct WorkerState {
    pub id: usize,
    pub status: WorkerStatus,
}

#[derive(Clone, Debug)]
pub enum WorkerCmd {
    StartJob { test_path: String },
    ProceedIsolation,
    Kill,
}

pub struct SubprocessHelper {
    pub child: tokio::process::Child,
    pub stdout_buf: Arc<Mutex<Vec<u8>>>,
    pub stderr_buf: Arc<Mutex<Vec<u8>>>,
}

impl SubprocessHelper {
    pub fn spawn(test_path: &str) -> std::io::Result<Self> {
        let mut child = tokio::process::Command::new(test_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;

        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();

        let stdout_buf = Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(Vec::new()));

        let s_buf = stdout_buf.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0; 1024];
            while let Ok(n) = stdout.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                s_buf.lock().await.extend_from_slice(&buf[..n]);
            }
        });

        let e_buf = stderr_buf.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0; 1024];
            while let Ok(n) = stderr.read(&mut buf).await {
                if n == 0 {
                    break;
                }
                e_buf.lock().await.extend_from_slice(&buf[..n]);
            }
        });

        Ok(Self {
            child,
            stdout_buf,
            stderr_buf,
        })
    }

    pub async fn wait_status(&mut self) -> std::io::Result<std::process::ExitStatus> {
        self.child.wait().await
    }

    pub async fn get_output(&self) -> (String, String) {
        let out = String::from_utf8_lossy(&self.stdout_buf.lock().await).into_owned();
        let err = String::from_utf8_lossy(&self.stderr_buf.lock().await).into_owned();
        (out, err)
    }

    pub async fn kill(&mut self) -> std::io::Result<()> {
        self.child.kill().await
    }
}

pub async fn run_worker(
    worker_id: usize,
    mut rx_cmd: mpsc::Receiver<WorkerCmd>,
    tx_event: mpsc::Sender<AppEvent>,
    hang_timeout: Duration,
    lldb_path: String,
    report_dir: String,
) {
    loop {
        match rx_cmd.recv().await {
            Some(WorkerCmd::StartJob { test_path }) => {
                let start_instant = Instant::now();
                let mut helper = match SubprocessHelper::spawn(&test_path) {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = tx_event
                            .send(AppEvent::WorkerFinished {
                                worker_id,
                                test_path,
                                status: Err(e.to_string()),
                                duration: Duration::ZERO,
                                stdout: String::new(),
                                stderr: String::new(),
                            })
                            .await;
                        continue;
                    }
                };

                let pid = helper.child.id().unwrap_or(0);
                let _ = tx_event
                    .send(AppEvent::WorkerStarted {
                        worker_id,
                        test_path: test_path.clone(),
                        pid,
                    })
                    .await;

                let mut is_suspected = false;
                tokio::select! {
                    res = helper.wait_status() => {
                        let duration = start_instant.elapsed();
                        let (out, err) = helper.get_output().await;
                        let status_str = match res {
                            Ok(code) => {
                                if code.success() {
                                    Ok(0)
                                } else {
                                    Ok(code.code().unwrap_or(-1))
                                }
                            }
                            Err(e) => Err(e.to_string()),
                        };
                        let _ = tx_event.send(AppEvent::WorkerFinished {
                            worker_id,
                            test_path: test_path.clone(),
                            status: status_str,
                            duration,
                            stdout: out,
                            stderr: err,
                        }).await;
                    }
                    _ = tokio::time::sleep(hang_timeout) => {
                        is_suspected = true;
                    }
                    Some(cmd) = rx_cmd.recv() => {
                        if let WorkerCmd::Kill = cmd {
                            let _ = helper.kill().await;
                            let _ = tx_event.send(AppEvent::WorkerAborted { worker_id }).await;
                        }
                    }
                }

                if is_suspected {
                    let _ = tx_event
                        .send(AppEvent::WorkerSuspectedHang { worker_id })
                        .await;

                    match rx_cmd.recv().await {
                        Some(WorkerCmd::ProceedIsolation) => {
                            tokio::select! {
                                res = helper.wait_status() => {
                                    let duration = start_instant.elapsed();
                                    let (out, err) = helper.get_output().await;
                                    let status_str = match res {
                                        Ok(code) => {
                                            if code.success() {
                                                Ok(0)
                                            } else {
                                                Ok(code.code().unwrap_or(-1))
                                            }
                                        }
                                        Err(e) => Err(e.to_string()),
                                    };
                                    let _ = tx_event.send(AppEvent::WorkerFinished {
                                        worker_id,
                                        test_path,
                                        status: status_str,
                                        duration,
                                        stdout: out,
                                        stderr: err,
                                    }).await;
                                }
                                _ = tokio::time::sleep(hang_timeout) => {
                                    let test_filename = Path::new(&test_path)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(&test_path);
                                    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
                                    let core_path = format!("{}/core_{}_{}.core", report_dir, test_filename, timestamp);
                                    let backtrace = run_lldb_bt(&lldb_path, pid, Some(&core_path)).await.unwrap_or_else(|| "Failed to capture backtrace".to_string());
                                    let _ = tx_event.send(AppEvent::WorkerConfirmedHang {
                                        worker_id,
                                        backtrace,
                                        core_path: Some(core_path),
                                    }).await;
                                }
                                Some(cmd) = rx_cmd.recv() => {
                                    if let WorkerCmd::Kill = cmd {
                                        let _ = helper.kill().await;
                                        let _ = tx_event.send(AppEvent::WorkerAborted { worker_id }).await;
                                    }
                                }
                            }
                        }
                        Some(WorkerCmd::Kill) | Some(WorkerCmd::StartJob { .. }) | None => {
                            let _ = helper.kill().await;
                            let _ = tx_event.send(AppEvent::WorkerAborted { worker_id }).await;
                        }
                    }
                }
            }
            Some(WorkerCmd::Kill) => {
                let _ = tx_event.send(AppEvent::WorkerAborted { worker_id }).await;
            }
            Some(WorkerCmd::ProceedIsolation) => {}
            None => break,
        }
    }
}

pub async fn run_lldb_bt(lldb_path: &str, pid: u32, core_path: Option<&str>) -> Option<String> {
    let mut cmd = tokio::process::Command::new(lldb_path);
    cmd.arg("-p")
        .arg(pid.to_string())
        .arg("--batch")
        .arg("-o")
        .arg("thread backtrace all");

    if let Some(path) = core_path {
        cmd.arg("-o").arg(format!("process save-core {}", path));
    }

    cmd.arg("-o").arg("quit");

    let output = cmd.output().await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            Some(format!("STDOUT:\n{}\nSTDERR:\n{}", stdout, stderr))
        }
        Err(e) => Some(format!("Failed to run lldb: {}", e)),
    }
}

pub async fn run_lldb_crash(
    lldb_path: &str,
    binary_path: &str,
    core_path: Option<&str>,
) -> Option<String> {
    let mut cmd = tokio::process::Command::new(lldb_path);
    cmd.arg("--batch")
        .arg("-o")
        .arg("run")
        .arg("-o")
        .arg("thread backtrace all");

    if let Some(path) = core_path {
        cmd.arg("-o").arg(format!("process save-core {}", path));
    }

    cmd.arg("-o").arg("quit").arg("--").arg(binary_path);

    let output = cmd.output().await;

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
            Some(format!("STDOUT:\n{}\nSTDERR:\n{}", stdout, stderr))
        }
        Err(e) => Some(format!("Failed to run lldb crash: {}", e)),
    }
}
