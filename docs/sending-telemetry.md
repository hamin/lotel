# Sending telemetry

lotel accepts OpenTelemetry over OTLP/HTTP from any app or SDK. This page covers the endpoints, formats and auth. To point a coding agent at lotel, see [Quickstart](quickstart.md).

## Endpoints

The receiver listens on `127.0.0.1:4318` by default. Set `LOTEL_OTLP_ADDR` to change it (see [Configuration](configuration.md)).

| Signal | Method and path |
| --- | --- |
| Logs | `POST /v1/logs` |
| Traces | `POST /v1/traces` |
| Metrics | `POST /v1/metrics` |

lotel serves OTLP over HTTP only. It does not run an OTLP/gRPC receiver on port 4317.

## Formats

The receiver takes both OTLP/HTTP encodings:

| Encoding | `Content-Type` |
| --- | --- |
| Protobuf | `application/x-protobuf` |
| JSON | `application/json` |

Any other content type gets `415`. Gzip request bodies (`Content-Encoding: gzip`) are accepted.

Metrics of all four OTLP shapes are stored: gauge, sum, histogram and exponential histogram.

## Auth

Every request needs the ingest token. lotel prints it in the startup banner, and the Dashboard and Setup pages show it too. Send it in either header:

```
Authorization: Bearer <token>
x-api-key: <token>
```

A missing or wrong token gets `401`. A request lotel accepts gets `202`.

The token is random on every start unless you set `LOTEL_INGEST_TOKEN`. Set it if you don't want to update your exporters after each restart. It must be at least 16 characters.

## Configure an SDK

Most OpenTelemetry SDKs read the standard exporter variables. Set the base endpoint, and the SDK adds `/v1/logs`, `/v1/traces` and `/v1/metrics` itself:

```sh
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <token>"
export OTEL_SERVICE_NAME=my-app
```

If your SDK asks for one endpoint per signal, use the full paths, for example `http://127.0.0.1:4318/v1/traces`.

The service name you set becomes the `service_name` column. Logs, traces, Services and Explore all filter and group by it.

## Example: one log record with curl

This sends one log line as OTLP JSON:

```sh
TOKEN=<token>
NOW=$(date +%s)000000000

curl -X POST http://127.0.0.1:4318/v1/logs \
  -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' \
  --data '{
    "resourceLogs": [{
      "resource": {"attributes": [
        {"key": "service.name", "value": {"stringValue": "my-app"}}
      ]},
      "scopeLogs": [{"logRecords": [{
        "timeUnixNano": "'"$NOW"'",
        "severityText": "INFO",
        "body": {"stringValue": "hello from curl"}
      }]}]
    }]
  }'
```

The response is `202` with a small JSON body such as `{"status":"buffered","rows":1,"batches":1}`. The receiver buffers rows and commits them shortly after, so the row shows up on `/logs` within a few seconds.

Rows age by their own timestamp. A record with an old `timeUnixNano` lands outside the default time ranges and may already be past its retention line. See [Retention](retention.md).

## Example: a Python app

With the OpenTelemetry Python SDK and the OTLP/HTTP exporter installed, the standard variables above are enough:

```sh
pip install opentelemetry-distro opentelemetry-exporter-otlp-proto-http
export OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf
export OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318
export OTEL_EXPORTER_OTLP_HEADERS="Authorization=Bearer <token>"
export OTEL_SERVICE_NAME=my-app
opentelemetry-instrument python app.py
```

## Sample traffic

If you have a checkout of the lotel repository, `scripts/otlp-gen.py` streams sample telemetry with no dependencies beyond Python. It simulates an `api-gateway`, `checkout` and `payments` topology: linked traces, error logs tied to those traces, and moving metrics.

```sh
scripts/otlp-gen.py --token <token> --once      # one burst, then exit
scripts/otlp-gen.py --token <token> --for 60    # stream for 60 seconds
```

The script sends OTLP JSON to `http://127.0.0.1:4318`. It also reads the token from `$LOTEL_TOKEN` and the endpoint from `$LOTEL_OTLP_URL`.

## Check the receiver

The Dashboard's "Ingest: otlp_serve" table shows request and row counts and the last error, if any. `GET /api/health` on the UI port returns `ok` when the store answers queries.
