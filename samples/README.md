# Samples

Reference scripts for operating `iot-ftp-upload-gateway`. Not part of the Rust build/test
pipeline (see [scripts/](../scripts/) for that) -- these are examples to adapt, not something
this project runs itself.

## cloudwatch_metrics.py

Scrapes the gateway's [`/metrics`](../README.md#metrics) endpoint and publishes the values to
CloudWatch as custom metrics. One-shot: run it periodically (cron / systemd timer), not as a
long-running daemon.

```sh
pip install -r requirements.txt

python3 cloudwatch_metrics.py \
  --metrics-url http://127.0.0.1:9273/metrics \
  --namespace IoTFtpUploadGateway \
  --dimension InstanceId=$(curl -s http://169.254.169.254/latest/meta-data/instance-id)
```

Run `python3 cloudwatch_metrics.py --help` for the full option list (namespace, dimensions,
region, timeout).

Without at least one `--dimension`, CloudWatch aggregates data points from every gateway
instance publishing to the same namespace together — pass one identifying the host (instance
ID, Auto Scaling Group name, etc.) if you run more than one instance.

### What gets published

Every series the gateway exposes becomes a CloudWatch metric of the same name — see the main
README's [Metrics](../README.md#metrics) for the list, including the upload outcome counters
(`uploads_started` / `completed` / `failed`), the backend failure and timeout counters, and
`data_connections_rejected`.

- **Labels become dimensions.** With the gateway's
  [source rotation](../README.md#backend-source-rotation) on, the per-source-address series carry
  `source` (and `backend`) labels; they are published with those as CloudWatch dimensions, after
  any `--dimension` you pass. A label with the same name as a `--dimension` replaces it.
- **Counters are cumulative.** `*_total` values are the gateway's running totals, published as
  they are, not per-interval deltas. Graph or alarm on them with metric math such as `RATE(m1)`.
- **Mind the cost.** Each distinct metric name plus dimension combination is a separate custom
  metric, billed as one. The plain series are 15 per `--dimension` set; with source rotation on,
  add one `unhealthy` per source and four per (backend, source) — three sources and one backend
  is 3 + 12 = 15 more. The script publishes everything it scrapes; it is a sample, so trim
  `to_metric_data` if you only want some of it.
- **Batching.** CloudWatch accepts at most 1000 data points per `PutMetricData` call; the script
  sends as many calls as needed.
- Lines the script cannot use (malformed, `NaN`/`Inf`) are skipped rather than sent, since
  CloudWatch rejects a whole call if one data point in it is invalid.

Alarms worth having: `RATE(ftp_gateway_uploads_failed_total)` above zero,
`RATE(ftp_gateway_backend_pasv_failures_total)` or `..._backend_timeouts_total` above zero (the
backend is struggling), `RATE(ftp_gateway_backend_source_slot_timeouts_total)` above zero (a source
address has hit the backend's concurrency limit — add source addresses),
`ftp_gateway_backend_source_unhealthy` at `1` for several periods (a source cannot connect), and
`RATE(ftp_gateway_data_connections_rejected_total)` above zero (a host is connecting to PASV ports
that are not its own, or devices' control and data connections leave from different addresses —
see `limits.require_data_ip_match` in the main README).

### Tests

The parsing and payload logic is covered by tests that need neither AWS nor a running gateway:

```sh
python3 -m unittest samples/test_cloudwatch_metrics.py
```

[testdata/gateway_metrics_rotation_on.txt](testdata/gateway_metrics_rotation_on.txt) is a real
`/metrics` scrape from a gateway with source rotation on.

### IAM permissions

The credentials boto3 resolves (instance profile, environment variables, `~/.aws/credentials`,
...) need `cloudwatch:PutMetricData`:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "cloudwatch:PutMetricData",
      "Resource": "*"
    }
  ]
}
```

(`PutMetricData` doesn't support resource-level restriction to a specific namespace; scope this
statement with a `cloudwatch:namespace` condition key instead if that matters for your account.)

### Running on a schedule

A systemd timer, for the EC2 host-networking deployment described in the main
[README](../README.md#production-deployment-on-ec2-host-networking):

```ini
# /etc/systemd/system/iot-ftp-upload-gateway-metrics.service
[Unit]
Description=Publish iot-ftp-upload-gateway metrics to CloudWatch

[Service]
Type=oneshot
ExecStart=/usr/bin/python3 /opt/iot-ftp-upload-gateway/samples/cloudwatch_metrics.py \
  --dimension InstanceId=%H
```

```ini
# /etc/systemd/system/iot-ftp-upload-gateway-metrics.timer
[Unit]
Description=Run iot-ftp-upload-gateway-metrics.service every minute

[Timer]
OnCalendar=*:0/1
Persistent=false

[Install]
WantedBy=timers.target
```

```sh
sudo systemctl enable --now iot-ftp-upload-gateway-metrics.timer
```

(`%H` expands to the host's short hostname, not the EC2 instance ID -- swap in an
`ExecStartPre` that resolves the instance ID via instance metadata if you need that specifically
as the dimension value.)

## docker-compose.transfer-family.yml

Runs the gateway with [AWS Transfer Family as the backend](../README.md#using-aws-transfer-family-as-the-backend)
on an EC2 instance, with Explicit FTPS to the backend, source-address rotation, JSON logs sent to
CloudWatch Logs, and the metrics endpoint on loopback. Unlike the compose files in the repository
root, it does not start any FTP server of its own.

1. **Edit the placeholders** in the file: the endpoint host name in `GATEWAY_BACKENDS`, the
   address devices reach the gateway at in `GATEWAY_PASSIVE_ADDRESS` (and the passive port range if
   you want another one), and the region and log group under `logging.options`.
2. **Check the network path.** The instance must reach the endpoint directly (no NAT gateway or NLB
   in between), the endpoint's security group must allow port 21 and 8192-8200 from every source
   address of the instance, and the instance's security group must allow the listen port and the
   passive port range from the devices. See the README section linked above for why.
3. **Give the instance role** `logs:CreateLogStream` and `logs:PutLogEvents` (plus
   `logs:CreateLogGroup` while `awslogs-create-group` is `"true"`). The Docker daemon does the
   sending, not the container.
4. **Start it and check the log:**

   ```sh
   docker compose -f samples/docker-compose.transfer-family.yml up -d
   docker compose -f samples/docker-compose.transfer-family.yml logs gateway
   ```

   `backend source address rotation enabled` lists the source addresses the gateway chose; each
   one adds roughly nine concurrent uploads.

Things to know:

- `network_mode: host` means there is no `ports:` mapping; the gateway's ports open on the host.
- The container runs as root (`user: "0:0"`) so it can listen on port 21; the comments in the file
  describe the non-root alternative.
- The image tag is pinned; bump it when you upgrade.
- To scrape the metrics with [cloudwatch_metrics.py](#cloudwatch_metricspy), use
  `--metrics-url http://127.0.0.1:9273/metrics`.
