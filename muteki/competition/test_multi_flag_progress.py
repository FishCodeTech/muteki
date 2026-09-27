"""Multi-flag local progress: digest counting and solved transitions."""
from __future__ import annotations

import unittest

from muteki.competition.models import (
    ChallengeState,
    IllegalTransitionError,
    ensure_challenge_transition,
)


class MultiFlagProgressTests(unittest.TestCase):
    def test_running_can_become_solved(self) -> None:
        ensure_challenge_transition(
            ChallengeState.RUNNING, ChallengeState.SOLVED)

    def test_candidate_found_can_become_solved(self) -> None:
        ensure_challenge_transition(
            ChallengeState.CANDIDATE_FOUND, ChallengeState.SOLVED)

    def test_submitting_can_still_become_solved(self) -> None:
        ensure_challenge_transition(
            ChallengeState.SUBMITTING, ChallengeState.SOLVED)

    def test_discovered_cannot_become_solved(self) -> None:
        with self.assertRaises(IllegalTransitionError):
            ensure_challenge_transition(
                ChallengeState.DISCOVERED, ChallengeState.SOLVED)

    def test_correct_digest_count_ignores_shared_slot(self) -> None:
        submissions = [
            {"state": "correct", "answer_slot": 1, "digest": "a"},
            {"state": "correct", "answer_slot": 1, "digest": "b"},
            {"state": "correct", "answer_slot": 1, "digest": "c"},
            {"state": "correct", "answer_slot": 1, "digest": "d"},
            {"state": "wrong", "answer_slot": 1, "digest": "e"},
        ]
        confirmed = {
            item["digest"]
            for item in submissions
            if item["state"] == "correct"
        }
        slots = {
            item["answer_slot"]
            for item in submissions
            if item["state"] == "correct"
        }
        self.assertEqual(len(confirmed), 4)
        self.assertEqual(len(slots), 1)


if __name__ == "__main__":
    unittest.main()
