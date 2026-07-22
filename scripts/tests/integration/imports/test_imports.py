"""Integration test runner for MKN distributed import tests.

Runs the MKN (Meerkat Network Orchestrator) import test suite and
reports results in cargo-style test output format.

Location: scripts/tests/integration/imports/test_imports.py
Run from workspace root:
    python3 scripts/tests/integration/imports/test_imports.py
"""

import subprocess
import sys
import time

MANIFESTS = [
    (
        "microservices_dag",
        "scripts/tests/integration/imports/microservices/microservices.json",
        False,
    ),
    (
        "transitive_pipeline",
        "scripts/tests/integration/imports/pipeline/pipeline.json",
        False,
    ),
    (
        "diamond_topology",
        "scripts/tests/integration/imports/diamond/diamond.json",
        False,
    ),
    (
        "action_cross_node",
        "scripts/tests/integration/imports/action/action.json",
        False,
    ),
    (
        "file_imports",
        "scripts/tests/integration/imports/file_imports/file_imports.json",
        False,
    ),
    (
        "mixed_imports",
        "scripts/tests/integration/imports/mixed_imports/mixed_imports.json",
        False,
    ),
    (
        "rejection_imports",
        "scripts/tests/integration/imports/rejection_imports/rejection_imports.json",
        True,
    ),
]


def run_mkn(name, manifest, expect_fail=False):
    """Run a single MKN test manifest.

    Args:
        name (str): Short name of the test case.
        manifest (str): Path to the JSON manifest file.
        expect_fail (bool): True if non-zero exit code is expected.

    Returns:
        bool: True if the test met expectation, False otherwise.
    """
    print(f"test {name} ... ", end="", flush=True)
    res = subprocess.run(
        [sys.executable, "scripts/mkn.py", manifest],
        capture_output=True,
        text=True,
    )
    failed = res.returncode != 0
    if failed != expect_fail:
        print("FAILED")
        if res.stdout is not None and len(res.stdout) > 0:
            print(res.stdout, end="")
        if res.stderr is not None and len(res.stderr) > 0:
            print(res.stderr, end="")
        return False
    print("ok")
    return True


def main():
    """Run all MKN import integration tests and report results."""
    runner = "scripts/tests/integration/imports/test_imports.py"
    print(f"     Running {runner} (mkn import integration tests)")
    print()
    print(f"running {len(MANIFESTS)} tests")

    passed = 0
    failed = 0
    start = time.monotonic()

    for name, manifest, expect_fail in MANIFESTS:
        if run_mkn(name, manifest, expect_fail) == True:
            passed += 1
        else:
            failed += 1


    elapsed = time.monotonic() - start
    status = "ok" if failed == 0 else "FAILED"

    print()
    print(
        f"test result: {status}. "
        f"{passed} passed; {failed} failed; "
        f"finished in {elapsed:.2f}s"
    )

    if failed > 0:
        sys.exit(1)


if __name__ == "__main__":
    main()
