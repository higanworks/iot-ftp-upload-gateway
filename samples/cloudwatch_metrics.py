#!/usr/bin/env python3
"""Scrapes iot-ftp-upload-gateway's `/metrics` endpoint (see README.md's "Metrics" section)
and publishes the values to CloudWatch as custom metrics.

One-shot by design: run it periodically from cron or a systemd timer rather than as a
long-running daemon (see samples/README.md for a timer unit example).

Labels on a scraped series (the per-source-address metrics shown when the gateway's source
rotation is on carry `backend` and `source`) become CloudWatch dimensions on that metric, next to
any `--dimension` given on the command line.

Requires `cloudwatch:PutMetricData` IAM permission for whatever credentials boto3 resolves
(instance profile, environment variables, ~/.aws/credentials, ...).
"""

from __future__ import annotations

import argparse
import math
import sys
from typing import NamedTuple, Sequence

import boto3
import requests

# Maps a metric name exposed by the gateway to the CloudWatch Unit that best describes it.
# Anything scraped that isn't listed here is still published, with Unit "None" -- keeps this
# script forward-compatible with metrics the gateway might add later without needing an update.
METRIC_UNITS = {
    "ftp_gateway_sessions_active": "Count",
    "ftp_gateway_uploads_active": "Count",
    "ftp_gateway_pasv_ports_active": "Count",
    "ftp_gateway_pasv_ports_capacity": "Count",
    "ftp_gateway_sessions_total": "Count",
    "ftp_gateway_upload_bytes_total": "Bytes",
    "ftp_gateway_connections_rejected_total": "Count",
    "ftp_gateway_uploads_started_total": "Count",
    "ftp_gateway_uploads_completed_total": "Count",
    "ftp_gateway_uploads_failed_total": "Count",
    "ftp_gateway_backend_pasv_failures_total": "Count",
    "ftp_gateway_backend_data_connection_failures_total": "Count",
    "ftp_gateway_data_connections_rejected_total": "Count",
    "ftp_gateway_backend_timeouts_total": "Count",
    "ftp_gateway_session_idle_timeouts_total": "Count",
    # Only present when the gateway's backend source rotation is on; labeled by source (and,
    # except for `unhealthy`, by backend).
    "ftp_gateway_backend_source_unhealthy": "Count",
    "ftp_gateway_backend_source_transfers_active": "Count",
    "ftp_gateway_backend_source_transfers_capacity": "Count",
    "ftp_gateway_backend_source_transfers_total": "Count",
    "ftp_gateway_backend_source_slot_timeouts_total": "Count",
}

# CloudWatch accepts at most this many data points in one PutMetricData call.
MAX_METRICS_PER_CALL = 1000


class Sample(NamedTuple):
    """One scraped series: a metric name, its labels (empty for most of the gateway's
    metrics), and its value."""

    name: str
    labels: dict[str, str]
    value: float


def parse_prometheus_text(text: str) -> list[Sample]:
    """Parses the subset of the Prometheus text exposition format this gateway emits: lines of
    `name value` or `name{label="value",...} value`, with `#` comment lines (HELP/TYPE) ignored.
    Label values may contain the three escapes the format defines (backslash-backslash,
    backslash-quote, backslash-n). Not a general-purpose Prometheus
    parser (no histograms, no exemplars), but lines it does not understand are skipped rather than
    sent to CloudWatch -- PutMetricData rejects the whole batch if one data point is invalid.
    Values that are not finite (NaN, +Inf) are skipped for the same reason.
    """
    samples: list[Sample] = []
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        sample = parse_sample_line(line)
        if sample is not None:
            samples.append(sample)
    return samples


def parse_sample_line(line: str) -> Sample | None:
    brace = line.find("{")
    first_space = line.find(" ")
    if brace == -1 or (first_space != -1 and first_space < brace):
        # No labels: `name value [timestamp]`.
        parts = line.split()
        if len(parts) not in (2, 3):
            return None
        name, labels, raw_value = parts[0], {}, parts[1]
    else:
        name = line[:brace]
        parsed = parse_labels(line, brace + 1)
        if parsed is None:
            return None
        labels, end = parsed
        parts = line[end:].split()
        if len(parts) not in (1, 2):
            return None
        raw_value = parts[0]
    try:
        value = float(raw_value)
    except ValueError:
        return None
    if not math.isfinite(value) or not name:
        return None
    return Sample(name, labels, value)


def parse_labels(line: str, i: int) -> tuple[dict[str, str], int] | None:
    """Parses `name="value",name="value"}` starting at `line[i]`. Returns the labels and the
    index just past the closing brace, or None if the text is malformed."""
    labels: dict[str, str] = {}
    escapes = {"\\": "\\", '"': '"', "n": "\n"}
    while True:
        while i < len(line) and line[i] == " ":
            i += 1
        if i >= len(line):
            return None
        if line[i] == "}":
            return labels, i + 1
        eq = line.find("=", i)
        if eq == -1 or eq + 1 >= len(line) or line[eq + 1] != '"':
            return None
        label_name = line[i:eq].strip()
        i = eq + 2
        value: list[str] = []
        while True:
            if i >= len(line):
                return None
            ch = line[i]
            if ch == "\\":
                if i + 1 >= len(line) or line[i + 1] not in escapes:
                    return None
                value.append(escapes[line[i + 1]])
                i += 2
            elif ch == '"':
                i += 1
                break
            else:
                value.append(ch)
                i += 1
        if not label_name:
            return None
        labels[label_name] = "".join(value)
        while i < len(line) and line[i] == " ":
            i += 1
        if i < len(line) and line[i] == ",":
            i += 1


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


def fetch_metrics(url: str, timeout: float) -> list[Sample]:
    response = requests.get(url, timeout=timeout)
    response.raise_for_status()
    return parse_prometheus_text(response.text)


def to_metric_data(
    samples: list[Sample], dimensions: list[dict[str, str]]
) -> list[dict[str, object]]:
    """Turns scraped series into CloudWatch data points. Each series' labels become dimensions
    after the `--dimension` ones; a label with the same name as a `--dimension` replaces it, and
    a label with an empty value is dropped (CloudWatch rejects empty dimension values)."""
    data = []
    for sample in samples:
        merged = {d["Name"]: d["Value"] for d in dimensions}
        for label, label_value in sorted(sample.labels.items()):
            if label_value:
                merged[label] = label_value
        datum: dict[str, object] = {
            "MetricName": sample.name,
            "Value": sample.value,
            "Unit": METRIC_UNITS.get(sample.name, "None"),
        }
        if merged:
            datum["Dimensions"] = [{"Name": k, "Value": v} for k, v in merged.items()]
        data.append(datum)
    return data


def main(argv: Sequence[str] | None = None) -> int:
    args = build_arg_parser().parse_args(argv)

    try:
        samples = fetch_metrics(args.metrics_url, args.timeout)
    except requests.RequestException as err:
        print(f"error: failed to fetch {args.metrics_url}: {err}", file=sys.stderr)
        return 1

    if not samples:
        print(f"error: no metrics parsed from {args.metrics_url}", file=sys.stderr)
        return 1

    metric_data = to_metric_data(samples, args.dimension)

    cloudwatch = boto3.client("cloudwatch", region_name=args.region)
    try:
        for start in range(0, len(metric_data), MAX_METRICS_PER_CALL):
            cloudwatch.put_metric_data(
                Namespace=args.namespace,
                MetricData=metric_data[start : start + MAX_METRICS_PER_CALL],
            )
    except Exception as err:  # botocore raises various ClientError subclasses
        print(f"error: failed to publish to CloudWatch: {err}", file=sys.stderr)
        return 1

    print(f"published {len(metric_data)} metric(s) to CloudWatch namespace {args.namespace!r}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
