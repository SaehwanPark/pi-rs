"""receiptledger: dependency-free signed admission, leased delivery, receipts.

The package intentionally exposes only standard-library functionality. The
public surface is the CLI (`python -m receiptledger ...`); internal modules keep
their helpers module-private unless another module imports them deliberately.
"""

__all__ = ["__version__"]

__version__ = "0.1.0"
