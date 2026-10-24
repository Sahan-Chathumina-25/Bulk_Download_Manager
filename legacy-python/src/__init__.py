"""Bulk Download Manager Phase-1 core engine.

Package layout:

- ``src.core`` — download engine, validation, security, queueing
- ``src.providers`` — URL source adapters (plain HTTP today)
- ``src.database`` — SQLite persistence
- ``src.utils`` — cross-cutting helpers (paths, logging)

No GUI code lives here; PySide6 is a Phase-2 concern and must never be
imported from within ``src``.
"""
