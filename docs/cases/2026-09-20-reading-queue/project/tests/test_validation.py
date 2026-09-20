"""Unit tests for :mod:`readqueue.validation` (field rules only)."""

from __future__ import annotations

import unittest

from support import StoreTestCase  # noqa: F401 - keeps sys.path wiring in one place

from readqueue.validation import (
    normalise_tags,
    parse_id,
    parse_status_filter,
    validate_create,
    validate_patch,
    ValidationError,
)


def expect_error(test: unittest.TestCase, call, message: str) -> None:
    with test.assertRaises(ValidationError) as caught:
        call()
    test.assertIn(message, str(caught.exception))


class ValidateCreateTests(unittest.TestCase):
    def test_minimal_payload_gets_empty_tags(self) -> None:
        self.assertEqual(
            validate_create({"title": "T", "url": "https://example.test"}),
            {"title": "T", "url": "https://example.test", "tags": []},
        )

    def test_tags_are_trimmed_lowercased_unique_and_sorted(self) -> None:
        values = validate_create(
            {"title": "T", "url": "u", "tags": [" Beta ", "alpha", "BETA", "alpha", " gamma"]}
        )
        self.assertEqual(values["tags"], ["alpha", "beta", "gamma"])

    def test_title_and_url_are_required(self) -> None:
        expect_error(self, lambda: validate_create({"title": "T"}), "missing required field: url")
        expect_error(self, lambda: validate_create({"url": "u"}), "missing required field: title")
        expect_error(self, lambda: validate_create({}), "missing required field")

    def test_empty_or_whitespace_text_is_rejected(self) -> None:
        expect_error(
            self,
            lambda: validate_create({"title": "", "url": "u"}),
            "title must be a non-empty string",
        )
        expect_error(
            self,
            lambda: validate_create({"title": "   ", "url": "u"}),
            "title must be a non-empty string",
        )
        expect_error(
            self,
            lambda: validate_create({"title": "T", "url": "  "}),
            "url must be a non-empty string",
        )

    def test_wrong_field_types_are_rejected(self) -> None:
        expect_error(
            self, lambda: validate_create({"title": 1, "url": "u"}), "title must be a string"
        )
        expect_error(
            self, lambda: validate_create({"title": None, "url": "u"}), "title must be a string"
        )
        expect_error(self, lambda: validate_create({"title": "T", "url": []}), "url must be a string")
        expect_error(
            self, lambda: validate_create({"title": "T", "url": "u", "tags": "a"}), "must be a list"
        )
        expect_error(
            self,
            lambda: validate_create({"title": "T", "url": "u", "tags": ["a", 3]}),
            "must contain only strings",
        )
        expect_error(
            self,
            lambda: validate_create({"title": "T", "url": "u", "tags": ["  "]}),
            "must contain only non-empty strings",
        )

    def test_unknown_fields_are_rejected(self) -> None:
        expect_error(
            self,
            lambda: validate_create({"title": "T", "url": "u", "status": "done"}),
            "unknown field(s): status",
        )
        expect_error(
            self,
            lambda: validate_create({"title": "T", "url": "u", "extra": 1, "also": 2}),
            "unknown field(s): also, extra",
        )

    def test_non_object_documents_are_rejected(self) -> None:
        for payload in ([], "text", 3, None, True):
            expect_error(self, lambda value=payload: validate_create(value), "must be a JSON object")


class ValidatePatchTests(unittest.TestCase):
    def test_each_field_is_normalised(self) -> None:
        self.assertEqual(
            validate_patch({"status": "reading", "tags": ["B", "a"]}),
            {"status": "reading", "tags": ["a", "b"]},
        )
        self.assertEqual(validate_patch({"title": " New "}), {"title": " New "})

    def test_empty_object_is_rejected(self) -> None:
        expect_error(self, lambda: validate_patch({}), "at least one field")

    def test_status_vocabulary_is_enforced(self) -> None:
        for status in ("queued", "reading", "done"):
            self.assertEqual(validate_patch({"status": status})["status"], status)
        for bad in ("QUEUED", "started", "", 2, None):
            expect_error(self, lambda value=bad: validate_patch({"status": value}), "status must be")

    def test_unknown_field_is_rejected(self) -> None:
        expect_error(self, lambda: validate_patch({"done": True}), "unknown field(s): done")

    def test_same_text_rules_apply(self) -> None:
        expect_error(self, lambda: validate_patch({"url": ""}), "url must be a non-empty string")
        expect_error(self, lambda: validate_patch({"tags": {"a": 1}}), "must be a list")


class ParserTests(unittest.TestCase):
    def test_parse_id_accepts_positive_integers_only(self) -> None:
        self.assertEqual(parse_id("7"), 7)
        self.assertEqual(parse_id("42"), 42)
        for bad in ("", "0", "-1", "1.5", "abc", " 1", "1 ", "١٢", "9999999999999999999999"):
            with self.assertRaises(ValidationError, msg=bad):
                parse_id(bad)

    def test_parse_status_filter(self) -> None:
        self.assertEqual(parse_status_filter("done"), "done")
        with self.assertRaises(ValidationError):
            parse_status_filter("paused")

    def test_normalise_tags_output_is_stable(self) -> None:
        first = normalise_tags(["x", "Y", "x", " Z "])
        second = normalise_tags(["Z", "x", "y"])
        self.assertEqual(first, second)


if __name__ == "__main__":  # pragma: no cover
    unittest.main()
