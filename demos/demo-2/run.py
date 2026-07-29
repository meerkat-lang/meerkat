"""
Meerkat Demo 2 Orchestrator

Usage:
    python3 demos/demo-2/run.py

Purpose:
    Orchestrates the Smart Grid Web Dashboard demo (Demo 2).
    1. Cleanly terminates any stale background listeners on ports 8000, 9240, 9241.
    2. Rebuilds WASM package if needed.
    3. Spawns local HTTP server on port 8000.
    4. Launches backend Meerkat server network (server.mkt).
    5. Polls socket until dashboard WebSocket port 9241 is accepting connections.
    6. Opens the default system browser to dashboard.mkt for WASM remote subscription.
    7. Displays an 8-second terminal countdown timer while user views initial state.
    8. Triggers controller.mkt over P2P network to push live OTA state updates.
    9. Gracefully shuts down all child processes on Ctrl+C.
"""

import os
import sys
import time
import socket
import subprocess
import signal
import atexit
import platform
import webbrowser

PROCS = []

def cleanup():
    """Terminate all background processes spawned by this orchestrator."""
    print("\n[demo-2] Shutting down demo processes...")
    for proc in PROCS:
        if proc.poll() is None:
            try:
                proc.terminate()
                proc.wait(timeout=3)
            except Exception:
                try:
                    proc.kill()
                except Exception:
                    pass
    PROCS.clear()
    print("[demo-2] Cleanup complete.")

def kill_port(port: int):
    """Kill any process currently bound to the specified TCP port.

    Uses platform-appropriate tools:
      - Windows: netstat + taskkill
      - macOS/Linux: lsof + kill
    """
    try:
        if platform.system() == "Windows":
            # netstat -ano lists PID in the last column for LISTENING entries
            res = subprocess.run(
                ["netstat", "-ano"],
                capture_output=True, text=True
            )
            for line in res.stdout.splitlines():
                if f":{port}" in line and "LISTENING" in line:
                    parts = line.split()
                    pid = parts[-1]
                    if pid.isdigit():
                        subprocess.run(
                            ["taskkill", "/F", "/PID", pid],
                            capture_output=True
                        )
        else:
            res = subprocess.run(
                ["lsof", "-t", f"-iTCP:{port}", "-sTCP:LISTEN"],
                capture_output=True, text=True
            )
            for pid in res.stdout.strip().split():
                if pid and pid.isdigit():
                    subprocess.run(["kill", "-9", pid], capture_output=True)
    except Exception:
        pass

def wait_for_port(host: str, port: int, timeout: float = 40.0) -> bool:
    """Poll a TCP socket until it is accepting connections.

    Args:
        host: Host IP string
        port: Target TCP port integer
        timeout: Maximum seconds to wait

    Returns:
        True if socket opened, False if timed out
    """
    start = time.time()
    while time.time() - start < timeout:
        try:
            with socket.create_connection((host, port), timeout=1.0):
                return True
        except (OSError, ConnectionRefusedError):
            time.sleep(0.5)
    return False

def main():
    atexit.register(cleanup)
    signal.signal(signal.SIGINT, lambda sig, frame: sys.exit(0))
    signal.signal(signal.SIGTERM, lambda sig, frame: sys.exit(0))

    # 1. Clean up any stale processes occupying ports 8000, 9240, 9241
    kill_port(8000)
    kill_port(9240)
    kill_port(9241)

    # 2. Build WASM package if missing
    pkg_dir = os.path.join("meerkat-wasm", "www", "pkg")
    if not os.path.exists(pkg_dir):
        print("[demo-2] Building WASM package...")
        res = subprocess.run(["wasm-pack", "build", "--target", "web", "--out-dir", "www/pkg"], cwd="meerkat-wasm")
        if res.returncode != 0:
            print("[demo-2] Error: WASM build failed.")
            sys.exit(1)

    # 3. Start HTTP server on port 8000
    print("[demo-2] Starting web server at http://localhost:8000...")
    http_proc = subprocess.Popen(
        [sys.executable, "-m", "http.server", "8000"],
        cwd=os.path.join("meerkat-wasm", "www"),
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    PROCS.append(http_proc)

    # 4. Start Meerkat server network
    manifest_path = os.path.join("demos", "demo-2", "manifest-server.json")
    print(f"[demo-2] Starting Meerkat server via {manifest_path}...")
    mkn_proc = subprocess.Popen([sys.executable, "scripts/mkn.py", manifest_path])
    PROCS.append(mkn_proc)

    # 5. Wait for WebSocket port 9241 to open
    print("[demo-2] Waiting for dashboard WebSocket port 9241 to initialize...")
    if wait_for_port("127.0.0.1", 9241, timeout=40.0):
        print("[demo-2] Dashboard WebSocket port 9241 is online!")
        target_url = "http://localhost:8000/?peer_id=12D3KooWSrbdDG8vbm4z3e1XLMzFAatPQc1VdkVDRHYG5SZbBGVk&path=dashboard.mkt"
        print(f"[demo-2] Opening browser: {target_url}")
        # webbrowser.open is platform-agnostic: works on macOS, Linux, and Windows
        webbrowser.open(target_url)
    else:
        print("[demo-2] Error: Timed out waiting for port 9241.")
        sys.exit(1)

    # 6. Live Countdown Sequence
    print("\n[demo-2] ==========================================================")
    print("[demo-2] Initial dashboard loaded! Initial state: 4.0 kW solar (80%), 85% battery.")
    print("[demo-2] Watch your browser window to observe the live update!")
    print("[demo-2] ==========================================================\n")

    for remaining in range(8, 0, -1):
        print(f"[demo-2] Triggering live OTA update in {remaining} seconds...", end="\r", flush=True)
        time.sleep(1)

    print("\n[demo-2] Triggering live OTA update now over P2P network...")

    # 7. Execute controller client update node targeting inverter, battery, and home on port 9240
    dashboard_peer = "12D3KooWSrbdDG8vbm4z3e1XLMzFAatPQc1VdkVDRHYG5SZbBGVk"
    controller_cmd = [
        "cargo", "run", "-p", "meerkat", "--", "--local",
        "-f", "demos/demo-2/controller.mkt",
        "-i", f"/ip4/127.0.0.1/tcp/9240/p2p/{dashboard_peer}/inverter",
        "-i", f"/ip4/127.0.0.1/tcp/9240/p2p/{dashboard_peer}/battery",
        "-i", f"/ip4/127.0.0.1/tcp/9240/p2p/{dashboard_peer}/home"
    ]
    update_res = subprocess.run(controller_cmd)
    if update_res.returncode == 0:
        print("\n[demo-2] OTA update successfully delivered! Browser dashboard updated live.")
    else:
        print("\n[demo-2] Warning: Controller update node exited with error code.")

    # 8. Keep orchestrator running until interrupted
    try:
        mkn_proc.wait()
    except KeyboardInterrupt:
        pass

if __name__ == "__main__":
    main()

