"""Exception types shared by the store and HTTP layers."""

from __future__ import annotations


class ReadQueueError(Exception):
    """Base class for expected, user-facing failures."""


class UnknownItem(ReadQueueError):
    """Raised when an item id does not exist in the store."""

    def __init__(self, item_id: int) -> None:
        super().__init__(f"no item with id {item_id}")
        self.item_id = item_id


class DuplicateURL(ReadQueueError):
    """Raised when an item URL collides with an existing item."""

    def __init__(self, url: str) -> None:
        super().__init__(f"URL already in the queue: {url}")
        self.url = url
