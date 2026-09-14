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
