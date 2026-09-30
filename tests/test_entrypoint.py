import subprocess
import sys


def test_package_entrypoint_exposes_web_server_options():
    result = subprocess.run(
        [sys.executable, "-m", "rf_router_planner", "--help"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode == 0, result.stderr
    assert "--host" in result.stdout
    assert "--port" in result.stdout
