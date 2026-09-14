#!/usr/bin/env python3
"""Scrapes iot-ftp-upload-gateway's `/metrics` endpoint (see README.md's "Metrics" section)
and publishes the values to CloudWatch as custom metrics.

One-shot by design: run it periodically from cron or a systemd timer rather than as a
long-running daemon (see samples/README.md for a timer unit example).

Requires `cloudwatch:PutMetricData` IAM permission for whatever credentials boto3 resolves
(instance profile, environment variables, ~/.aws/credentials, ...).
"""

from __future__ import annotations

import argparse
import sys
from typing import Sequence

import boto3
import requests

# Maps a metric name exposed by the gateway to the CloudWatch Unit that best describes it.
# Anything scraped that isn't listed here is still published, with Unit "None" -- keeps this
# script forward-compatible with metrics the gateway might add later without needing an update.
METRIC_UNITS = {
    "ftp_gateway_sessions_active": "Count",
    "ftp_gateway_uploads_active": "Count",
    "ftp_gateway_pasv_ports_active": "Count",
    "ftp_gateway_sessions_total": "Count",
    "ftp_gateway_upload_bytes_total": "Bytes",
    "ftp_gateway_connections_rejected_total": "Count",
}


def parse_prometheus_text(text: str) -> dict[str, float]:
    """Parses the small subset of the Prometheus text exposition format this gateway emits:
    one `metric_name value` pair per line, with `#` comment lines (HELP/TYPE) ignored. Not a
    general-purpose Prometheus parser -- none of this gateway's metrics carry labels, so
    label syntax isn't handled.
    """
    metrics: dict[str, float] = {}
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if len(parts) != 2:
            continue
        name, raw_value = parts
        try:
            metrics[name] = float(raw_value)
        except ValueError:
            continue
    return metrics


def parse_dimension(raw: str) -> dict[str, str]:
    if "=" not in raw:
        raise argparse.ArgumentTypeError(f"dimension must be NAME=VALUE, got: {raw!r}")
    name, value = raw.split("=", 1)
    if not name or not value:
        raise argparse.ArgumentTypeError(f"dimension must be NAME=VALUE, got: {raw!r}")
    return {"Name": name, "Value": value}


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--metrics-url",
        default="http://127.0.0.1:9273/metrics",
        help="URL of the gateway's /metrics endpoint (default: %(default)s)",
    )
    parser.add_argument(
        "--namespace",
        default="IoTFtpUploadGateway",
        help="CloudWatch namespace to publish under (default: %(default)s)",
    )
    parser.add_argument(
        "--dimension",
        action="append",
        type=parse_dimension,
        default=[],
        metavar="NAME=VALUE",
        help="Dimension to attach to every published metric, e.g. "
        "InstanceId=i-0123456789abcdef0. May be given multiple times. Without at least one, "
        "data points from every gateway instance publishing to the same namespace are "
        "aggregated together by CloudWatch.",
    )
    parser.add_argument(
        "--region",
        default=None,
        help="AWS region. Defaults to boto3's normal resolution (environment variable, "
        "config file, or instance metadata).",
    )
    parser.add_argument(
        "--timeout",
        type=float,
        default=5.0,
        help="HTTP timeout in seconds for the /metrics request (default: %(default)s)",
    )
    return parser


def fetch_metrics(url: str, timeout: float) -> dict[str, float]:
    response = requests.get(url, timeout=timeout)
    response.raise_for_status()
    return parse_prometheus_text(response.text)


def to_metric_data(
    metrics: dict[str, float], dimensions: list[dict[str, str]]
) -> list[dict[str, object]]:
    data = []
    for name, value in metrics.items():
        datum: dict[str, object] = {
            "MetricName": name,
            "Value": value,
            "Unit": METRIC_UNITS.get(name, "None"),
        }
        if dimensions:
            datum["Dimensions"] = dimensions
        data.append(datum)
    return data


def main(argv: Sequence[str] | None = None) -> int:
    args = build_arg_parser().parse_args(argv)

    try:
        metrics = fetch_metrics(args.metrics_url, args.timeout)
    except requests.RequestException as err:
        print(f"error: failed to fetch {args.metrics_url}: {err}", file=sys.stderr)
        return 1

    if not metrics:
        print(f"error: no metrics parsed from {args.metrics_url}", file=sys.stderr)
        return 1

    metric_data = to_metric_data(metrics, args.dimension)

    cloudwatch = boto3.client("cloudwatch", region_name=args.region)
    try:
        cloudwatch.put_metric_data(Namespace=args.namespace, MetricData=metric_data)
    except Exception as err:  # botocore raises various ClientError subclasses
        print(f"error: failed to publish to CloudWatch: {err}", file=sys.stderr)
        return 1

    print(f"published {len(metric_data)} metric(s) to CloudWatch namespace {args.namespace!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
