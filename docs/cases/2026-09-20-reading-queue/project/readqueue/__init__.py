"""Read Queue: a dependency-free HTTP JSON service for a small reading queue.

The package is split into small layers so each concern can be tested on its
own:

``readqueue.store``
    SQLite persistence for items (single source of truth for ordering and
    uniqueness rules).
``readqueue.validation``
    Pure request-body validation and tag normalisation.
``readqueue.api``
    Route dispatch that turns (method, path, query, body) into a response
    object; no sockets involved, so it is unit-testable in isolation.
``readqueue.server``
    ``http.server`` wiring for the dispatch layer.
``readqueue.cli``
    ``python -m readqueue`` entry point and argument parsing.
"""

__all__ = ["__version__"]

__version__ = "0.1.0"
