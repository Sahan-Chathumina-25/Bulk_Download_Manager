"""Script launcher for the Bulk Download Manager.

Usage (Windows PowerShell, from the project root)::

    .venv\\Scripts\\python.exe run.py
"""
from __future__ import annotations

import sys

from src.main import main


def _run() -> int:
    """Run ``src.main:main`` and propagate the exit code."""
    return main(sys.argv[1:])


if __name__ == "__main__":
    raise SystemExit(_run())
