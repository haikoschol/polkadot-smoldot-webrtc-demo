#!/usr/bin/env python3
import argparse
import subprocess
import threading
import sys
import os
import time
from pathlib import Path

# --- Configuration & State ---
INIT_LOCK = threading.Lock()  # For thread-safe logging
INIT_FAILED = threading.Event()  # Signal failure across threads

class Colors:
    RESET = "\033[0m"
    CLONE = "\033[34m"  # Blue
    BUILD = "\033[93m"  # Bright Yellow
    PNPM = "\033[36m"  # Cyan
    INFO = "\033[37m"  # White
    ERROR = "\033[91m"  # Red
    SUCCESS = "\033[32m"  # Green
    WARN = "\033[33m"  # Yellow

def print_log(source, message, color):
    """Thread-safe logging with timestamps and colors"""
    timestamp = time.strftime("%H:%M:%S")
    with INIT_LOCK:
        sys.stdout.write(f"{color}[{timestamp}] [{source}] {message}{Colors.RESET}\n")
        sys.stdout.flush()

class TaskResult:
    """Track task completion and errors"""
    def __init__(self, name):
        self.name = name
        self.success = False
        self.error_message = None
        self.skipped = False  # Track if task was skipped (already existed)
        self.completed = threading.Event()

# --- Idempotency Check Functions ---

def check_repo_exists(path):
    """Check if directory exists and contains .git folder"""
    repo_path = Path(path)
    return repo_path.exists() and (repo_path / ".git").exists()

def check_binary_exists(path):
    """Check if binary file exists and is executable"""
    binary_path = Path(path)
    return binary_path.exists() and os.access(binary_path, os.X_OK)

def check_node_modules_exists(path):
    """Check if node_modules directory exists"""
    node_modules = Path(path) / "node_modules"
    return node_modules.exists() and node_modules.is_dir()

# --- Task Execution Functions ---

def task_clone_repo(url, branch, dest, task_result):
    """Clone a repository with shallow clone"""
    try:
        # Check if already cloned
        if check_repo_exists(dest):
            print_log("Clone", f"Repository {dest} already exists, skipping", Colors.SUCCESS)
            task_result.success = True
            task_result.completed.set()
            return

        print_log("Clone", f"Cloning {url} (branch: {branch})...", Colors.CLONE)

        cmd = ["git", "clone", "--branch", branch, "--depth", "1", url, dest]
        process = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1
        )

        # Stream output
        while True:
            if INIT_FAILED.is_set():
                process.terminate()
                return

            line = process.stdout.readline()
            if not line and process.poll() is not None:
                break
            if line:
                clean_line = line.strip()
                print_log("Clone", f"[{Path(dest).name}] {clean_line}", Colors.CLONE)

        rc = process.poll()
        if rc == 0:
            print_log("Clone", f"Successfully cloned {dest}", Colors.SUCCESS)
            task_result.success = True
        else:
            error_msg = f"Failed to clone {url} (exit code {rc})"
            print_log("Clone", error_msg, Colors.ERROR)
            task_result.error_message = error_msg
            INIT_FAILED.set()

    except Exception as e:
        error_msg = f"Exception cloning {url}: {e}"
        print_log("Clone", error_msg, Colors.ERROR)
        task_result.error_message = error_msg
        INIT_FAILED.set()
    finally:
        task_result.completed.set()

def task_cargo_build_bin(repo_dir, package, binary_path, task_result, dependency=None, bin_name=None):
    """Build a Rust binary using cargo"""
    try:
        # Wait for dependency if specified
        if dependency:
            dependency.completed.wait()
            if not dependency.success:
                target_desc = f"{bin_name}" if bin_name else package
                print_log("Build", f"Skipping {target_desc} build due to dependency failure", Colors.WARN)
                task_result.completed.set()
                return

        # Check if already built
        if check_binary_exists(binary_path):
            print_log("Build", f"Binary {binary_path} already exists, skipping", Colors.SUCCESS)
            task_result.success = True
            task_result.skipped = True
            task_result.completed.set()
            return

        target_desc = f"--bin {bin_name}" if bin_name else ""
        print_log("Build", f"Building {package} {target_desc} in {repo_dir}...".replace("  ", " "), Colors.BUILD)

        cmd = ["cargo", "build", "-p", package]
        if bin_name:
            cmd.extend(["--bin", bin_name])
        process = subprocess.Popen(
            cmd,
            cwd=repo_dir,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1
        )

        # Stream filtered output
        while True:
            if INIT_FAILED.is_set():
                process.terminate()
                return

            line = process.stdout.readline()
            if not line and process.poll() is not None:
                break
            if line:
                clean_line = line.strip()
                # Filter to reduce noise
                if any(keyword in clean_line for keyword in ["Compiling", "Finished", "error", "Error"]):
                    target_label = bin_name if bin_name else package
                    print_log("Build", f"[{target_label}] {clean_line}", Colors.BUILD)

        rc = process.poll()
        if rc == 0:
            target_desc = f"--bin {bin_name}" if bin_name else ""
            print_log("Build", f"Successfully built {package} {target_desc}".replace("  ", " "), Colors.SUCCESS)
            task_result.success = True
        else:
            target_desc = f"--bin {bin_name}" if bin_name else ""
            error_msg = f"Failed to build {package} {target_desc} (exit code {rc})".replace("  ", " ")
            print_log("Build", error_msg, Colors.ERROR)
            task_result.error_message = error_msg
            INIT_FAILED.set()

    except Exception as e:
        target_desc = f"--bin {bin_name}" if bin_name else ""
        error_msg = f"Exception building {package} {target_desc}: {e}".replace("  ", " ")
        print_log("Build", error_msg, Colors.ERROR)
        task_result.error_message = error_msg
        INIT_FAILED.set()
    finally:
        task_result.completed.set()

def task_npm_build(work_dir, task_result, dependency=None):
    """Run npm install then npm run build in work_dir"""
    try:
        if dependency:
            dependency.completed.wait()
            if not dependency.success:
                print_log("NPM", f"Skipping npm build in {work_dir} due to dependency failure", Colors.WARN)
                task_result.completed.set()
                return

        for npm_cmd in [["npm", "install"], ["npm", "run", "build"]]:
            if INIT_FAILED.is_set():
                return

            cmd_str = " ".join(npm_cmd)
            print_log("NPM", f"Running '{cmd_str}' in {work_dir}...", Colors.BUILD)

            process = subprocess.Popen(
                npm_cmd,
                cwd=work_dir,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                bufsize=1
            )

            while True:
                if INIT_FAILED.is_set():
                    process.terminate()
                    return

                line = process.stdout.readline()
                if not line and process.poll() is not None:
                    break
                if line:
                    clean_line = line.strip()
                    if clean_line:
                        print_log("NPM", f"[{Path(work_dir).name}] {clean_line}", Colors.BUILD)

            rc = process.poll()
            if rc != 0:
                error_msg = f"'{cmd_str}' failed in {work_dir} (exit code {rc})"
                print_log("NPM", error_msg, Colors.ERROR)
                task_result.error_message = error_msg
                INIT_FAILED.set()
                return

        print_log("NPM", f"Successfully built {work_dir}", Colors.SUCCESS)
        task_result.success = True

    except Exception as e:
        error_msg = f"Exception running npm build in {work_dir}: {e}"
        print_log("NPM", error_msg, Colors.ERROR)
        task_result.error_message = error_msg
        INIT_FAILED.set()
    finally:
        task_result.completed.set()

def task_pnpm_install(papi_dir, task_result, dependencies=None):
    """Install pnpm dependencies"""
    try:
        # Wait for all dependencies if specified
        if dependencies:
            for dep in dependencies:
                dep.completed.wait()
            if any(not dep.success for dep in dependencies):
                print_log("PNPM", "Skipping pnpm install due to dependency failure", Colors.WARN)
                task_result.completed.set()
                return

        # Check if already installed
        if check_node_modules_exists(papi_dir):
            print_log("PNPM", f"Dependencies in {papi_dir} already installed, skipping", Colors.SUCCESS)
            task_result.success = True
            task_result.completed.set()
            return

        print_log("PNPM", f"Installing dependencies in {papi_dir}...", Colors.PNPM)

        cmd = ["corepack", "pnpm", "install"]
        process = subprocess.Popen(
            cmd,
            cwd=papi_dir,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            bufsize=1
        )

        # Stream output
        while True:
            if INIT_FAILED.is_set():
                process.terminate()
                return

            line = process.stdout.readline()
            if not line and process.poll() is not None:
                break
            if line:
                clean_line = line.strip()
                if clean_line:  # Only print non-empty lines
                    print_log("PNPM", clean_line, Colors.PNPM)

        rc = process.poll()
        if rc == 0:
            print_log("PNPM", "Successfully installed dependencies", Colors.SUCCESS)
            task_result.success = True
        else:
            error_msg = f"Failed to install dependencies (exit code {rc})"
            print_log("PNPM", error_msg, Colors.ERROR)
            task_result.error_message = error_msg
            INIT_FAILED.set()

    except Exception as e:
        error_msg = f"Exception installing dependencies: {e}"
        print_log("PNPM", error_msg, Colors.ERROR)
        task_result.error_message = error_msg
        INIT_FAILED.set()
    finally:
        task_result.completed.set()

def task_create_placeholder_file(file_path, task_result, dependency=None):
    """Create an empty placeholder file"""
    try:
        # Wait for dependency if specified
        if dependency:
            dependency.completed.wait()
            if not dependency.success:
                print_log("Placeholder", "Skipping placeholder creation due to dependency failure", Colors.WARN)
                task_result.completed.set()
                return

        # Create parent directories if they don't exist
        file_path_obj = Path(file_path)
        file_path_obj.parent.mkdir(parents=True, exist_ok=True)

        # Create empty file
        file_path_obj.touch()
        print_log("Placeholder", f"Created placeholder file: {file_path}", Colors.SUCCESS)
        task_result.success = True

    except Exception as e:
        error_msg = f"Exception creating placeholder file: {e}"
        print_log("Placeholder", error_msg, Colors.ERROR)
        task_result.error_message = error_msg
        INIT_FAILED.set()
    finally:
        task_result.completed.set()

# --- Orchestration ---

def initialize_demo():
    """Run the initialization phase with 3 parallel phases"""
    print_log("Init", "Starting initialization...", Colors.INFO)

    # Repository configuration
    repos = [
        ("https://github.com/ChainSafe/polkadot-sdk", "haiko-webrtc-demo", "./polkadot-sdk"),
        ("https://github.com/haikoschol/papi-console", "webrtc-demo", "./papi-console"),
        ("https://github.com/ChainSafe/litep2p-perf", "haiko-capture-traffic", "./litep2p-perf"),
        ("https://github.com/ChainSafe/smoldot", "haiko-webrtc-deadlock-fix", "./smoldot"),
    ]

    # Phase 1: Clone repositories in parallel
    print_log("Init", "Phase 1: Cloning repositories...", Colors.INFO)
    clone_results = []
    clone_threads = []

    for url, branch, dest in repos:
        result = TaskResult(f"clone-{dest}")
        clone_results.append(result)
        thread = threading.Thread(
            target=task_clone_repo,
            args=(url, branch, dest, result)
        )
        thread.start()
        clone_threads.append(thread)

    # Wait for all clones to complete
    for thread in clone_threads:
        thread.join()

    # Check if any clone failed
    if INIT_FAILED.is_set():
        print_log("Init", "Initialization failed during clone phase", Colors.ERROR)
        return False

    # Phase 2: Build operations in parallel
    print_log("Init", "Phase 2: Building binaries and installing dependencies...", Colors.INFO)

    polkadot_result = TaskResult("build-polkadot")
    pcap_result = TaskResult("build-pcap-analyzer")
    smoldot_wasm_result = TaskResult("build-smoldot-wasm")
    pnpm_result = TaskResult("pnpm-install")

    polkadot_thread = threading.Thread(
        target=task_cargo_build_bin,
        args=("./polkadot-sdk", "polkadot", "./polkadot-sdk/target/debug/polkadot", polkadot_result, clone_results[0])
    )

    pcap_thread = threading.Thread(
        target=task_cargo_build_bin,
        args=("./litep2p-perf", "smoldot-automation", "./litep2p-perf/target/debug/pcap-analyzer", pcap_result, clone_results[2]),
        kwargs={"bin_name": "pcap-analyzer"}
    )

    smoldot_wasm_thread = threading.Thread(
        target=task_npm_build,
        args=("./smoldot/wasm-node/javascript", smoldot_wasm_result, clone_results[3])
    )

    # pnpm install waits for both the papi-console clone and the smoldot wasm build
    pnpm_thread = threading.Thread(
        target=task_pnpm_install,
        args=("./papi-console", pnpm_result),
        kwargs={"dependencies": [clone_results[1], smoldot_wasm_result]}
    )

    polkadot_thread.start()
    pcap_thread.start()
    smoldot_wasm_thread.start()
    pnpm_thread.start()

    # Wait for all builds to complete
    polkadot_thread.join()
    pcap_thread.join()
    smoldot_wasm_thread.join()
    pnpm_thread.join()

    # Check if any build failed
    if INIT_FAILED.is_set():
        print_log("Init", "Initialization failed during build phase", Colors.ERROR)
        return False

    # Phase 3: Create placeholder chainspec file for papi-console
    print_log("Init", "Phase 3: Creating placeholder chainspec file...", Colors.INFO)

    placeholder_result = TaskResult("create-placeholder")
    placeholder_thread = threading.Thread(
        target=task_create_placeholder_file,
        args=("./papi-console/src/state/chains/chainspecs/polkadot-dev-webrtc.ts", placeholder_result, pnpm_result)
    )

    placeholder_thread.start()
    placeholder_thread.join()

    # Check if placeholder creation failed
    if INIT_FAILED.is_set():
        print_log("Init", "Initialization failed during placeholder creation", Colors.ERROR)
        return False

    print_log("Init", "Initialization completed successfully!", Colors.SUCCESS)
    return True

# --- Main ---

def main():
    parser = argparse.ArgumentParser(
        description="Automated demo for Polkadot/Smoldot WebRTC"
    )
    parser.add_argument(
        "--skip-init",
        action="store_true",
        help="Skip initialization phase"
    )
    parser.add_argument(
        "--init-only",
        action="store_true",
        help="Run initialization only, then exit"
    )

    args = parser.parse_args()

    try:
        # Run initialization unless skipped
        if not args.skip_init:
            success = initialize_demo()
            if not success:
                print_log("System", "Initialization failed, exiting", Colors.ERROR)
                sys.exit(1)

        # Exit if init-only mode
        if args.init_only:
            sys.exit(0)

        # TODO: Rest of demo orchestration (not in scope for this plan)
        print_log("System", "Demo orchestration not yet implemented", Colors.INFO)

    except KeyboardInterrupt:
        print_log("System", "Interrupted by user, shutting down...", Colors.WARN)
        INIT_FAILED.set()
        sys.exit(0)

if __name__ == "__main__":
    main()
