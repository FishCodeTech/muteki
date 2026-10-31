import unittest
from apps.web.run_state import _safe_exception_detail


class RuntimeErrorRedactionTest(unittest.TestCase):
    def test_diagnostic_masks_credentials_before_persistence(self):
        text = _safe_exception_detail('worker failed', RuntimeError(
            'Authorization: Bearer fictional-secret-token '
            'https://fake.invalid/api?api_key=fictional-query-key '
            'password=fictional-password'))
        for secret in ('fictional-secret-token', 'fictional-query-key', 'fictional-password'):
            self.assertNotIn(secret, text)
        self.assertIn('RuntimeError', text)

    def test_useful_provider_diagnostic_is_preserved(self):
        text = _safe_exception_detail('worker failed', RuntimeError('Not logged in · Please run /login'))
        self.assertIn('Not logged in', text)
        self.assertIn('/login', text)
