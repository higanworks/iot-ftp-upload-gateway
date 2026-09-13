#!/usr/bin/env bash
# Drives the docker-compose.ci.yml stack (gateway + 3 real FTP backends) exactly like the
# manual curl-based checks used throughout development: upload through the gateway, then read
# the file back directly from the backend it should have landed on (round-robin) to confirm
# content integrity and correct backend selection.
set -euo pipefail

GATEWAY_PORT=2131
BACKEND_PORTS=(2221 2222 2223)
WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

echo "Waiting for the gateway control port to accept connections..."
for _ in $(seq 1 30); do
  if (exec 3<>"/dev/tcp/127.0.0.1/${GATEWAY_PORT}") 2>/dev/null; then
    exec 3>&-
    echo "Gateway is up."
    break
  fi
  sleep 1
done

failed=0

upload_and_verify() {
  local index="$1"
  local backend_port="$2"
  local filename="ci-test-${index}.txt"
  local local_file="${WORKDIR}/${filename}"

  echo "ci-integration-test-payload-${index}-$$" >"$local_file"

  if ! curl -4 -fsS --disable-epsv -T "$local_file" \
    "ftp://iot:pass123@127.0.0.1:${GATEWAY_PORT}/${filename}"; then
    echo "FAIL: upload for client ${index} did not succeed"
    failed=1
    return
  fi

  local remote_file="${WORKDIR}/${filename}.remote"
  if ! curl -4 -fsS --disable-epsv \
    "ftp://iot:pass123@127.0.0.1:${backend_port}/${filename}" -o "$remote_file"; then
    echo "FAIL: could not read back ${filename} from backend port ${backend_port}"
    failed=1
    return
  fi

  if ! cmp -s "$local_file" "$remote_file"; then
    echo "FAIL: content mismatch for client ${index} on backend port ${backend_port}"
    failed=1
    return
  fi

  echo "OK: client ${index} correctly landed on backend port ${backend_port}"
}

# 4 clients against 3 backends proves round-robin including the wraparound back to backend 1.
upload_and_verify 1 "${BACKEND_PORTS[0]}"
upload_and_verify 2 "${BACKEND_PORTS[1]}"
upload_and_verify 3 "${BACKEND_PORTS[2]}"
upload_and_verify 4 "${BACKEND_PORTS[0]}"

if [ "$failed" -ne 0 ]; then
  echo "Docker integration test FAILED"
  exit 1
fi

echo "Docker integration test PASSED"
