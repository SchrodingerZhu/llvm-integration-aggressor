use palc::Parser;

/// High-performance parallel integration test stress-tester with TUI and debugger integration
#[derive(Parser, Debug)]
#[command(version)]
pub struct Args {
    /// Duration to run stress test in seconds
    #[arg(short, long)]
    pub duration: Option<f64>,

    /// Number of parallel workers
    #[arg(short, long)]
    pub workers: Option<usize>,

    /// Timeout in seconds before suspecting a hang
    #[arg(short, long, default_value_t = 60.0)]
    pub hang_timeout: f64,

    /// Directory containing integration tests (defaults to auto-detect)
    #[arg(short, long)]
    pub test_dir: Option<String>,

    /// Directory to save hang and crash reports
    #[arg(short, long, default_value = "reports")]
    pub report_dir: String,

    /// TUI refresh/tick rate in milliseconds
    #[arg(long, default_value_t = 200)]
    pub tick_rate: u64,

    /// Path to LLDB binary
    #[arg(long, default_value = "lldb")]
    pub lldb_path: String,
}
