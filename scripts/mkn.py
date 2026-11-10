"""
Meerkat Network Orchestrator (mkn)
Usage:
  python3 scripts/mkn.py [manifest_file_path] [--clean] [--dump-state]

Purpose:
  Orchestrates and tests multi-node, relay, and multi-hop distributed topologies
  for Meerkat. It dynamically parses a JSON network manifest, launches background server nodes,
  captures their runtime Peer IDs and service URLs, wires dependent nodes together, and
  performs a clean shutdown of all background processes when completed.
"""

import os
import sys
import argparse
import time
import subprocess
import signal
import re
import atexit
import queue
import threading
import json
import shutil
from datetime import datetime


def _safe_print(*args, **kwargs):
    """Print, but never raise.

    Used on every shutdown path. When the process reading this script's output
    has gone (for example `test_mkn.py` stopped with Ctrl-C), printing raises
    `BrokenPipeError`, and a print that raises before the nodes are terminated
    leaves them running on their fixed ports, where they break later runs
    (#204).
    """
    try:
        print(*args, **kwargs)
    except (OSError, ValueError):
        pass

def purge_mkn_logs(clean: bool, log_dir_session: str = None, success: bool = False):
    """Purges session log files or the entire mkn log directory.

    If clean is True, it will completely purge the mkn log directory.
    If success is True, it deletes only the logs from this specific session.
    If success is False, the session logs are preserved for debugging.

    Args:
        clean: A boolean flag indicating whether to delete the entire log directory.
        log_dir_session: The specific session directory path to remove on success.
        success: A boolean flag indicating whether the run succeeded.
    """
    mkn_dir = os.path.join("tmp", "logs", "mkn")
    if clean:
        shutil.rmtree(mkn_dir, ignore_errors=True)
        _safe_print("Purged entire log directory.")
    elif success and log_dir_session:
        shutil.rmtree(log_dir_session, ignore_errors=True)
        # If the parent directory 'tmp/logs/mkn' is empty, delete it as well
        if os.path.exists(mkn_dir) and not os.listdir(mkn_dir):
            try:
                os.rmdir(mkn_dir)
            except OSError:
                pass

class Service:
    """Granular tracking of a discovered service in the network."""
    def __init__(self, name: str, url: str, host_peer_id: str):
        """
        Initializes a Service tracker.

        Args:
            name: The service slug name.
            url: The canonical dialable URL/multiaddr of the service.
            host_peer_id: The PeerID of the node hosting the service.
        """
        self.name = name
        self.url = url
        self.host_peer_id = host_peer_id

        # Determine if this service is behind a relay proxy
        self.is_relayed = "/p2p-circuit/" in url
        self.relay_peer_id = self._extract_relay() if self.is_relayed else None

    def _extract_relay(self):
        """
        Parses the multiaddr to find the relay hop.
        Example URL: /ip4/127.0.0.1/tcp/9001/p2p/QmRelay123/p2p-circuit/p2p/QmClient456/svc_name
        We extract 'QmRelay123' as the relay_peer_id.

        Returns:
            The relay PeerID string if found, otherwise None.
        """
        match = re.search(r'/p2p/([^/]+)/p2p-circuit/', self.url)
        if match:
            return match.group(1)
        return None

class Node:
    """Models a running Meerkat node."""
    def __init__(self, manifest_def: dict):
        """
        Initializes a Node configuration and runtime tracking object.

        Args:
            manifest_def: Dictionary representing the parsed node configuration from the manifest.
        """
        self.alias = manifest_def["alias"]
        self.type = manifest_def["type"]
        self.source_file = manifest_def.get("file")
        self.cmd = manifest_def.get("cmd")
        self.port = manifest_def.get("port")
        self.relay = manifest_def.get("relay")
        self.timeout = manifest_def.get("timeout", 0)
        self.imports = manifest_def.get("imports", [])
        # How this node dials its imports: "tcp" (the default) uses each
        # service's TCP Service URL; "ws" uses the WebSocket address the
        # server prints for browser clients, so the test exercises the
        # WebSocket transport between native nodes.
        self.transport = manifest_def.get("transport", "tcp")
        self.peer_id = None
        # The WebSocket listen address a server prints, ending in /p2p/<id>
        self.ws_url = None

        self.is_started = False
        self.is_online = False
        self.is_finished = False
        self.exit_code = None
        self.proc = None
        self.log_file = None
        self.log_path = None
        # Monotonic, for timeouts: the wall clock can step, which WSL2 is known
        # to do, and a forward step looks like a timeout (#205)
        self.start_time = None
        # Wall clock at the same moment, reported only when a client times out
        self.start_wall_time = None

        # The Three Namespaces
        self.local_services = {}   # Dict[svc_name, Service]
        self.remote_services = {}  # Dict[svc_name, Service]
        self.relayed_services = {} # Dict[svc_name, Service]


def _resolve_python_command(command):
    """Use the interpreter running mkn for portable Python helper commands."""
    resolved = list(command)
    if resolved and resolved[0] in {"python", "python3"} and sys.executable:
        resolved[0] = sys.executable
    return resolved


class Manifest:
    """Parses and validates the Meerkat Network Orchestrator JSON manifest."""
    def __init__(self, file_path: str):
        """
        Loads the manifest from file and runs full schema, semantic, and cycle validations.

        Args:
            file_path: The absolute or relative path to the JSON manifest.

        Raises:
            ValueError: If parsing or validation fails.
        """
        self.file_path = file_path
        self.settings = {}
        self.nodes = []
        self.nodes_by_alias = {}

        try:
            with open(file_path, "r") as f:
                data = json.load(f)
        except Exception as e:
            raise ValueError(f"Failed to parse manifest JSON: {e}")

        if not isinstance(data, dict):
            raise ValueError("Manifest must be a JSON object")

        self.settings = data.get("settings", {})
        if not isinstance(self.settings, dict):
            raise ValueError("'settings' must be a JSON object")

        nodes_def = data.get("nodes")
        if nodes_def is None:
            raise ValueError("Manifest missing required 'nodes' key")
        if not isinstance(nodes_def, list):
            raise ValueError("'nodes' must be a list")
        if len(nodes_def) == 0:
            raise ValueError("'nodes' list cannot be empty")

        # 1. Parse and validate each node schema
        for idx, node_def in enumerate(nodes_def):
            if not isinstance(node_def, dict):
                raise ValueError(f"Node at index {idx} must be a JSON object")

            # alias check
            if "alias" not in node_def:
                raise ValueError(f"Node at index {idx} is missing 'alias'")
            alias = node_def["alias"]
            if not isinstance(alias, str) or not alias:
                raise ValueError(f"Node at index {idx} 'alias' must be a non-empty string")
            if not re.match(r'^[a-zA-Z0-9_]+$', alias):
                raise ValueError(f"Node alias '{alias}' must match alphanumeric/underscore format (^[a-zA-Z0-9_]+$)")
            if alias in self.nodes_by_alias:
                raise ValueError(f"Duplicate node alias detected: '{alias}'")

            # type check
            if "type" not in node_def:
                raise ValueError(f"Node '{alias}' is missing required 'type' key")
            node_type = node_def["type"]
            if node_type not in ("server", "client"):
                raise ValueError(f"Node '{alias}' type must be 'server' or 'client', got '{node_type}'")

            # file / cmd check
            if "file" not in node_def and "cmd" not in node_def:
                raise ValueError(f"Node '{alias}' must specify either 'file' or 'cmd'")
            if "file" in node_def and "cmd" in node_def:
                raise ValueError(f"Node '{alias}' cannot specify both 'file' and 'cmd'")
            if "file" in node_def and not isinstance(node_def["file"], str):
                raise ValueError(f"Node '{alias}' 'file' must be a string")
            if "cmd" in node_def:
                if not isinstance(node_def["cmd"], list) or not all(isinstance(x, str) for x in node_def["cmd"]):
                    raise ValueError(f"Node '{alias}' 'cmd' must be a list of strings")

            # port check
            if "port" in node_def:
                if node_type == "client":
                    raise ValueError(f"Client node '{alias}' cannot specify a port number")
                if node_def["port"] is not None and not isinstance(node_def["port"], int):
                    raise ValueError(f"Server node '{alias}' 'port' must be an integer")
                if node_def["port"] <= 0 or node_def["port"] > 65535:
                    raise ValueError(f"Server node '{alias}' 'port' must be between 1 and 65535")

            # relay check
            if "relay" in node_def:
                if node_type == "server":
                    raise ValueError(f"Server node '{alias}' cannot specify a relay")
                relay = node_def["relay"]
                if not isinstance(relay, str) or not relay:
                    raise ValueError(f"Node '{alias}' 'relay' must be a non-empty string")

            # transport check
            if "transport" in node_def and node_def["transport"] not in ("tcp", "ws"):
                raise ValueError(f"Node '{alias}' 'transport' must be 'tcp' or 'ws', got {node_def['transport']!r}")

            # imports check
            imports = node_def.get("imports", [])
            if not isinstance(imports, list) or not all(isinstance(x, str) for x in imports):
                raise ValueError(f"Node '{alias}' 'imports' must be a list of strings")
            for imp in imports:
                parts = imp.split('.')
                if len(parts) != 2 or not parts[0] or not parts[1]:
                    raise ValueError(f"Node '{alias}' import '{imp}' must use 'alias.service_name' dot-notation")

            self.nodes_by_alias[alias] = node_def
            self.nodes.append(node_def)

        # 2. Semantic lookup checks
        for node_def in self.nodes:
            alias = node_def["alias"]

            # relay check
            if "relay" in node_def:
                relay = node_def["relay"]
                if relay not in self.nodes_by_alias:
                    raise ValueError(f"Node '{alias}' specifies relay '{relay}' which does not exist in the manifest")

            # imports check
            for imp in node_def.get("imports", []):
                dep_alias = imp.split('.')[0]
                if dep_alias not in self.nodes_by_alias:
                    raise ValueError(f"Node '{alias}' imports from node '{dep_alias}' which does not exist in the manifest")

        # 3. Cycle checks
        if self.check_cycles():
            raise ValueError("Circular dependency detected in manifest imports/relays")

    def check_cycles(self) -> bool:
        """
        Constructs a dependency graph and detects cycles via Depth-First Search.

        Returns:
            True if a cycle is found, otherwise False.
        """
        adj = {alias: [] for alias in self.nodes_by_alias}
        for alias, node_def in self.nodes_by_alias.items():
            if "relay" in node_def:
                adj[alias].append(node_def["relay"])
            for imp in node_def.get("imports", []):
                dep_alias = imp.split('.')[0]
                if dep_alias in adj:
                    adj[alias].append(dep_alias)

        visited = {alias: 0 for alias in self.nodes_by_alias} # 0=unvisited, 1=visiting, 2=visited

        def dfs(u):
            visited[u] = 1
            for v in adj[u]:
                if visited[v] == 1:
                    return True
                elif visited[v] == 0:
                    if dfs(v):
                        return True
            visited[u] = 2
            return False

        for alias in self.nodes_by_alias:
            if visited[alias] == 0:
                if dfs(alias):
                    return True
        return False

def extract_peer_id(url: str) -> str:
    """
    Parses a multiaddr URL string to extract the final cryptographic PeerID.

    Args:
        url: The multiaddr address.

    Returns:
        The PeerID string if found, otherwise None.
    """
    matches = re.findall(r'/p2p/([^/]+)', url)
    if matches:
        return matches[-1]
    return None

class NetworkOrchestrator:
    """Stateful manager of the test network."""
    def __init__(self, manifest: Manifest, clean: bool = False):
        """Initializes the network orchestrator state, directories, and node models.

        Args:
            manifest: A validated Manifest configuration instance.
            clean: A boolean flag indicating whether the entire mkn log directory
                should be purged on exit.
        """
        self.manifest = manifest
        self.clean = clean
        self.success = False
        self._cleaned_up = False
        self.nodes_by_alias = {} # Dict[alias, Node]
        self.nodes_by_peer_id = {} # Dict[peer_id, Node]

        # Session config
        self.session_id = f"{datetime.now().strftime('%Y%m%d_%H%M%S')}_{os.getpid()}"
        self.log_dir = os.path.join("tmp", "logs")
        self.log_dir_session = os.path.join(self.log_dir, "mkn", self.session_id)

        # Queue for thread-safe line reading
        self.line_queue = queue.Queue()
        self.threads = []

        # Settings
        settings = manifest.settings
        self.startup_timeout = float(settings.get("startup_timeout", 10.0))
        self.monitor_poll_interval = float(settings.get("monitor_poll_interval", 0.25))
        self.terminate_timeout = float(settings.get("terminate_timeout", 5.0))
        self.client_node_timeout = 10.0

        os.makedirs(os.path.join(self.log_dir_session, "clients"), exist_ok=True)
        os.makedirs(os.path.join(self.log_dir_session, "servers"), exist_ok=True)

        # Instantiate Node models
        for alias, node_def in manifest.nodes_by_alias.items():
            node = Node(node_def)
            log_dir_type = "clients" if node.type == "client" else "servers"
            node.log_path = os.path.join(self.log_dir_session, log_dir_type, f"{node.alias}.log")
            self.nodes_by_alias[alias] = node

        # Precalculate the set of dependent node aliases to optimize dependency checks.
        # This optimizes the _is_node_dependency lookup from O(N * M) to O(1) amortized time,
        # where N is the number of nodes and M is the maximum imports per node.
        self.dependency_aliases = set()
        for node_def in manifest.nodes_by_alias.values():
            if "relay" in node_def and node_def["relay"]:
                self.dependency_aliases.add(node_def["relay"])
            for imp in node_def.get("imports", []):
                self.dependency_aliases.add(imp.split('.')[0])

    def _reader_thread_func(self, node):
        """
        Thread target function. Reads lines from a node's stdout stream, writes
        them to the local log file, and pushes them to the orchestrator line queue.

        Args:
            node: The Node instance being read.
        """
        try:
            for line in iter(node.proc.stdout.readline, ''):
                node.log_file.write(line)
                node.log_file.flush()
                self.line_queue.put((node, line))
        except Exception as e:
            print(f"Error in reader thread for {node.alias}: {e}")
        finally:
            node.log_file.close()

    def spawn_node(self, node):
        """
        Constructs the command line, resolves dynamic dependency URL values,
        spawns the subprocess, and registers the reader thread.

        Args:
            node: The Node to spawn.
        """
        import_flags = []
        for imp in node.imports:
            parts = imp.split('.')
            alias = parts[0]
            svc_name = parts[1]
            dep_node = self.nodes_by_alias[alias]

            svc = dep_node.local_services.get(svc_name)
            if not svc:
                svc = dep_node.relayed_services.get(svc_name)

            node.remote_services[svc_name] = svc
            url = svc.url
            if node.transport == "ws":
                if svc.is_relayed:
                    raise RuntimeError(f"Node '{node.alias}' imports relayed service '{svc_name}', which has no WebSocket address")
                if not dep_node.ws_url:
                    raise RuntimeError(f"Node '{node.alias}' imports '{svc_name}' over WebSocket, but '{alias}' printed no WebSocket address")
                url = f"{dep_node.ws_url}/{svc_name}"
            import_flags.extend(["-i", url])

        if node.cmd:
            cmd = _resolve_python_command(node.cmd)
            if node.relay:
                relay_node = self.nodes_by_alias[node.relay]
                cmd.extend(["--relay", relay_node.peer_id])
            cmd.extend(import_flags)
        else:
            cmd = ["cargo", "run", "-p", "meerkat", "--", "--local"]
            if node.type == "server":
                cmd.append("-s")
            cmd.extend(["-f", node.source_file])
            if node.type == "server" and node.port is not None:
                cmd.extend(["-p", str(node.port)])
            cmd.extend(import_flags)

        print(f"[{node.alias}] Spawning {node.type} node...")
        print(f"Executing: {' '.join(cmd)}")
        print("---------------------------------------------------")

        log_file = open(node.log_path, "w")
        node.log_file = log_file
        proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, bufsize=1)
        node.proc = proc
        node.start_time = time.monotonic()
        node.start_wall_time = time.time()
        node.is_started = True

        t = threading.Thread(target=self._reader_thread_func, args=(node,))
        t.daemon = True
        t.start()
        self.threads.append(t)

    def parse_line(self, node, line):
        """
        Parses a single stdout/stderr line from a node's stream to extract PeerIDs,
        register service URLs, map circuit relays, and determine online markers.

        Args:
            node: The Node that produced the line.
            line: The raw line string.
        """
        line = line.strip()

        # 1. Parse listening address: Server listening at: <url>
        match_listen = re.match(r'^Server listening at:\s+(\S+)$', line)
        if match_listen:
            url = match_listen.group(1)
            peer_id = extract_peer_id(url)
            if peer_id:
                node.peer_id = peer_id
                self.nodes_by_peer_id[peer_id] = node
                print(f"[{node.alias}] Discovered PeerID: {peer_id}")

        # 1b. Parse the WebSocket address: Browser clients connect at: <url>
        match_ws = re.match(r'^Browser clients connect at:\s+(\S+)$', line)
        if match_ws:
            node.ws_url = match_ws.group(1)

        # 2. Parse service URLs: Service URL: <url>
        match_svc = re.match(r'^Service URL:\s+(\S+)$', line)
        if match_svc:
            url = match_svc.group(1)
            svc_name = url.split('/')[-1]
            peer_id = extract_peer_id(url)

            if node.peer_id is None and peer_id:
                node.peer_id = peer_id
                self.nodes_by_peer_id[peer_id] = node
                print(f"[{node.alias}] Discovered PeerID: {peer_id}")

            svc = Service(svc_name, url, peer_id)
            node.local_services[svc_name] = svc

            if svc.is_relayed:
                relay_node = None
                for n in self.nodes_by_alias.values():
                    if n.peer_id == svc.relay_peer_id:
                        relay_node = n
                        break
                if relay_node:
                    relay_node.relayed_services[svc_name] = svc
                    print(f"[{node.alias}] Service '{svc_name}' (relayed via relay '{relay_node.alias}') is registered.")
                else:
                    print(f"[{node.alias}] Warning: Service '{svc_name}' is relayed via relay peer ID {svc.relay_peer_id}, but relay not found!")
            else:
                print(f"[{node.alias}] Service '{svc_name}' is online at {url}")

        # 3. Parse online marker: Server running...
        if "Server running, press Ctrl+C to stop..." in line:
            node.is_online = True
            print(f"[{node.alias}] All services online.")

    def spawn_resolved_nodes(self):
        """
        Scans all pending nodes and spawns them if all of their imports and
        relay dependencies are fully initialized and online.
        """
        for node in self.nodes_by_alias.values():
            if node.is_started:
                continue

            imports_resolved = True
            if node.relay:
                dep_node = self.nodes_by_alias[node.relay]
                # Prevent edge case where online, but peer id not assigned
                if (not dep_node.is_online) or (not dep_node.peer_id):
                    imports_resolved = False

            if imports_resolved:
                for imp in node.imports:
                    parts = imp.split('.')
                    dep_alias = parts[0]
                    svc_name = parts[1]
                    dep_node = self.nodes_by_alias[dep_alias]

                    if not dep_node.is_online:
                        imports_resolved = False
                        break
                    if svc_name not in dep_node.local_services and svc_name not in dep_node.relayed_services:
                        raise RuntimeError(f"Node '{node.alias}' imports missing service '{svc_name}' from online node '{dep_alias}'")

            if imports_resolved:
                self.spawn_node(node)

    def process_output_queue(self):
        """
        Drains the thread-safe queue and parses all queued stdout/stderr lines.
        """
        while not self.line_queue.empty():
            try:
                node, line = self.line_queue.get_nowait()
                self.parse_line(node, line)
            except queue.Empty:
                break

    def _is_node_dependency(self, node):
        """
        Determines if any other node in the network depends on the given node
        either as a relay or via service imports.

        This check uses a precalculated set of dependency aliases computed
        during initialization. This optimizes the query from a nested graph search
        of O(N * M) time complexity down to a simple set lookup of O(1) time complexity,
        where N is the number of nodes and M is the maximum imports per node.

        Args:
            node: The Node instance to check.

        Returns:
            True if the node is depended upon by another node, otherwise False.
        """
        return node.alias in self.dependency_aliases

    def check_nodes_status(self):
        """
        Inspects process exit codes, monitors client execution timeouts,
        and enforces server startup time limits.

        Raises:
            RuntimeError: If a server node terminates unexpectedly.
            TimeoutError: If a node times out during startup or execution.
        """
        now = time.monotonic()
        for node in self.nodes_by_alias.values():
            if not node.is_started or node.is_finished:
                continue

            # A. Check for exit
            status = node.proc.poll()
            if status is not None:
                node.is_finished = True
                node.exit_code = status
                time.sleep(0.05)
                self.process_output_queue()

                if node.type == "server":
                    print(f"\nError: Server node '{node.alias}' (PID: {node.proc.pid}) exited unexpectedly with code {status}.")
                    self.dump_node_log(node)
                    raise RuntimeError(f"Server node '{node.alias}' exited unexpectedly")
                else:
                    print(f"[{node.alias}] Client node completed with exit code {status}.")
                    self.dump_node_log(node)

            # B. Check for startup timeout
            elif not node.is_online and (node.type == "server" or self._is_node_dependency(node)):
                if now - node.start_time > self.startup_timeout:
                    print(f"\nError: Timeout waiting for node '{node.alias}' to initialize after {self.startup_timeout} seconds.")
                    self.dump_node_log(node)
                    raise TimeoutError(f"Timeout waiting for node '{node.alias}' to start")

            # C. Check for client execution timeout
            elif node.type == "client":
                node_timeout = node.timeout if node.timeout > 0 else self.client_node_timeout
                if now - node.start_time > node_timeout:
                    print(f"\nError: Client node '{node.alias}' execution timed out after {node_timeout} seconds.")
                    print(
                        f"Elapsed since spawn: {now - node.start_time:.2f}s monotonic, "
                        f"{time.time() - node.start_wall_time:.2f}s wall clock."
                    )
                    self.dump_node_log(node)
                    raise TimeoutError(f"Client node '{node.alias}' execution timed out")

    def dump_node_log(self, node):
        """
        Prints the complete accumulated stdout/stderr log file of a node to console.

        Args:
            node: The Node whose log is to be dumped.
        """
        print(f"\n--- Log output for {node.type} '{node.alias}' ---")
        if os.path.exists(node.log_path):
            with open(node.log_path, "r") as lf:
                print(lf.read())
        print(f"--- End of log output for '{node.alias}' ---\n")

    def run(self):
        """Starts the orchestration main loop and handles network lifecycle.

        This method enters a polling loop where it resolves dependencies, spawns
        nodes, collects process output, and monitors status. If client nodes are
        configured, it waits for all client nodes to complete.

        Args:
            None.

        Returns:
            int: 0 if all client nodes exit successfully (exit code 0), or the
                exit code of the first failing client node.
        """
        print("===================================================")
        print("       Starting Meerkat Orchestrated Network       ")
        print("===================================================")
        print(f"Using manifest: {self.manifest.file_path}")
        print(f"Logs will be written to: {self.log_dir_session}/")
        print("Offline/loopback mode is active (--local flag enabled)")
        print("---------------------------------------------------")

        has_clients = any(node.type == "client" for node in self.nodes_by_alias.values())

        while True:
            self.spawn_resolved_nodes()
            self.process_output_queue()
            self.check_nodes_status()

            if has_clients:
                client_nodes = [node for node in self.nodes_by_alias.values() if node.type == "client"]
                if all(node.is_finished for node in client_nodes):
                    failed_clients = [node for node in client_nodes if node.exit_code != 0]
                    if failed_clients:
                        print(f"\nOrchestrator exiting: Client node(s) failed.")
                        return failed_clients[0].exit_code
                    else:
                        print("\n===================================================")
                        print("      All nodes ran successfully     ")
                        print("===================================================")
                        self.success = True
                        return 0
            else:
                pass

            time.sleep(self.monitor_poll_interval)

    def dump_state(self):
        """
        Prints a JSON dump of the final internal state (registry and namespaces).
        """
        state = {}
        for alias, node in self.nodes_by_alias.items():
            state[alias] = {
                "alias": node.alias,
                "peer_id": node.peer_id,
                "local_services": {name: {"name": svc.name, "url": svc.url, "host_peer_id": svc.host_peer_id, "is_relayed": svc.is_relayed, "relay_peer_id": svc.relay_peer_id} for name, svc in node.local_services.items()},
                "remote_services": {name: {"name": svc.name, "url": svc.url, "host_peer_id": svc.host_peer_id, "is_relayed": svc.is_relayed, "relay_peer_id": svc.relay_peer_id} for name, svc in node.remote_services.items()},
                "relayed_services": {name: {"name": svc.name, "url": svc.url, "host_peer_id": svc.host_peer_id, "is_relayed": svc.is_relayed, "relay_peer_id": svc.relay_peer_id} for name, svc in node.relayed_services.items()}
            }
        print("--- STATE DUMP ---")
        print(json.dumps(state, indent=2))
        print("--- END STATE DUMP ---")

    def cleanup(self):
        """Terminates spawned background processes and cleans up temporary files and logs.

        This method terminates all active subprocesses registered under the orchestrator.
        It also removes Python cache directories and, depending on the error state
        and cleanup options, deletes session or general log subdirectories.

        Note:
            If the orchestrator is terminated with a SIGKILL signal, this cleanup routine
            will be bypassed and temporary files/logs will remain on disk.

        Args:
            None.

        Returns:
            None.
        """
        if self._cleaned_up:
            return
        self._cleaned_up = True

        if self.nodes_by_alias:
            _safe_print("\nShutting down all Meerkat nodes...")
            # Signal every node before waiting on any, so that if a wait fails
            # or is interrupted (a second Ctrl-C), no node is left unsignalled
            # and holding its fixed port
            signalled = []
            for node in self.nodes_by_alias.values():
                if node.proc:
                    proc = node.proc
                    status = proc.poll()
                    if status is not None:
                        _safe_print(f"Node '{node.alias}' (PID: {proc.pid}) has already exited with code {status}.")
                    else:
                        _safe_print(f"Stopping node '{node.alias}' (PID: {proc.pid}) with terminate()...")
                        try:
                            proc.terminate()
                            signalled.append(node)
                        except Exception as e:
                            _safe_print(f"Error: Failed to stop node '{node.alias}' (PID: {proc.pid}): {e}", file=sys.stderr)
            for node in signalled:
                proc = node.proc
                try:
                    try:
                        exit_code = proc.wait(timeout=self.terminate_timeout)
                        _safe_print(f"Node '{node.alias}' (PID: {proc.pid}) stopped cleanly (Exit code: {exit_code}).")
                    except subprocess.TimeoutExpired:
                        _safe_print(f"Warning: Node '{node.alias}' (PID: {proc.pid}) did not terminate in time. Killing...")
                        proc.kill()
                        exit_code = proc.wait()
                        _safe_print(f"Node '{node.alias}' (PID: {proc.pid}) killed (Exit code: {exit_code}).")
                except Exception as e:
                    _safe_print(f"Error: Failed to stop node '{node.alias}' (PID: {proc.pid}): {e}", file=sys.stderr)
            self.nodes_by_alias.clear()
            _safe_print("Cleanup complete.")

        # Handle log directories cleanup
        purge_mkn_logs(clean=self.clean, log_dir_session=self.log_dir_session, success=self.success)

def main():
    """
    Main entry point: parses arguments, loads manifest, configures global
    signal and exception hooks, and triggers orchestrator execution.
    """
    reconfigure_stdout = getattr(sys.stdout, "reconfigure", None)
    if reconfigure_stdout is not None:
        reconfigure_stdout(errors="backslashreplace")

    parser = argparse.ArgumentParser(
        description="Meerkat Network Orchestrator (mkn): orchestrates and tests multi-node, relay, and multi-hop distributed topologies for Meerkat.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Manifest file format (JSON):
  A JSON file containing 'settings' and a list of 'nodes'.

  Example manifest:
  {
    "settings": {
      "startup_timeout": 10.0,
      "monitor_poll_interval": 0.25,
      "terminate_timeout": 5.0
    },
    "nodes": [
      {
        "alias": "server_node",
        "type": "server",
        "file": "meerkat/tests/net_orch1.mkt",
        "port": 9001
      },
      {
        "alias": "client_node",
        "type": "client",
        "file": "meerkat/tests/net_orch3.mkt",
        "relay": "server_node",
        "imports": ["server_node.node1_a"]
      }
    ]
  }
"""
    )
    parser.add_argument("manifest_file_path", nargs="?", help="Path to the JSON network manifest file")
    parser.add_argument("--clean", action="store_true", help="Purge the entire mkn log subdirectory on exit (can be run without a manifest)")
    parser.add_argument("--dump-state", action="store_true", help="Dump internal registry state on exit")
    args = parser.parse_args()

    # Handle standalone or early clean flag check
    if not args.manifest_file_path:
        if args.clean:
            purge_mkn_logs(clean=True)
            print("Cleanup completed.")
            sys.exit(0)
        else:
            parser.error("the following arguments are required: manifest_file_path")

    if not os.path.isfile(args.manifest_file_path):
        print(f"Error: Manifest file '{args.manifest_file_path}' not found.")
        sys.exit(1)

    manifest = None
    try:
        manifest = Manifest(args.manifest_file_path)
    except Exception as e:
        print(f"Pre-flight validation failed: {e}")
        sys.exit(1)

    orchestrator = NetworkOrchestrator(manifest, clean=args.clean)

    def exception_hook(exc_type, exc_value, exc_traceback):
        if issubclass(exc_type, KeyboardInterrupt):
            _safe_print("\nOrchestrator exiting: Terminated by user (KeyboardInterrupt)")
        else:
            _safe_print(f"\nOrchestrator crashed due to unhandled exception: {exc_type.__name__}: {exc_value}")
        orchestrator.cleanup()
        sys.__excepthook__(exc_type, exc_value, exc_traceback)

    sys.excepthook = exception_hook
    atexit.register(orchestrator.cleanup)

    def signal_handler(sig, frame):
        try:
            sig_name = signal.Signals(sig).name
        except Exception:
            sig_name = f"Signal {sig}"
        _safe_print(f"\nOrchestrator exiting: Terminated by signal: {sig_name}")
        orchestrator.cleanup()
        sys.exit(128 + sig)

    # Register signal handlers for clean termination.
    # Note: SIGKILL cannot be caught by Python, meaning any forced kill
    # bypasses these signal handlers and the atexit cleanup logic, leaving
    # logs and pycache on disk.
    signals = ["SIGINT", "SIGTERM"]
    if sys.platform == "win32":
        signals.append("SIGBREAK")
    else:
        signals.extend(["SIGHUP", "SIGQUIT"])
    for sig_name in signals:
        if hasattr(signal, sig_name):
            try:
                signal.signal(getattr(signal, sig_name), signal_handler)
            except (ValueError, OSError):
                pass

    exit_code = 1
    try:
        exit_code = orchestrator.run()
    except Exception as e:
        _safe_print(f"Orchestration run encountered an error: {e}")
        orchestrator.cleanup()

    if args.dump_state:
        orchestrator.dump_state()

    sys.exit(exit_code)

if __name__ == "__main__":
    main()
