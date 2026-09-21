import unittest

from outbox.model import ValidationError, normalize_event


class ModelTests(unittest.TestCase):
    def test_normalizes_strings_and_preserves_payload(self) -> None:
        event = normalize_event(
            {"event_id": "  evt-1 ", "topic": " release ", "payload": {"n": 1}}
        )
        self.assertEqual(event, {"event_id": "evt-1", "topic": "release", "payload": {"n": 1}})

    def test_rejects_unknown_fields_and_bad_ids(self) -> None:
        for document in (
            {"event_id": "bad id", "topic": "x", "payload": {}},
            {"event_id": "evt", "topic": "x", "payload": {}, "extra": True},
            {"event_id": "evt", "topic": "x", "payload": []},
        ):
            with self.assertRaises(ValidationError):
                normalize_event(document)
