# REST API

Start with `wrosecode --web --listen 127.0.0.1:7878`.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | Readiness probe |
| GET | `/v1/status` | Full dashboard snapshot (schema `wrosecode/live-v1`: panels, plan, flags, files, processes, usage and cost) plus the legacy `metrics` object |
| POST | `/v1/chat` | Run a task; body is `{"prompt":"..."}` |
| GET | `/v1/sessions/latest` | Latest SQLite checkpoint |

Bind to localhost unless an authenticated reverse proxy protects the service. The API can execute tools under the configured permission tier.
