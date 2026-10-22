#!/usr/bin/env python3
"""Validate the offline labeled evaluator dataset and score supplied predictions."""

from __future__ import annotations

import argparse
import json
import sys
from collections import Counter
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DATASET = ROOT / "evaluation" / "benchmark" / "v1.jsonl"
CLASSIFICATIONS = (
    "sustained",
    "contradicted",
    "insufficient_context",
    "preference_tradeoff",
    "already_fixed",
)
SOURCE_KINDS = {"synthetic_fixture", "project_fixture", "real_labeled"}


def read_jsonl(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    with path.open(encoding="utf-8") as dataset_file:
        for line_number, line in enumerate(dataset_file, start=1):
            if not line.strip():
                continue
            try:
                row = json.loads(line)
            except json.JSONDecodeError as error:
                raise ValueError(f"{path}:{line_number}: invalid JSON: {error.msg}") from error
            if not isinstance(row, dict):
                raise ValueError(f"{path}:{line_number}: each JSONL row must be an object")
            rows.append(row)
    return rows


def validate_dataset(rows: list[dict[str, Any]]) -> None:
    if not rows:
        raise ValueError("dataset must contain at least one labeled case")
    seen_ids: set[str] = set()
    required_strings = (
        "comment_body",
        "source_at_comment",
        "current_diff",
        "label_rationale",
    )
    for index, row in enumerate(rows, start=1):
        case_id = row.get("id")
        if not isinstance(case_id, str) or not case_id.strip():
            raise ValueError(f"row {index}: id must be a non-empty string")
        if case_id in seen_ids:
            raise ValueError(f"row {index}: duplicate case id {case_id!r}")
        seen_ids.add(case_id)
        if row.get("source_kind") not in SOURCE_KINDS:
            raise ValueError(f"row {index}: unsupported source_kind")
        if row.get("expected_classification") not in CLASSIFICATIONS:
            raise ValueError(f"row {index}: unsupported expected_classification")
        for field in required_strings:
            if not isinstance(row.get(field), str) or not row[field].strip():
                raise ValueError(f"row {index}: {field} must be a non-empty string")
        thread_context = row.get("thread_context")
        if not isinstance(thread_context, list) or any(
            not isinstance(item, str) for item in thread_context
        ):
            raise ValueError(f"row {index}: thread_context must be a list of strings")


def dataset_summary(rows: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "dataset": "cururu-desktop-evaluator-benchmark-v1",
        "case_count": len(rows),
        "by_source_kind": dict(sorted(Counter(row["source_kind"] for row in rows).items())),
        "by_expected_classification": dict(
            sorted(Counter(row["expected_classification"] for row in rows).items())
        ),
        "predictions_scored": False,
    }


def load_predictions(path: Path, expected_ids: set[str]) -> dict[str, str]:
    rows = read_jsonl(path)
    predictions: dict[str, str] = {}
    for index, row in enumerate(rows, start=1):
        case_id = row.get("id")
        classification = row.get("classification")
        if not isinstance(case_id, str) or not case_id:
            raise ValueError(f"prediction row {index}: id must be a non-empty string")
        if case_id in predictions:
            raise ValueError(f"prediction row {index}: duplicate id {case_id!r}")
        if classification not in CLASSIFICATIONS:
            raise ValueError(f"prediction row {index}: unsupported classification")
        predictions[case_id] = classification

    missing = expected_ids - predictions.keys()
    extra = predictions.keys() - expected_ids
    if missing or extra:
        raise ValueError(
            f"predictions must cover the dataset exactly; missing={sorted(missing)}, "
            f"extra={sorted(extra)}"
        )
    return predictions


def score_predictions(
    rows: list[dict[str, Any]], predictions: dict[str, str]
) -> dict[str, Any]:
    matrix = {expected: Counter() for expected in CLASSIFICATIONS}
    for row in rows:
        expected = row["expected_classification"]
        predicted = predictions[row["id"]]
        matrix[expected][predicted] += 1

    per_class: dict[str, dict[str, float | int]] = {}
    f1_values: list[float] = []
    correct = 0
    for label in CLASSIFICATIONS:
        true_positive = matrix[label][label]
        false_positive = sum(matrix[other][label] for other in CLASSIFICATIONS if other != label)
        false_negative = sum(matrix[label][other] for other in CLASSIFICATIONS if other != label)
        precision_denominator = true_positive + false_positive
        recall_denominator = true_positive + false_negative
        precision = true_positive / precision_denominator if precision_denominator else 0.0
        recall = true_positive / recall_denominator if recall_denominator else 0.0
        f1 = 2 * precision * recall / (precision + recall) if precision + recall else 0.0
        per_class[label] = {
            "support": recall_denominator,
            "precision": precision,
            "recall": recall,
            "f1": f1,
        }
        f1_values.append(f1)
        correct += true_positive

    return {
        "accuracy": correct / len(rows),
        "macro_f1": sum(f1_values) / len(f1_values),
        "per_class": per_class,
        "confusion_matrix": {
            expected: {predicted: matrix[expected][predicted] for predicted in CLASSIFICATIONS}
            for expected in CLASSIFICATIONS
        },
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dataset", type=Path, default=DEFAULT_DATASET)
    parser.add_argument("--predictions", type=Path)
    args = parser.parse_args()

    try:
        rows = read_jsonl(args.dataset)
        validate_dataset(rows)
        report = dataset_summary(rows)
        if args.predictions:
            predictions = load_predictions(args.predictions, {row["id"] for row in rows})
            report["predictions_scored"] = True
            report["metrics"] = score_predictions(rows, predictions)
    except (OSError, ValueError) as error:
        print(f"benchmark validation failed: {error}", file=sys.stderr)
        return 2

    print(json.dumps(report, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
