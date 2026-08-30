from __future__ import annotations

import argparse
import sys
from pathlib import Path

from .logging_config import configure_logging


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Terrain-aware RF router placement planner")
    parser.add_argument("--debug", action="store_true", help="Enable detailed logging")
    parser.add_argument("--log-file", type=Path, help="Write logs to a file")
    args = parser.parse_args(argv)
    configure_logging(args.debug, args.log_file)
    try:
        from PySide6.QtWidgets import QApplication
    except ImportError:
        print("PySide6 is not installed. Install with: pip install -e .", file=sys.stderr)
        return 2
    from .gui.main_window import MainWindow

    application = QApplication(sys.argv[:1])
    application.setApplicationName("RF Router Planner")
    window = MainWindow()
    window.show()
    return application.exec()


if __name__ == "__main__":
    raise SystemExit(main())
