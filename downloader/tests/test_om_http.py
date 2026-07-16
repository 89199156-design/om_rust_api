import unittest
from pathlib import Path


class OmHttpTests(unittest.TestCase):
    def test_nginx_client_api_exposes_range_bundles_without_status_or_latest(self):
        conf = Path("nginx/om_client_api.conf").read_text(encoding="utf-8")

        self.assertNotIn("/api/om/status", conf)
        self.assertNotIn("/data/om_http/status.json", conf)
        self.assertNotIn("/current/", conf)
        self.assertNotIn("ready_for_processing", conf)
        self.assertNotIn("(latest|ready_for_processing)", conf)
        self.assertNotIn("current/latest.json", conf)
        self.assertIn("location ~ ^/data/om/", conf)
        self.assertIn("/data/om_raw/", conf)
        self.assertIn(".omranges", conf)
        self.assertIn("location ~ ^/data/webp/", conf)
        self.assertIn("image/webp", conf)
        self.assertIn("application/json", conf)
        self.assertIn("application/octet-stream", conf)


if __name__ == "__main__":
    unittest.main()
