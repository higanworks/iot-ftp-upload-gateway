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

wait_for_port() {
  local port="$1"
  local label="$2"
  for _ in $(seq 1 60); do
    if (exec 3<>"/dev/tcp/127.0.0.1/${port}") 2>/dev/null; then
      exec 3>&-
      echo "${label} is up."
      return 0
    fi
    sleep 1
  done
  echo "FAIL: ${label} did not become ready in time"
  return 1
}

# The gateway (a small Rust binary) starts listening almost immediately, well before the
# alpine-ftp-server backends finish their own entrypoint setup (creating the FTP user etc.) --
# waiting on the gateway's port alone isn't enough; each backend's control port must be up too.
wait_for_port "$GATEWAY_PORT" "Gateway control port"
wait_for_port "${BACKEND_PORTS[0]}" "Backend 1 control port"
wait_for_port "${BACKEND_PORTS[1]}" "Backend 2 control port"
wait_for_port "${BACKEND_PORTS[2]}" "Backend 3 control port"

failed=0

upload_and_verify() {
  local index="$1"
  local backend_port="$2"
  local filename="ci-test-${index}.txt"
  local local_file="${WORKDIR}/${filename}"

  echo "ci-integration-test-payload-${index}-$$" >"$local_file"

  local curl_trace="${WORKDIR}/${filename}.trace"
  if ! curl -4 -sS --disable-epsv -T "$local_file" --trace-ascii "$curl_trace" \
    "ftp://iot:pass123@127.0.0.1:${GATEWAY_PORT}/${filename}"; then
    echo "FAIL: upload for client ${index} did not succeed"
    echo "--- curl trace (${filename}) ---"
    cat "$curl_trace"
    echo "--- end curl trace ---"
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
