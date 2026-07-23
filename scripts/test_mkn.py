"""MKN test suite: unit tests followed by integration tests.

Runs in two phases mirroring cargo test output:
  1. Unit tests   - manifest validation checks (no live nodes)
  2. Integration  - full orchestration tests (spawns real nodes)

Run from workspace root:
    python3 scripts/test_mkn.py [filters ...] [--exact]
"""

import subprocess
import contextlib
import io
import json
import sys
import os
import signal
import time
from argparse import ArgumentParser


# ---------------------------------------------------------------------------
# Subprocess helper
# ---------------------------------------------------------------------------

def run_cmd(args, timeout=30):
    """Run a subprocess with a timeout, merging stderr into stdout.

    Args:
        args (list[str]): Command and arguments to execute.
        timeout (int): Maximum seconds to wait before killing.

    Returns:
        tuple[int, str]: (returncode, combined output). returncode is
            -1 on timeout.
    """
    is_windows = sys.platform == "win32"
    kwargs = {}
    if is_windows:
        kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True

    try:
        proc = subprocess.Popen(
            args,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            **kwargs,
        )
        stdout, _ = proc.communicate(timeout=timeout)
        return proc.returncode, stdout
    except subprocess.TimeoutExpired:
        if is_windows:
            try:
                subprocess.run(
                    ["taskkill", "/F", "/T", "/PID", str(proc.pid)],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                )
            except Exception:
                pass
        else:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except ProcessLookupError:
                pass

        stdout, _ = proc.communicate()
        if stdout is None:
            stdout = ""
        return -1, stdout


# ---------------------------------------------------------------------------
# Test runner primitives
# ---------------------------------------------------------------------------

def run_suite(suite_name, tests):
    """Run a list of (name, callable) tests and report cargo-style output.

    Each callable must return True on pass, False on fail. Failure
    output is collected and printed at the end of the suite in a
    `failures:` block, mirroring cargo test output.

    Args:
        suite_name (str): Human-readable label for this suite.
        tests (list[tuple[str, callable]]): Ordered list of tests.

    Returns:
        tuple[int, int]: (passed, failed) counts.
    """
    print(f"     Running scripts/test_mkn.py ({suite_name})")
    print()
    print(f"running {len(tests)} tests")

    passed = 0
    failed_names = []
    failure_outputs = {}
    start = time.monotonic()

    for name, fn in tests:
        print(f"test {name} ... ", end="", flush=True)
        buf = io.StringIO()
        try:
            with contextlib.redirect_stdout(buf):
                ok = fn()
        except Exception as exc:
            ok = False
            print(f"panicked at: {exc}", file=buf)
        if ok:
            print("ok")
            passed += 1
        else:
            print("FAILED")
            failed_names.append(name)
            failure_outputs[name] = buf.getvalue()

    elapsed = time.monotonic() - start

    if failed_names:
        print()
        print("failures:")
        for name in failed_names:
            output = failure_outputs[name].strip()
            if output:
                print()
                print(f"---- {name} stdout ----")
                print(output)
        print()
        print("failures:")
        for name in failed_names:
            print(f"    {name}")

    status = "ok" if not failed_names else "FAILED"
    print()
    print(
        f"test result: {status}. "
        f"{passed} passed; {len(failed_names)} failed; "
        f"0 ignored; 0 measured; 0 filtered out; "
        f"finished in {elapsed:.2f}s"
    )
    print()
    return passed, len(failed_names)


# ---------------------------------------------------------------------------
# Unit tests — manifest validation (no live nodes)
# ---------------------------------------------------------------------------

MKN = "scripts/mkn.py"
MKN_DIR = "meerkat/tests/mkn"

VALIDATION_CASES = [
    ("invalid_port",
     "test_mkn_invalid_port.json", "cannot specify a port"),
    ("missing_alias",
     "test_mkn_missing_alias.json", "missing 'alias'"),
    ("empty_nodes_list",
     "test_mkn_empty_nodes.json", "'nodes' list cannot be empty"),
    ("duplicate_alias",
     "test_mkn_duplicate_alias.json", "Duplicate node alias detected"),
    ("invalid_alias_format",
     "test_mkn_invalid_alias_format.json",
     "must match alphanumeric/underscore format"),
    ("missing_type",
     "test_mkn_missing_type.json", "missing required 'type' key"),
    ("invalid_type",
     "test_mkn_invalid_type.json", "type must be 'server' or 'client'"),
    ("missing_file_or_cmd",
     "test_mkn_missing_file_or_cmd.json",
     "must specify either 'file' or 'cmd'"),
    ("invalid_cmd",
     "test_mkn_invalid_cmd.json", "'cmd' must be a list of strings"),
    ("invalid_port_type",
     "test_mkn_invalid_port_type.json", "'port' must be an integer"),
    ("server_with_relay",
     "test_mkn_server_relay.json", "cannot specify a relay"),
    ("invalid_relay_reference",
     "test_mkn_invalid_relay.json",
     "which does not exist in the manifest"),
    ("invalid_imports_format",
     "test_mkn_invalid_imports_format.json",
     "must use 'alias.service_name' dot-notation"),
    ("invalid_imports_reference",
     "test_mkn_invalid_imports_reference.json",
     "imports from node 'missing' which does not exist"),
    ("circular_dependency",
     "test_mkn_circular_dependency.json",
     "Circular dependency detected in manifest"),
]


def make_validation_test(filename, expected_error):
    """Return a zero-argument callable for a single validation case.

    Args:
        filename (str): Manifest filename relative to MKN_DIR.
        expected_error (str): Substring expected in the error output.

    Returns:
        callable: Test function returning True on pass, False on fail.
    """
    def test():
        path = f"{MKN_DIR}/{filename}"
        code, output = run_cmd([sys.executable, MKN, path])
        if code == 0:
            print(
                f"\nFAIL: expected non-zero exit for {filename}; "
                f"got 0. Output:\n{output.strip()}"
            )
            return False
        if expected_error not in output:
            print(
                f"\nFAIL: expected '{expected_error}' in output "
                f"for {filename}. Got:\n{output.strip()}"
            )
            return False
        return True
    return test


def test_empty_expect_fail():
    """Verify that make_mkn_test rejects empty string expect_fail.

    Returns:
        bool: True if AssertionError was raised with expected message.
    """
    try:
        make_mkn_test("dummy.json", expect_fail="")
    except AssertionError as err:
        if "expect_fail string must not be empty" in str(err):
            return True
        print(f"\nFAIL: unexpected AssertionError message: {err}")
        return False
    print(
        "\nFAIL: expected AssertionError for expect_fail='' "
        "but none was raised."
    )
    return False


def unit_tests():
    """Return the full list of unit test (name, callable) pairs.

    Returns:
        list[tuple[str, callable]]: Unit test pairs.
    """
    tests = [
        (name, make_validation_test(filename, expected_error))
        for name, filename, expected_error in VALIDATION_CASES
    ]
    tests.append((
        "empty_expect_fail",
        test_empty_expect_fail,
    ))
    return tests


# ---------------------------------------------------------------------------
# Integration tests — full orchestration (spawns real nodes)
# ---------------------------------------------------------------------------

def make_mkn_test(manifest, expect_fail=False):
    """Return a zero-argument callable for a single MKN manifest test.

    Runs `mkn.py` against `manifest` and asserts the exit code and,
    when `expect_fail` is a non-empty string, that the error substring
    appears in the combined process output.

    Args:
        manifest (str): Workspace-relative path to the MKN manifest.
        expect_fail (bool | str): `False` for tests that must pass.
            A non-empty string for tests expected to fail with that
            substring present in the output. Must not be `True`.

    Returns:
        callable: Test function returning True on pass, False on fail.
    """
    assert isinstance(expect_fail, (bool, str)), (
        f"expect_fail must be False or a non-empty string, "
        f"got {type(expect_fail).__name__!r}: {expect_fail!r}"
    )
    assert expect_fail is not True, (
        "expect_fail=True is ambiguous; pass the expected error substring instead"
    )
    if isinstance(expect_fail, str):
        assert len(expect_fail) > 0, (
            "expect_fail string must not be empty"
        )

    def test():
        if not os.path.isfile(manifest):
            print(f"\nFAIL: manifest not found: {manifest}")
            return False
        code, output = run_cmd(
            [sys.executable, MKN, manifest],
            timeout=90,
        )
        if code == -1:
            print(
                f"\nFAIL: test timed out after 90s."
                f"\n{output.strip()}"
            )
            return False
        failed = code != 0
        if failed != bool(expect_fail):
            print(
                f"\nFAIL: exit code {code} "
                f"(expected {'non-zero' if expect_fail else 'zero'})."
                f"\n{output.strip()}"
            )
            return False
        if isinstance(expect_fail, str):
            if expect_fail.lower() not in output.lower():
                print(
                    f"\nFAIL: expected '{expect_fail}' in output."
                    f"\n{output.strip()}"
                )
                return False
        return True
    return test


def test_mkn_namespace_split():
    """Verify three-namespace tracking and relay routing via state dump.

    Returns:
        bool: True if the test passed.
    """
    code, output = run_cmd(
        [sys.executable, MKN,
         f"{MKN_DIR}/test_mkn_relay.json", "--dump-state"],
        timeout=90,
    )
    if code != 0:
        print(
            f"\nFAIL: namespace split exited {code}. "
            f"Output:\n{output.strip()}"
        )
        return False

    marker_start = "--- STATE DUMP ---"
    marker_end = "--- END STATE DUMP ---"
    if marker_start not in output or marker_end not in output:
        print("\nFAIL: state dump markers not found in output.")
        return False

    state_str = (
        output.split(marker_start)[1].split(marker_end)[0].strip()
    )
    try:
        state = json.loads(state_str)
    except Exception as exc:
        print(f"\nFAIL: could not parse state dump JSON: {exc}")
        return False

    relay = state.get("relay_node")
    client = state.get("relayed_client")
    if not relay or not client:
        print(
            "\nFAIL: relay_node or relayed_client missing from dump."
        )
        return False

    if "relay_svc" not in relay.get("local_services", {}):
        print("\nFAIL: relay_svc missing from relay local_services.")
        return False

    relayed = relay.get("relayed_services", {})
    if "client_svc" not in relayed:
        print(
            "\nFAIL: client_svc missing from relay relayed_services."
        )
        return False

    client_svc = relayed["client_svc"]
    if not client_svc.get("is_relayed"):
        print("\nFAIL: client_svc.is_relayed is false.")
        return False

    if client_svc.get("relay_peer_id") != relay.get("peer_id"):
        print(
            f"\nFAIL: relay_peer_id mismatch: "
            f"{client_svc.get('relay_peer_id')} != "
            f"{relay.get('peer_id')}"
        )
        return False

    if "relay_svc" not in client.get("remote_services", {}):
        print(
            "\nFAIL: relay_svc missing from client remote_services."
        )
        return False

    return True


def test_mkn_client_timeout_slow():
    """Verify a slow client that exceeds startup but not exec timeout.

    Returns:
        bool: True if the test passed.
    """
    code, output = run_cmd(
        [sys.executable, MKN,
         f"{MKN_DIR}/test_mkn_client_slow.json"],
        timeout=90,
    )
    if code != 0:
        print(
            f"\nFAIL: slow client exited {code}. "
            f"Output:\n{output.strip()}"
        )
        return False
    return True


def test_mkn_client_timeout_exec():
    """Verify a hanging client is terminated by the execution timeout.

    Returns:
        bool: True if the test passed.
    """
    code, output = run_cmd(
        [sys.executable, MKN,
         f"{MKN_DIR}/test_mkn_client_exec_timeout.json"],
        timeout=90,
    )
    if code == 0:
        print(
            "\nFAIL: hanging client exited 0; expected timeout failure."
        )
        return False
    if "execution timed out" not in output:
        print(
            "\nFAIL: 'execution timed out' not in output.\n"
            + output.strip()
        )
        return False
    return True


def test_mkn_missing_service():
    """Verify importing a missing service fails with a clear error.

    Returns:
        bool: True if the test passed.
    """
    code, output = run_cmd(
        [sys.executable, MKN,
         f"{MKN_DIR}/test_mkn_missing_service.json"],
        timeout=90,
    )
    if code == 0:
        print(
            "\nFAIL: missing service test exited 0; expected failure."
        )
        return False
    expected = (
        "imports missing service 'non_existent_svc' "
        "from online node 'basic_server'"
    )
    if expected not in output:
        print(
            f"\nFAIL: expected '{expected}' in output.\n"
            + output.strip()
        )
        return False
    return True


def integration_tests():
    """Return the full list of integration test (name, callable) pairs.

    Returns:
        list[tuple[str, callable]]: Integration test pairs.
    """
    IDIR = "scripts/tests/integration"
    return [
        # --- bespoke orchestration tests ---
        ("mkn_namespace_split",       test_mkn_namespace_split),
        ("mkn_client_timeout_slow",   test_mkn_client_timeout_slow),
        ("mkn_client_timeout_exec",   test_mkn_client_timeout_exec),
        ("mkn_missing_service",       test_mkn_missing_service),
        # --- import integration tests ---
        ("microservices_dag",         make_mkn_test(f"{IDIR}/imports/microservices/microservices.json")),
        ("transitive_pipeline",       make_mkn_test(f"{IDIR}/imports/pipeline/pipeline.json")),
        ("diamond_topology",          make_mkn_test(f"{IDIR}/imports/diamond/diamond.json")),
        ("action_cross_node",         make_mkn_test(f"{IDIR}/imports/action/action.json")),
        ("file_imports",              make_mkn_test(f"{IDIR}/imports/file_imports/file_imports.json")),
        ("mixed_imports",             make_mkn_test(f"{IDIR}/imports/mixed_imports/mixed_imports.json")),
        ("rejection_imports",         make_mkn_test(f"{IDIR}/imports/rejection_imports/rejection_imports.json",
                                                    expect_fail="Unknown identifier")),
        ("circular_imports",          make_mkn_test(f"{IDIR}/imports/circular_imports/circular_imports.json",
                                                    expect_fail="Circular dependency detected")),
        ("cyclic_member_imports",     make_mkn_test(f"{IDIR}/imports/cyclic_member_imports/cyclic_member_imports.json",
                                                    expect_fail="dependency cycle detected")),
        # --- lock group integration tests ---
        ("cascade_lock_success",      make_mkn_test(f"{IDIR}/lock_group/success.json")),
        ("cascade_abort_wait_die",    make_mkn_test(f"{IDIR}/lock_group/abort.json")),
        ("cascade_lock_wait",         make_mkn_test(f"{IDIR}/lock_group/wait.json")),
    ]


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def _validate_unit_fixtures():
    """Assert every VALIDATION_CASES fixture file exists on disk.

    Raises:
        SystemExit: If any fixture file is missing.
    """
    missing = [
        f"{MKN_DIR}/{filename}"
        for _, filename, _ in VALIDATION_CASES
        if not os.path.isfile(f"{MKN_DIR}/{filename}")
    ]
    if missing:
        print("FAIL: missing unit test fixture files:", file=sys.stderr)
        for path in missing:
            print(f"  {path}", file=sys.stderr)
        sys.exit(1)


def _validate_no_duplicate_names(label, tests):
    """Assert all test names in `tests` are unique.

    Args:
        label (str): Suite label for error messages.
        tests (list[tuple[str, callable]]): Test list to validate.

    Raises:
        SystemExit: If any duplicate names are found.
    """
    seen = set()
    duplicates = []
    for name, _ in tests:
        if name in seen:
            duplicates.append(name)
        seen.add(name)
    if duplicates:
        print(
            f"FAIL: duplicate test names in {label}: {duplicates}",
            file=sys.stderr,
        )
        sys.exit(1)


def main():
    """Run unit tests then integration tests and report overall result.

    Parses command-line arguments to allow substring or exact matching
    filters on test names, mirroring `cargo test` behavior
    """
    _validate_unit_fixtures()

    parser = ArgumentParser(
        description=(
            "Run MKN unit and integration test suite with filtering"
        )
    )
    parser.add_argument(
        "filters",
        nargs="*",
        help="Optional filters for test names",
    )
    parser.add_argument(
        "--exact",
        action="store_true",
        help="Require exact test name matching",
    )
    args = parser.parse_args()

    assert isinstance(args.filters, list), (
        "args.filters must be a list"
    )
    assert isinstance(args.exact, bool), (
        "args.exact must be a boolean"
    )

    utests = unit_tests()
    itests = integration_tests()
    _validate_no_duplicate_names("unit tests", utests)
    _validate_no_duplicate_names("integration tests", itests)

    if len(args.filters) > 0:
        if args.exact == True:
            utests = [
                (name, fn)
                for name, fn in utests
                if name in args.filters
            ]
            itests = [
                (name, fn)
                for name, fn in itests
                if name in args.filters
            ]
        else:
            utests = [
                (name, fn)
                for name, fn in utests
                if any(f in name for f in args.filters)
            ]
            itests = [
                (name, fn)
                for name, fn in itests
                if any(f in name for f in args.filters)
            ]

    total_passed = 0
    total_failed = 0

    if len(utests) > 0:
        p, f = run_suite("unit tests", utests)
        total_passed += p
        total_failed += f

    if len(itests) > 0:
        p, f = run_suite("integration tests", itests)
        total_passed += p
        total_failed += f

    overall = "ok" if total_failed == 0 else "FAILED"
    print(
        f"overall test result: {overall}. "
        f"{total_passed} passed; {total_failed} failed."
    )
    sys.exit(0 if total_failed == 0 else 1)


if __name__ == "__main__":
    main()
