"""
Meerkat Network Orchestrator

Purpose:
  Orchestrates and tests multi-node, relay, and multi-hop distributed topologies
  for the Meerkat programming language. It dynamically parses a network manifest,
  launches background server nodes, captures their runtime Peer IDs and service URLs,
  wires dependent nodes together, runs client tests in the foreground, and performs
  a clean shutdown of all background processes when completed.

Usage:
  python3 scripts/run_network.py [manifest_file_path]

Default manifest:
  scripts/default_manifest.txt
"""

import os
import sys
import time
import subprocess
import signal
import re
import shutil
import atexit

# Enable loopback-only local bindings by default
os.environ["MEERKAT_LOCAL"] = "1"

LOG_DIR = os.path.join("tmp", "logs")
processes = []

def cleanup_processes():
    global processes
    if not processes:
        return
    print("\nShutting down all Meerkat nodes...")
    for node_name, proc in processes:
        if proc.poll() is None:
            print(f"Stopping server '{node_name}' (PID: {proc.pid})...")
            try:
                proc.terminate()
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
            except Exception:
                pass
    processes = []
    print("Cleanup complete.")

def cleanup(sig=None, frame=None, exit_code=0):
    if isinstance(sig, int):
        sys.exit(128 + sig)
    sys.exit(exit_code)

# Register atexit handler to ensure processes are always killed
atexit.register(cleanup_processes)

# Register signal handlers for clean exits
signal.signal(signal.SIGINT, cleanup)
signal.signal(signal.SIGTERM, cleanup)

def main():
    global processes

    # Parse arguments
    manifest_path = "scripts/default_manifest.txt"
    if len(sys.argv) > 1:
        if sys.argv[1] in ("-h", "--help"):
            print("Usage: python3 scripts/run_network.py [manifest_file_path]")
            print("Default manifest: scripts/default_manifest.txt")
            sys.exit(0)
        manifest_path = sys.argv[1]

    if not os.path.isfile(manifest_path):
        print(f"Error: Manifest file '{manifest_path}' not found.")
        sys.exit(1)

    # Clean and recreate log directory
    if os.path.exists(LOG_DIR):
        shutil.rmtree(LOG_DIR)
    os.makedirs(LOG_DIR, exist_ok=True)

    print("===================================================")
    print("       Starting Meerkat Orchestrated Network       ")
    print("===================================================")
    print(f"Using manifest: {manifest_path}")
    print(f"Logs will be written to: {LOG_DIR}/")
    print("Offline/loopback mode is active (MEERKAT_LOCAL=1)")
    print("---------------------------------------------------")

    # Read manifest nodes
    nodes = []
    with open(manifest_path, 'r') as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith('#'):
                continue
            parts = [p.strip() for p in line.split(':')]
            if len(parts) < 3:
                continue
            node_name = parts[0]
            file_path = parts[1]
            port = parts[2]
            imports = parts[3] if len(parts) > 3 else ""
            nodes.append((node_name, file_path, port, imports))

    # Track started node URLs
    service_urls = {}

    for node_name, file_path, port, imports in nodes:
        # Resolve imports
        import_flags = []
        if imports:
            for imp in [i.strip() for i in imports.split(',') if i.strip()]:
                resolved_url = service_urls.get(imp)
                if not resolved_url:
                    url_file = os.path.join(LOG_DIR, f"{imp}.url")
                    if os.path.exists(url_file):
                        with open(url_file, 'r') as uf:
                            resolved_url = uf.read().strip()
                
                if not resolved_url:
                    print(f"Error: Node '{node_name}' imports '{imp}', but '{imp}' has not been started yet.")
                    cleanup(exit_code=1)

                import_flags.extend(["-i", resolved_url])

        log_file_path = os.path.join(LOG_DIR, f"{node_name}.log")

        if port.lower() == "client":
            # Client Node (runs in foreground)
            print(f"[{node_name}] Starting client node running '{file_path}'...")
            cmd = ["./target/debug/meerkat", "-f", file_path] + import_flags
            print(f"Executing: {' '.join(cmd)}")
            print("---------------------------------------------------")
            
            try:
                res = subprocess.run(cmd)
                if res.returncode != 0:
                    print(f"[{node_name}] Execution failed with code {res.returncode}. Output above.")
                    sys.exit(res.returncode)
            except Exception as e:
                print(f"[{node_name}] Execution failed: {e}")
                sys.exit(1)
        else:
            # Server Node (runs in background)
            print(f"[{node_name}] Starting server node on port {port} running '{file_path}'...")
            cmd = ["./target/debug/meerkat", "-s", "-f", file_path, "-p", port] + import_flags
            
            log_file = open(log_file_path, "w")
            proc = subprocess.Popen(cmd, stdout=log_file, stderr=subprocess.STDOUT, text=True)
            processes.append((node_name, proc))

            # Wait for the node to print its Service URL
            print(f"Waiting for '{node_name}' to generate its URL...")
            url_found = False
            svc_url = None
            
            for _ in range(100): # Up to 20 seconds
                time.sleep(0.2)
                if proc.poll() is not None:
                    print(f"Error: Server '{node_name}' crashed during startup. Log output:")
                    log_file.close()
                    with open(log_file_path, "r") as lf:
                        print(lf.read())
                    cleanup(exit_code=1)

                if os.path.exists(log_file_path):
                    with open(log_file_path, "r") as lf:
                        content = lf.read()
                        matches = re.findall(r"Service URL:\s+(\S+)", content)
                        if matches:
                            for url in matches:
                                svc_name = url.split('/')[-1]
                                service_urls[svc_name] = url
                            svc_url = matches[0]
                            url_found = True
                            break

            if not url_found:
                print(f"Error: Timeout waiting for server '{node_name}' to start. Log output:")
                log_file.close()
                with open(log_file_path, "r") as lf:
                    print(lf.read())
                cleanup(exit_code=1)

            log_file.close()
            service_urls[node_name] = svc_url
            
            # Save URL file for team integration
            url_file_path = os.path.join(LOG_DIR, f"{node_name}.url")
            with open(url_file_path, "w") as uf:
                uf.write(svc_url)
                
            print(f"[{node_name}] Started successfully! Service URL: {svc_url}\n")

    # If all nodes finished successfully
    print("\n===================================================")
    print("      All manifest nodes completed successfully     ")
    print("===================================================")
    cleanup()

if __name__ == "__main__":
    main()
