import json
import tempfile
import unittest
from pathlib import Path

import evaluator_benchmark


class EvaluatorBenchmarkTests(unittest.TestCase):
    def test_version_one_dataset_is_balanced_and_synthetic_only(self) -> None:
        rows = evaluator_benchmark.read_jsonl(evaluator_benchmark.DEFAULT_DATASET)
        evaluator_benchmark.validate_dataset(rows)
        summary = evaluator_benchmark.dataset_summary(rows)

        self.assertEqual(summary["case_count"], 10)
        self.assertEqual(summary["by_source_kind"], {"synthetic_fixture": 10})
        self.assertEqual(
            summary["by_expected_classification"],
            {
                "already_fixed": 2,
                "contradicted": 2,
                "insufficient_context": 2,
                "preference_tradeoff": 2,
                "sustained": 2,
            },
        )
        self.assertFalse(summary["predictions_scored"])

    def test_scoring_reports_accuracy_macro_f1_and_confusion_matrix(self) -> None:
        rows = evaluator_benchmark.read_jsonl(evaluator_benchmark.DEFAULT_DATASET)
        predictions = {
            row["id"]: row["expected_classification"]
            for row in rows
        }

        result = evaluator_benchmark.score_predictions(rows, predictions)

        self.assertEqual(result["accuracy"], 1.0)
        self.assertEqual(result["macro_f1"], 1.0)
        for label in evaluator_benchmark.CLASSIFICATIONS:
            self.assertEqual(result["per_class"][label]["support"], 2)
            self.assertEqual(result["confusion_matrix"][label][label], 2)

    def test_predictions_must_cover_each_case_once(self) -> None:
        rows = evaluator_benchmark.read_jsonl(evaluator_benchmark.DEFAULT_DATASET)
        expected_ids = {row["id"] for row in rows}
        first = rows[0]
        prediction = {
            "id": first["id"],
            "classification": first["expected_classification"],
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "predictions.jsonl"
            path.write_text(json.dumps(prediction) + "\n", encoding="utf-8")

            with self.assertRaisesRegex(ValueError, "cover the dataset exactly"):
                evaluator_benchmark.load_predictions(path, expected_ids)


if __name__ == "__main__":
    unittest.main()
