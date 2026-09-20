"""Artifact Pipeline: a dependency-free HTTP intake service and local worker.

The package is intentionally split into small modules so each layer of the
data flow can be reasoned about on its own:

``artifactpipe.ids``
    Identifier and structural validation shared by the HTTP and worker sides.
``artifactpipe.storage``
    The only module that talks to SQLite; owns the schema and every state
    transition that has to be atomic.
``artifactpipe.service``
    The HTTP server: signature verification, request validation, and intake.
``artifactpipe.worker``
    Claims runnable jobs from SQLite, resolves declared output references,
    invokes the sink program, and finalizes job state.
``artifactpipe.__main__``
    Command line interface described in ``README.md``.

Only the Python standard library is used anywhere in the package.
"""

__all__ = ["__version__"]

__version__ = "0.1.0"
