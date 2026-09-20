"""leasecascade: a dependency-free fan-out/fan-in job cascade service.

The package exposes a small HTTP admission service backed by SQLite and a
bounded ``--once`` worker that delivers runnable jobs to a local sink program.
"""

__all__ = ["__version__"]

__version__ = "0.1.0"
