use crossterm::event::KeyEvent;
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table, TableState},
};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};
use sysinfo::System;
use tokio::sync::mpsc;

use crate::args::Args;
use crate::worker::{WorkerCmd, WorkerState, WorkerStatus, run_lldb_crash, run_worker};

const NUM_COLS: usize = 4;

#[derive(Clone, Debug)]
pub struct TestStats {
    pub attempts: u64,
    pub successes: u64,
    pub failures: u64,
    pub hangs: u64,
    pub total_duration: Duration,
}

pub enum AppEvent {
    Key(KeyEvent),
    Tick,
    WorkerStarted {
        worker_id: usize,
        test_path: String,
        pid: u32,
    },
    WorkerFinished {
        worker_id: usize,
        test_path: String,
        status: Result<i32, String>,
        duration: Duration,
        stdout: String,
        stderr: String,
    },
    WorkerSuspectedHang {
        worker_id: usize,
    },
    WorkerConfirmedHang {
        worker_id: usize,
        backtrace: String,
        core_path: Option<String>,
    },
    WorkerAborted {
        worker_id: usize,
    },
    LogMessage(String),
}

pub enum RunState {
    Running,
    Isolating { suspect_worker_id: usize },
    Terminated,
}

pub struct App {
    pub args: Args,
    pub test_binaries: Vec<String>,
    pub next_test_idx: usize,

    pub total_attempts: u64,
    pub total_failures: u64,
    pub total_hangs: u64,
    pub start_time: Instant,

    pub workers: Vec<WorkerState>,
    pub worker_txs: Vec<mpsc::Sender<WorkerCmd>>,
    pub test_stats: BTreeMap<String, TestStats>,

    pub run_state: RunState,

    pub event_logs: Vec<String>,
    pub test_table_state: TableState,
    pub log_list_state: ListState,

    pub sys: System,
    pub cpu_usage: f32,
    pub mem_usage: f64,
}

impl App {
    pub fn new(args: Args, test_binaries: Vec<String>, tx_event: mpsc::Sender<AppEvent>) -> Self {
        let hang_timeout = Duration::from_secs_f64(args.hang_timeout);
        let mut worker_txs = Vec::new();
        let mut workers = Vec::new();

        let lldb_path = args.lldb_path.clone();
        let workers_count = args.workers.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
        });
        for i in 0..workers_count {
            let (tx, rx) = mpsc::channel(10);
            worker_txs.push(tx);
            workers.push(WorkerState {
                id: i,
                status: WorkerStatus::Idle,
            });
            tokio::spawn(run_worker(
                i,
                rx,
                tx_event.clone(),
                hang_timeout,
                lldb_path.clone(),
                args.report_dir.clone(),
            ));
        }

        let mut test_stats = BTreeMap::new();
        for path in &test_binaries {
            test_stats.insert(
                path.clone(),
                TestStats {
                    attempts: 0,
                    successes: 0,
                    failures: 0,
                    hangs: 0,
                    total_duration: Duration::ZERO,
                },
            );
        }

        let mut test_table_state = TableState::default();
        if !test_binaries.is_empty() {
            test_table_state.select(Some(0));
        }

        let log_list_state = ListState::default();

        Self {
            args,
            test_binaries,
            next_test_idx: 0,
            total_attempts: 0,
            total_failures: 0,
            total_hangs: 0,
            start_time: Instant::now(),
            workers,
            worker_txs,
            test_stats,
            run_state: RunState::Running,
            event_logs: Vec::new(),
            test_table_state,
            log_list_state,
            sys: System::new_all(),
            cpu_usage: 0.0,
            mem_usage: 0.0,
        }
    }

    pub fn log(&mut self, msg: String) {
        let timestamp = chrono::Local::now().format("%H:%M:%S");
        self.event_logs.push(format!("[{}] {}", timestamp, msg));
        if self.event_logs.len() > 1000 {
            self.event_logs.remove(0);
        }
        self.log_list_state
            .select(Some(self.event_logs.len().saturating_sub(1)));
    }

    pub fn get_next_job(&mut self) -> String {
        let job = self.test_binaries[self.next_test_idx].clone();
        self.next_test_idx = (self.next_test_idx + 1) % self.test_binaries.len();
        job
    }

    pub fn schedule_jobs(&mut self) {
        if !matches!(self.run_state, RunState::Running) {
            return;
        }

        if let Some(limit) = self.args.duration
            && self.start_time.elapsed().as_secs_f64() >= limit
        {
            return;
        }

        for i in 0..self.workers.len() {
            if self.workers[i].status == WorkerStatus::Idle {
                let test_path = self.get_next_job();
                let tx = &self.worker_txs[i];
                let tx_clone = tx.clone();
                let test_path_clone = test_path.clone();
                tokio::spawn(async move {
                    let _ = tx_clone
                        .send(WorkerCmd::StartJob {
                            test_path: test_path_clone,
                        })
                        .await;
                });
                self.workers[i].status = WorkerStatus::Running {
                    test_path,
                    pid: 0,
                    start_time: Instant::now(),
                };
            }
        }
    }

    pub fn handle_worker_started(&mut self, id: usize, test_path: String, pid: u32) {
        if let WorkerStatus::Running { start_time, .. } = &self.workers[id].status {
            self.workers[id].status = WorkerStatus::Running {
                test_path,
                pid,
                start_time: *start_time,
            };
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_worker_finished(
        &mut self,
        id: usize,
        test_path: String,
        status: Result<i32, String>,
        duration: Duration,
        _stdout: String,
        _stderr: String,
        tx_event: mpsc::Sender<AppEvent>,
    ) {
        if let Some(stats) = self.test_stats.get_mut(&test_path) {
            stats.attempts += 1;
            self.total_attempts += 1;
            stats.total_duration += duration;

            let test_filename = Path::new(&test_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&test_path);

            match &status {
                Ok(0) => {
                    stats.successes += 1;
                }
                Ok(code) => {
                    stats.failures += 1;
                    self.total_failures += 1;
                    self.log(format!(
                        "Worker {} - test '{}' failed (exit code {}). Rerunning with LLDB...",
                        id, test_filename, code
                    ));
                    self.spawn_lldb_crash(test_path.clone(), tx_event.clone());
                }
                Err(err) => {
                    stats.failures += 1;
                    self.total_failures += 1;
                    self.log(format!(
                        "Worker {} - test '{}' crashed: {}. Rerunning with LLDB...",
                        id, test_filename, err
                    ));
                    self.spawn_lldb_crash(test_path.clone(), tx_event.clone());
                }
            }
        }

        self.workers[id].status = WorkerStatus::Idle;

        if let RunState::Isolating {
            suspect_worker_id, ..
        } = self.run_state
            && suspect_worker_id == id
        {
            let test_filename = Path::new(&test_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&test_path);
            self.log(format!(
                "Worker {} - suspected hang on '{}' was a false positive. Resuming...",
                id, test_filename
            ));
            self.run_state = RunState::Running;
        }
    }

    pub fn handle_suspected_hang(&mut self, worker_id: usize) {
        if !matches!(self.run_state, RunState::Running) {
            return;
        }

        let test_path = match &self.workers[worker_id].status {
            WorkerStatus::Running { test_path, .. } => test_path.clone(),
            _ => return,
        };
        let test_filename = Path::new(&test_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&test_path);

        self.log(format!(
            "[SUSPECT HANG] Worker {} running '{}' has timed out. Isolating...",
            worker_id, test_filename
        ));

        self.run_state = RunState::Isolating {
            suspect_worker_id: worker_id,
        };

        let tx_suspect = &self.worker_txs[worker_id];
        let tx_suspect_clone = tx_suspect.clone();
        tokio::spawn(async move {
            let _ = tx_suspect_clone.send(WorkerCmd::ProceedIsolation).await;
        });

        self.workers[worker_id].status = match &self.workers[worker_id].status {
            WorkerStatus::Running {
                test_path,
                pid,
                start_time,
            } => WorkerStatus::Isolating {
                test_path: test_path.clone(),
                pid: *pid,
                start_time: *start_time,
            },
            _ => self.workers[worker_id].status.clone(),
        };

        for i in 0..self.workers.len() {
            if i != worker_id
                && let WorkerStatus::Running { .. } = self.workers[i].status
            {
                let tx = &self.worker_txs[i];
                let tx_clone = tx.clone();
                tokio::spawn(async move {
                    let _ = tx_clone.send(WorkerCmd::Kill).await;
                });
            }
        }
    }

    pub fn handle_worker_confirmed_hang(
        &mut self,
        id: usize,
        backtrace: String,
        core_path: Option<String>,
    ) {
        self.total_hangs += 1;
        let test_path = match &self.workers[id].status {
            WorkerStatus::Isolating { test_path, .. } => test_path.clone(),
            _ => "unknown".to_string(),
        };

        let test_filename = Path::new(&test_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&test_path);

        if let Some(stats) = self.test_stats.get_mut(&test_path) {
            stats.hangs += 1;
        }

        self.log(format!(
            "Worker {} - CONFIRMED HANG on '{}'!",
            id, test_filename
        ));

        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let report_path = format!(
            "{}/hang_{}_{}.log",
            self.args.report_dir, test_filename, timestamp
        );

        let _ = std::fs::create_dir_all(&self.args.report_dir);
        let _ = std::fs::write(&report_path, &backtrace);

        self.run_state = RunState::Terminated;

        ratatui::restore();
        println!("\n========================================================");
        println!("[CONFIRMED HANG] Test '{}' hung!", test_filename);
        println!("Executable path: {}", test_path);
        println!("Report written to: {}", report_path);
        if let Some(core) = &core_path {
            println!("Core dump written to: {}", core);
        }
        println!("========================================================\n");
        println!("LLDB Thread Backtrace:\n{}", backtrace);

        std::process::exit(1);
    }

    pub fn handle_worker_aborted(&mut self, id: usize) {
        self.workers[id].status = WorkerStatus::Idle;
    }

    pub fn spawn_lldb_crash(&self, test_path: String, tx_event: mpsc::Sender<AppEvent>) {
        let lldb_path = self.args.lldb_path.clone();
        let report_dir = self.args.report_dir.clone();
        tokio::spawn(async move {
            let filename = Path::new(&test_path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
            let core_path = format!("{}/core_crash_{}_{}.core", report_dir, filename, timestamp);
            if let Some(bt) = run_lldb_crash(&lldb_path, &test_path, Some(&core_path)).await {
                let report_path = format!("{}/crash_{}_{}.log", report_dir, filename, timestamp);
                let _ = tokio::fs::create_dir_all(&report_dir).await;
                if tokio::fs::write(&report_path, bt).await.is_ok() {
                    let _ = tx_event
                        .send(AppEvent::LogMessage(format!(
                            "Crash BT for {} saved to {} (Core: {})",
                            filename, report_path, core_path
                        )))
                        .await;
                }
            }
        });
    }

    pub fn refresh_sysinfo(&mut self) {
        self.sys.refresh_cpu();
        self.sys.refresh_memory();

        self.cpu_usage = self.sys.global_cpu_info().cpu_usage();
        let total_mem = self.sys.total_memory();
        if total_mem > 0 {
            self.mem_usage = (self.sys.used_memory() as f64 / total_mem as f64) * 100.0;
        }
    }

    pub fn scroll_table(&mut self, delta: isize) {
        let n = self.test_stats.len();
        if n == 0 {
            return;
        }
        let current = self.test_table_state.selected().unwrap_or(0);
        let next = (current as isize + delta).rem_euclid(n as isize) as usize;
        self.test_table_state.select(Some(next));
    }

    pub fn scroll_logs(&mut self, delta: isize) {
        let n = self.event_logs.len();
        if n == 0 {
            return;
        }
        let current = self.log_list_state.selected().unwrap_or(n - 1);
        let next = (current as isize + delta).clamp(0, n as isize - 1) as usize;
        self.log_list_state.select(Some(next));
    }

    pub fn draw(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> std::io::Result<()> {
        terminal.draw(|f| {
            let rect = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Min(10),
                    Constraint::Length(10),
                ])
                .split(rect);

            let elapsed_dur = self.start_time.elapsed();
            let limit_str = match self.args.duration {
                Some(limit) => format!(
                    "{:02}:{:02}:{:02}",
                    limit as u64 / 3600,
                    (limit as u64 % 3600) / 60,
                    limit as u64 % 60
                ),
                None => "Indefinite".to_string(),
            };
            let time_str = format!(
                "Elapsed: {:02}:{:02}:{:02} / {}",
                elapsed_dur.as_secs() / 3600,
                (elapsed_dur.as_secs() % 3600) / 60,
                elapsed_dur.as_secs() % 60,
                limit_str
            );

            let status_text = match &self.run_state {
                RunState::Running => Span::styled(
                    "NORMAL",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                RunState::Isolating {
                    suspect_worker_id, ..
                } => Span::styled(
                    format!("ISOLATING WORKER {}", suspect_worker_id),
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK),
                ),
                RunState::Terminated => Span::styled(
                    "TERMINATED",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ),
            };

            let header_spans = vec![
                Span::raw("Status: "),
                status_text,
                Span::raw(" | "),
                Span::raw(time_str),
                Span::raw(" | "),
                Span::raw(format!("Total Runs: {} | ", self.total_attempts)),
                Span::styled(
                    format!("Failures: {} | ", self.total_failures),
                    Style::default().fg(Color::LightRed),
                ),
                Span::styled(
                    format!("Hangs: {} | ", self.total_hangs),
                    Style::default().fg(Color::Red),
                ),
                Span::raw(format!("CPU: {:.1}% | ", self.cpu_usage)),
                Span::raw(format!("MEM: {:.1}%", self.mem_usage)),
            ];

            let header = Paragraph::new(Line::from(header_spans)).block(
                Block::default()
                    .title(" INTEGRATION AGGRESSOR ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Blue)),
            );
            f.render_widget(header, chunks[0]);

            let body_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
                .split(chunks[1]);

            let workers_per_col = self.workers.len().div_ceil(NUM_COLS);
            let mut rows = Vec::new();
            for i in 0..workers_per_col {
                let mut row_cells = Vec::new();
                for col in 0..NUM_COLS {
                    let worker_idx = col * workers_per_col + i;
                    if worker_idx < self.workers.len() {
                        let w = &self.workers[worker_idx];
                        let cell_text = match &w.status {
                            WorkerStatus::Idle => format!("{:2}: Idle", w.id),
                            WorkerStatus::Running { test_path, .. } => {
                                let name = Path::new(test_path)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("..");
                                let trunc = if name.len() > 10 { &name[..10] } else { name };
                                format!("{:2}: RUN {}", w.id, trunc)
                            }
                            WorkerStatus::Isolating { test_path, .. } => {
                                let name = Path::new(test_path)
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("..");
                                format!("{:2}: ISO {}", w.id, name)
                            }
                        };

                        let cell_style = match &w.status {
                            WorkerStatus::Idle => Style::default().fg(Color::DarkGray),
                            WorkerStatus::Running { .. } => Style::default().fg(Color::Cyan),
                            WorkerStatus::Isolating { .. } => Style::default()
                                .fg(Color::Red)
                                .add_modifier(Modifier::SLOW_BLINK),
                        };

                        row_cells.push(Cell::from(cell_text).style(cell_style));
                    } else {
                        row_cells.push(Cell::from(""));
                    }
                }
                rows.push(Row::new(row_cells));
            }

            let w_widths = [
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
            ];

            let workers_table = Table::new(rows, w_widths)
                .block(Block::default().title(" Workers ").borders(Borders::ALL));
            f.render_widget(workers_table, body_chunks[0]);

            let mut table_rows = Vec::new();
            for (path, stats) in &self.test_stats {
                let name = Path::new(path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(path);
                let avg_time = if stats.attempts > 0 {
                    format!("{:.2?}", stats.total_duration / stats.attempts as u32)
                } else {
                    "-".to_string()
                };

                let fail_cell = if stats.failures > 0 {
                    Cell::from(stats.failures.to_string())
                        .style(Style::default().fg(Color::LightRed))
                } else {
                    Cell::from("0")
                };

                let hang_cell = if stats.hangs > 0 {
                    Cell::from(stats.hangs.to_string()).style(Style::default().fg(Color::Red))
                } else {
                    Cell::from("0")
                };

                table_rows.push(Row::new(vec![
                    Cell::from(name.to_string()),
                    Cell::from(stats.attempts.to_string()),
                    Cell::from(stats.successes.to_string()),
                    fail_cell,
                    hang_cell,
                    Cell::from(avg_time),
                ]));
            }

            let t_widths = [
                Constraint::Percentage(45),
                Constraint::Percentage(11),
                Constraint::Percentage(11),
                Constraint::Percentage(11),
                Constraint::Percentage(11),
                Constraint::Percentage(11),
            ];

            let table = Table::new(table_rows, t_widths)
                .header(
                    Row::new(vec!["Test", "Runs", "OK", "Fail", "Hang", "Avg Time"]).style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                )
                .block(
                    Block::default()
                        .title(" Test Statistics ")
                        .borders(Borders::ALL),
                )
                .row_highlight_style(
                    Style::default()
                        .bg(Color::Rgb(50, 50, 50))
                        .add_modifier(Modifier::BOLD),
                );

            f.render_stateful_widget(table, body_chunks[1], &mut self.test_table_state);

            let logs: Vec<ListItem> = self
                .event_logs
                .iter()
                .map(|log| ListItem::new(log.as_str()))
                .collect();

            let log_list = List::new(logs)
                .block(Block::default().title(" System Log ").borders(Borders::ALL))
                .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

            f.render_stateful_widget(log_list, chunks[2], &mut self.log_list_state);
        })?;
        Ok(())
    }
}
