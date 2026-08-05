# Self-hosting Meetily on the web

Run Meetily as a web app instead of a desktop install. Transcription, summarization,
model downloads, and storage all happen on the server; the browser only captures audio
and renders the UI.

```bash
cp .env.docker.example .env
# edit .env and set MEETILY_PASSWORD
docker compose up -d --build
```

Open <http://localhost:8080>, sign in with that password, and record.

To run a local LLM for summaries alongside it:

```bash
docker compose --profile ollama up -d --build
docker compose exec ollama ollama pull gemma3:1b
```

## What runs where

| Concern | Desktop app | Self-hosted web |
|---|---|---|
| Audio capture | cpal + ScreenCaptureKit/WASAPI | browser `getUserMedia` + `getDisplayMedia` |
| Mixing | Rust pipeline with RMS ducking | WebAudio gain nodes in the browser |
| Transcription | on your machine | on the server (Whisper or Parakeet) |
| Summaries | bundled model / Ollama / cloud | Ollama / cloud |
| Storage | app data directory | the `./data` volume |

The browser streams 16 kHz mono PCM over a WebSocket; the server segments it,
transcribes each utterance, and pushes results back over server-sent events.

## Browser support

| Browser | Microphone | System / meeting audio |
|---|---|---|
| Chrome, Edge | yes | yes — pick the meeting tab in the share dialog, keep **Share tab audio** checked |
| Firefox, Safari | yes | no — microphone only |

There is no browser API for "capture everything the OS is playing". Chrome's tab/screen
share is the closest thing, so on other browsers Meetily records your microphone and
skips the far end. Recording still works; remote participants just are not transcribed
unless they come through your speakers loudly enough.

The page must be served over HTTPS (or `localhost`) — browsers refuse microphone access
otherwise. Put a TLS-terminating reverse proxy in front of the container and set
`MEETILY_COOKIE_SECURE=1`.

## Configuration

| Variable | Default | Purpose |
|---|---|---|
| `MEETILY_PASSWORD` | — | Shared login password. Required on any non-loopback bind. |
| `MEETILY_PORT` | `8080` | Host port published by compose. |
| `MEETILY_BIND` | `0.0.0.0:8080` | Listen address inside the container. |
| `MEETILY_DATA_DIR` | `/data` | SQLite, models, recordings, uploads. |
| `MEETILY_WEB_ROOT` | `/app/web` | Static UI directory. |
| `MEETILY_ALLOW_NO_AUTH` | unset | Set to `1` to run without a password on a public bind. |
| `MEETILY_COOKIE_SECURE` | `0` | Set to `1` behind HTTPS so the session cookie is `Secure`. |
| `OLLAMA_ENDPOINT` | unset | Overrides the endpoint stored in settings. |
| `MEETILY_FEATURES` | empty | Cargo features for the build, e.g. `cuda`, `vulkan`. |
| `MEETILY_RUST_BASE` | `rust:1.83-bookworm` | Builder base image. |

## Security model

One password, one workspace. Anyone who can sign in can read **every** meeting and
transcript in the database, and can change model settings and API keys. There are no
user accounts and no per-meeting permissions.

The server refuses to start when `MEETILY_PASSWORD` is unset and the bind address is not
loopback. Override that only when something else already protects it — a reverse proxy
with its own auth, a VPN, or a private network — by setting `MEETILY_ALLOW_NO_AUTH=1`.

Sessions are random tokens held in memory, so restarting the container signs everyone
out. API keys for cloud LLM providers are stored unencrypted in the SQLite file, exactly
as in the desktop app; treat the `./data` volume as a secret.

## GPU acceleration

The default image is CPU-only, which runs anywhere. For an NVIDIA host:

```bash
MEETILY_FEATURES=cuda \
MEETILY_RUST_BASE=nvidia/cuda:12.4.1-devel-ubuntu22.04 \
docker compose up -d --build
```

Then uncomment the `deploy.resources.reservations.devices` block in
`docker-compose.yml`. For AMD or Intel use `MEETILY_FEATURES=vulkan`. Parakeet runs on
CPU regardless of this setting.

Transcription is serialized behind a single inference slot. Two people recording at once
works, but their utterances queue.

## Data layout

```
data/
  meeting_minutes.sqlite   meetings, transcripts, summaries, settings
  models/                  downloaded Whisper and Parakeet models
  recordings/              per-meeting audio
  uploads/                 staging for uploaded files, cleared automatically
```

Back it up by stopping the container and copying the directory. To migrate from a
desktop install, copy its `meeting_minutes.sqlite` into `data/` before the first start.

## Choosing models

Open Settings in the UI:

- **Transcription** — Parakeet (fast, CPU, English) or Whisper (`large-v3-turbo` is a
  good default, `base` if the server is small). Downloads land in `data/models`.
- **Summaries** — Ollama through the compose profile, or an API key for OpenAI, Claude,
  Groq, or OpenRouter. The desktop app's bundled llama.cpp model is not included in the
  server image.

## Not available in the web build

- The bundled local summary model (use Ollama or a cloud provider)
- Retranscribing an existing meeting
- Importing a legacy database through the UI — copy the file into `data/` instead
- Desktop notifications, the system tray, global shortcuts, and the auto-updater
  (update with `docker compose pull && docker compose up -d`)

Unsupported commands fail with an explicit error in the UI and a warning in the server
log rather than doing nothing.

## Troubleshooting

**Server exits immediately** — `MEETILY_PASSWORD` is unset on a public bind. Check
`docker compose logs meetily`.

**"No Parakeet models are available"** — download a transcription model in Settings.

**No transcript appears while recording** — confirm the browser granted microphone
access, then check `docker compose logs -f meetily` for transcription errors. The first
utterance also waits for the model to load.

**Summaries fail with a connection error** — the Ollama endpoint is unreachable. With
the compose profile it is `http://ollama:11434`, not `localhost`.

## Building without Docker

```bash
cd frontend && NEXT_PUBLIC_TARGET=web pnpm run build   # produces frontend/out
cd .. && cargo build --release --bin meetily-server
MEETILY_PASSWORD=secret MEETILY_DATA_DIR=./data MEETILY_WEB_ROOT=./frontend/out \
  ./target/release/meetily-server
```

The web export must exist before `cargo build`: the crate embeds it at compile time.
Linux builds need the GTK/WebKit development packages listed in `docker/Dockerfile`,
because the crate still depends on Tauri even though the server never opens a window.
