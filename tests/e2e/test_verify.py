"""Regression tests for readiness polling, without Docker or network access."""

from http.client import RemoteDisconnected
import unittest
from unittest.mock import Mock, patch
from urllib.error import URLError

from verify import wait_for


class ReadinessTests(unittest.TestCase):
    @patch("verify.time.sleep")
    def test_retries_connections_accepted_before_application_is_ready(self, sleep):
        check = Mock(side_effect=[
            ConnectionResetError("container port is open, app is starting"),
            RemoteDisconnected("app closed connection"),
            URLError("connection refused"),
            TimeoutError("request timeout"),
            {"data": []},
        ])
        self.assertEqual(wait_for("startup", check), {"data": []})
        self.assertEqual(sleep.call_count, 4)

    @patch("verify.time.sleep")
    @patch("verify.time.monotonic", side_effect=[0, 0, 181])
    def test_permanent_failure_exits_at_deadline(self, monotonic, sleep):
        check = Mock(side_effect=ConnectionResetError("still starting"))
        with self.assertRaisesRegex(AssertionError, "Timed out waiting for startup"):
            wait_for("startup", check)
        self.assertEqual(check.call_count, 1)


if __name__ == "__main__":
    unittest.main()
