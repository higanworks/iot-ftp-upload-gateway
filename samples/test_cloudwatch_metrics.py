"""Tests for cloudwatch_metrics.py. No AWS access or running gateway needed:

    python3 -m unittest samples/test_cloudwatch_metrics.py

(`boto3` and `requests` are only needed by the script's `main`; they are stubbed out here if they
are not installed, so these tests run on a bare Python.)
"""

import os
import sys
import types
import unittest
from unittest import mock

for _module in ("boto3", "requests"):
    try:
        __import__(_module)
    except ImportError:
        sys.modules[_module] = types.ModuleType(_module)

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import cloudwatch_metrics as cw  # noqa: E402

TESTDATA = os.path.join(os.path.dirname(os.path.abspath(__file__)), "testdata")


def by_name(samples):
    return {s.name: s for s in samples}


class ParseTests(unittest.TestCase):
    def test_parses_a_plain_series(self):
        samples = cw.parse_prometheus_text("ftp_gateway_sessions_active 3\n")
        self.assertEqual(samples, [cw.Sample("ftp_gateway_sessions_active", {}, 3.0)])

    def test_parses_a_labeled_series(self):
        line = (
            'ftp_gateway_backend_source_transfers_active'
            '{backend="s-1.server.transfer.example:21",source="10.0.1.10"} 4\n'
        )
        (sample,) = cw.parse_prometheus_text(line)
        self.assertEqual(sample.name, "ftp_gateway_backend_source_transfers_active")
        self.assertEqual(
            sample.labels,
            {"backend": "s-1.server.transfer.example:21", "source": "10.0.1.10"},
        )
        self.assertEqual(sample.value, 4.0)

    def test_label_values_may_contain_escapes_braces_commas_and_spaces(self):
        line = r'm{a="x\"y",b="c\\d",c="line\nbreak",d="}, {",e="has space"} 7' + "\n"
        (sample,) = cw.parse_prometheus_text(line)
        self.assertEqual(
            sample.labels,
            {"a": 'x"y', "b": "c\\d", "c": "line\nbreak", "d": "}, {", "e": "has space"},
        )
        self.assertEqual(sample.value, 7.0)

    def test_ignores_comments_blank_lines_and_a_trailing_timestamp(self):
        text = "# HELP m help\n# TYPE m gauge\n\nm 5 1700000000000\n"
        self.assertEqual(cw.parse_prometheus_text(text), [cw.Sample("m", {}, 5.0)])

    def test_skips_lines_it_cannot_use_instead_of_publishing_garbage(self):
        text = "\n".join(
            [
                "m_nan NaN",
                "m_inf +Inf",
                "m_text hello",
                "m_one_token",
                'm_unterminated{a="x 1',
                'm_no_value{a="x"}',
                'm_bad_escape{a="x\\qy"} 1',
                'm_empty_label_name{="x"} 1',
                "good 1",
            ]
        )
        self.assertEqual(cw.parse_prometheus_text(text), [cw.Sample("good", {}, 1.0)])


class RealGatewayOutputTests(unittest.TestCase):
    """The fixture is a real `/metrics` scrape from a v2026.10.1 gateway with source rotation on."""

    def setUp(self):
        with open(os.path.join(TESTDATA, "gateway_metrics_rotation_on.txt")) as f:
            self.samples = cw.parse_prometheus_text(f.read())

    def test_every_series_parses(self):
        # 15 plain series plus the per-source `unhealthy` gauge.
        self.assertEqual(len(self.samples), 16)

    def test_every_gateway_metric_has_a_unit(self):
        unitless = [s.name for s in self.samples if s.name not in cw.METRIC_UNITS]
        self.assertEqual(unitless, [])

    def test_the_labeled_series_becomes_a_metric_with_a_source_dimension(self):
        data = cw.to_metric_data(self.samples, [])
        (unhealthy,) = [d for d in data if d["MetricName"].endswith("source_unhealthy")]
        self.assertEqual(unhealthy["MetricName"], "ftp_gateway_backend_source_unhealthy")
        self.assertEqual(unhealthy["Dimensions"], [{"Name": "source", "Value": "127.0.0.1"}])
        self.assertEqual(unhealthy["Unit"], "Count")

    def test_no_metric_name_carries_label_syntax(self):
        for datum in cw.to_metric_data(self.samples, []):
            self.assertNotRegex(datum["MetricName"], r"[{}\"=]")


class MetricDataTests(unittest.TestCase):
    def test_bytes_keep_their_unit_and_unknown_metrics_are_still_published(self):
        data = cw.to_metric_data(
            [
                cw.Sample("ftp_gateway_upload_bytes_total", {}, 42.0),
                cw.Sample("ftp_gateway_future_metric", {}, 1.0),
            ],
            [],
        )
        self.assertEqual([d["Unit"] for d in data], ["Bytes", "None"])
        self.assertNotIn("Dimensions", data[0])

    def test_labels_become_dimensions_after_the_command_line_ones(self):
        data = cw.to_metric_data(
            [cw.Sample("m", {"source": "10.0.0.1", "backend": "b:21"}, 1.0)],
            [{"Name": "InstanceId", "Value": "i-123"}],
        )
        self.assertEqual(
            data[0]["Dimensions"],
            [
                {"Name": "InstanceId", "Value": "i-123"},
                {"Name": "backend", "Value": "b:21"},
                {"Name": "source", "Value": "10.0.0.1"},
            ],
        )

    def test_a_label_replaces_a_command_line_dimension_of_the_same_name(self):
        data = cw.to_metric_data(
            [cw.Sample("m", {"source": "10.0.0.1"}, 1.0)],
            [{"Name": "source", "Value": "from-cli"}],
        )
        self.assertEqual(data[0]["Dimensions"], [{"Name": "source", "Value": "10.0.0.1"}])

    def test_an_empty_label_value_is_dropped(self):
        data = cw.to_metric_data([cw.Sample("m", {"source": ""}, 1.0)], [])
        self.assertNotIn("Dimensions", data[0])


class MainTests(unittest.TestCase):
    def run_main(self, samples):
        client = mock.Mock()
        with mock.patch.object(cw, "fetch_metrics", return_value=samples), mock.patch.object(
            cw.boto3, "client", create=True, return_value=client
        ):
            code = cw.main(["--dimension", "InstanceId=i-1"])
        return code, client

    def test_publishes_in_batches_of_at_most_1000(self):
        samples = [cw.Sample("ftp_gateway_sessions_active", {"n": str(i)}, 1.0) for i in range(2500)]
        code, client = self.run_main(samples)
        self.assertEqual(code, 0)
        sizes = [len(call.kwargs["MetricData"]) for call in client.put_metric_data.call_args_list]
        self.assertEqual(sizes, [1000, 1000, 500])

    def test_a_small_scrape_is_one_call(self):
        code, client = self.run_main([cw.Sample("ftp_gateway_sessions_active", {}, 1.0)])
        self.assertEqual(code, 0)
        self.assertEqual(client.put_metric_data.call_count, 1)

    def test_a_publish_failure_is_a_nonzero_exit(self):
        client = mock.Mock()
        client.put_metric_data.side_effect = RuntimeError("denied")
        with mock.patch.object(
            cw, "fetch_metrics", return_value=[cw.Sample("m", {}, 1.0)]
        ), mock.patch.object(cw.boto3, "client", create=True, return_value=client):
            self.assertEqual(cw.main([]), 1)


if __name__ == "__main__":
    unittest.main()
