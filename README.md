# Integration Aggressor

`integration-aggressor` is a high-performance parallel integration test stress-tester with a Terminal User Interface (TUI) and debugger integration. It is designed to run LLVM libc integration tests (specifically thread-related tests) repeatedly in parallel to identify flakiness, race conditions, and hangs.

## Prerequisites

To build and run `integration-aggressor` and the LLVM libc integration tests, you need the following:

### For `integration-aggressor` (Rust)
- **Rust Toolchain**: (Edition 2024 support required, Rust 1.80+ recommended)

### For building LLVM Libc Integration Tests
- **Git**: To clone the LLVM repository.
- **CMake** (version 3.20.0 or higher): The build configuration system.
- **C++ Compiler**: Clang or GCC supporting C++17.
- **Ninja** (Recommended) or Make.
- **Python**: For LLVM test infrastructure.

On Debian/Ubuntu:
```bash
sudo apt update
sudo apt install -y git cmake ninja-build build-essential python3 rustc cargo
```

## Step 1: Build LLVM Libc Integration Tests

`integration-aggressor` runs the integration tests built from the LLVM project. You must build these first.

1. **Clone the LLVM project**:
   ```bash
   git clone https://github.com/llvm/llvm-project.git
   cd llvm-project
   ```

2. **Configure the build**:
   Enable the `libc` project and ensure `LLVM_LIBC_FULL_BUILD` is enabled, as integration tests require a full libc build. It is recommended to use a `Release` build with assertions.

   ```bash
   cmake -S llvm -B build -G Ninja \
     -DLLVM_ENABLE_PROJECTS="libc" \
     -DLLVM_LIBC_FULL_BUILD=ON \
     -DCMAKE_BUILD_TYPE=Release \
     -DLLVM_ENABLE_ASSERTIONS=ON
   ```

3. **Build the integration tests**:
   ```bash
   ninja -C build generate-libc-headers libc-integration-tests
   ```
   This will compile the integration tests. The test binaries (ending in `.__build__`) will be located under `build/libc/test/integration/`.

## Step 2: Build and Run Integration Aggressor

1. **Clone this repository** (if you haven't already) and navigate to it:
   ```bash
   git clone <this-repo-url>
   cd integration-aggressor
   ```

2. **Build the tool**:
   ```bash
   cargo build --release
   ```

3. **Run the tool**:
   By default, `integration-aggressor` will attempt to auto-detect the test binaries if `llvm-project` is in the same parent directory as `integration-aggressor`, or if you run it from the `llvm-project` root.

   Otherwise, you can explicitly point it to the integration tests directory:

   ```bash
   ./target/release/integration-aggressor --test-dir /path/to/llvm-project/build/libc/test/integration
   ```

### Command Line Options

```
Usage: integration-aggressor [OPTIONS]

Options:
  -d, --duration <DURATION>      Duration to run stress test in seconds
  -w, --workers <WORKERS>        Number of parallel workers
  -h, --hang-timeout <TIMEOUT>   Timeout in seconds before suspecting a hang [default: 60.0]
  -t, --test-dir <TEST_DIR>      Directory containing integration tests (defaults to auto-detect)
  -r, --report-dir <REPORT_DIR>  Directory to save hang and crash reports [default: reports]
      --tick-rate <TICK_RATE>    TUI refresh/tick rate in milliseconds [default: 200]
      --lldb-path <LLDB_PATH>    Path to LLDB binary [default: lldb]
  -h, --help                     Print help
  -V, --version                  Print version
```

## License

This project is dual-licensed under the Apache License (Version 2.0) or the MIT license, at your option. See [LICENSE-APACHE](LICENSE-APACHE) and [LICENSE-MIT](LICENSE-MIT) for details.
