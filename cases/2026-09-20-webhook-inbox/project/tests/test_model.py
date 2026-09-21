from __future__ import annotations

import unittest

from webhookinbox.model import ValidationError, canonical_payload, validate_delivery


class ModelTests(unittest.TestCase):
    def test_validation_trims_text_and_keeps_payload(self) -> None:
        delivery = validate_delivery(
            {
                "delivery_id": " del-1 ",
                "event_type": " release.published ",
                "payload": {"version": "0.4.0"},
            }
        )
        self.assertEqual(delivery, ("del-1", "release.published", {"version": "0.4.0"}))

    def test_validation_rejects_unknown_fields_and_bad_ids(self) -> None:
        with self.assertRaises(ValidationError):
            validate_delivery(
                {
                    "delivery_id": "del/1",
                    "event_type": "release",
                    "payload": {},
                }
            )
        with self.assertRaises(ValidationError):
            validate_delivery(
                {
                    "delivery_id": "del-1",
                    "event_type": "release",
                    "payload": {},
                    "extra": True,
                }
            )

    def test_payload_encoding_is_deterministic(self) -> None:
        self.assertEqual(
            canonical_payload({"z": 1, "a": [True, "x"]}),
            '{"a":[true,"x"],"z":1}',
        )


if __name__ == "__main__":
    unittest.main()
