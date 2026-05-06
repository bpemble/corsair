"""Shared pytest fixtures for corsair tests.

Currently a no-op other than wiring `src/` onto sys.path so test modules
can import `src.ipc.protocol` (etc.) when pytest is run from the repo
root. The richer config / market-data fixtures that lived here were
retired alongside the Python pricing/trader modules in the Phase 6.7
dead-code purge.
"""

import sys
from pathlib import Path

# Make `src` importable when pytest is run from repo root
ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
