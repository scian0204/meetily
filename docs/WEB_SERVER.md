# Meetily Web (self-hosted server)

Meetily also runs as a web app: the server does every hardware-heavy job, the
browser only shows the UI and captures audio.

| Runs on the server | Runs in the browser |
|---|---|
| Audio mixing, VAD, noise filtering, loudness normalisation | Microphone capture (`getUserMedia`) |
| Whisper / Parakeet transcription (CPU or GPU) | Tab / screen audio capture (`getDisplayMedia`) |
| Built-in summary model (llama-helper), Ollama / cloud LLM calls | The Meetily UI |
| ffmpeg encoding, audio import and re-transcription | File picking and upload for import |
| Model downloads, SQLite database, recordings | |

## Quick start (Docker)

```bash
MEETILY_PASSWORD=change-me docker compose up -d --build
```

Open <http://localhost:8080> on the same machine and sign in.

Browsers only allow microphone capture on **HTTPS or localhost**. To use Meetily
from other devices, put it behind HTTPS. The bundled Caddy profile does that:

```bash
MEETILY_PASSWORD=change-me MEETILY_DOMAIN=meet.example.com docker compose --profile https up -d --build
```

A public domain gets a Let's Encrypt certificate. `localhost`, `*.local`,
`*.internal`, `*.home.arpa` and IP addresses get a certificate from Caddy's local CA;
for any other private name (`meetily.lan`, `nas`) also set `MEETILY_TLS="tls internal"`.
Trust Caddy's local CA once in each browser. Set `MEETILY_COOKIE_SECURE=1` when
everything goes through HTTPS.

The first build compiles whisper.cpp, llama.cpp and the Rust core; expect 30 to 60
minutes. Later builds reuse the cache. Each compile job can use 1 to 2 GB of RAM; the
default is 4 jobs (`MEETILY_BUILD_JOBS`).

### GPU (NVIDIA)

Uncomment the `deploy` block in `docker-compose.yml` (needs the NVIDIA Container
Toolkit), then build and start with the CUDA images:

```bash
MEETILY_PASSWORD=change-me \
MEETILY_BUILD_IMAGE=nvidia/cuda:12.4.1-devel-ubuntu22.04 \
MEETILY_RUNTIME_IMAGE=nvidia/cuda:12.4.1-runtime-ubuntu22.04 \
MEETILY_WHISPER_FEATURES=cuda MEETILY_LLAMA_FEATURES=cuda MEETILY_IMAGE_TAG=cuda \
docker compose up -d --build
```

Whisper and the built-in summary model then run on the GPU; Parakeet always runs on
the CPU. Only CUDA is wired up in the Docker build (Vulkan, OpenBLAS and ROCm need
extra packages or base images).

## Recording from the browser

- Pressing record asks for the microphone, then for a tab or screen to share.
  Pick the tab or window of your meeting and keep **Share audio** checked: that is
  how the other participants are recorded (the desktop app records the system
  output instead). Chrome and Edge support this; Firefox and Safari record the
  microphone only.
- To record the microphone only, choose **None (microphone only)** as the system
  audio device in Settings.
- Keep the tab open while recording. If it is reloaded, the microphone is
  re-attached automatically and a banner offers to share tab audio again. Brief
  network drops are survived: commands, their results and live events resume after
  reconnecting.
- Audio is streamed uncompressed (about 1.5 Mbit/s with tab audio). On a slower
  upload link the page warns, and Stop waits until the remaining audio is sent.
- Only one tab streams audio at a time; a second tab never takes over a recording
  that is still receiving audio.
- "Open folder" on a meeting opens a page with its files and an audio player.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `MEETILY_PASSWORD` | unset | Sign-in password. Without it the server only starts on a loopback address. |
| `MEETILY_ALLOW_NO_AUTH` | unset | `1` allows a non-loopback address without a password. |
| `MEETILY_ALLOWED_HOSTS` | unset | Without a password: host names accepted besides `localhost` and IP addresses (comma-separated). |
| `MEETILY_BIND` | `127.0.0.1:8080` (`0.0.0.0:8080` in Docker) | Listen address. |
| `MEETILY_WEB_ROOT` | `<exe dir>/web` (`/app/web` in Docker) | The Next.js static export (`frontend/out`). |
| `MEETILY_COOKIE_SECURE` | unset | `1` marks the session cookie `Secure`. |
| `MEETILY_RECORDINGS_DIR` | `<app data>/recordings` | Meeting folders. |
| `MEMORY_GB` | `8` | RAM hint for Whisper's quality tier. |
| `RUST_LOG` | `info` | Log level. |

App data lives in `$XDG_DATA_HOME/com.meetily.ai` (in Docker: the `/data` volume):
the database, models, settings, `recordings/` and temporary `uploads/`.

For Ollama, start the `ollama` profile and set the endpoint to
`http://ollama:11434` in Meetily's model settings (`localhost` inside the container
is Meetily itself).

### Behind your own reverse proxy

Proxy everything to port 8080, allow WebSocket upgrades on `/api/ws` and
`/api/audio`, disable read timeouts for them (stopping a long recording can take
minutes), and pass the original `Host` (or `X-Forwarded-Host`): the server refuses
WebSocket and upload requests whose `Origin` does not match it. Uploads for audio
import can be several GB, so lift body-size limits on `/api/upload`.

## Security model

- One shared workspace: everyone who signs in sees all meetings and settings,
  including stored API keys. Use one deployment per team you trust.
- Sessions are in memory; restarting the server signs everyone out.
- Password checks are serialized server-wide with a 1 s penalty per wrong guess.
- Without a password, requests must name the server as `localhost`, an IP address or
  a host in `MEETILY_ALLOWED_HOSTS`, so other websites cannot reach it through DNS
  rebinding.
- Cross-site requests are refused (`Origin` check), the session cookie is
  `HttpOnly` and `SameSite=Strict`, and file paths sent by the browser are confined
  to the server's upload and recordings directories (settings stores to plain
  `<name>.json` files in the app data directory).
- Commands that would act on the server machine itself (opening folders, native
  dialogs, notifications, updater, exiting the process) are refused; the browser
  handles them.
- Telemetry is off.

## How it works

`meetily-server` is the desktop app's Rust crate (`frontend/src-tauri`) built with
the `server` feature. It builds the same Tauri app (plugins, state, every command)
on Tauri's `MockRuntime` instead of a native window, and serves:

| Path | Purpose |
|---|---|
| `/` | The static UI, with `/__meetily/runtime.js` injected first into every page |
| `/api/ws` | Commands (`invoke`), their results, and app events on one ordered WebSocket; reconnects resume pending commands and replay missed events |
| `/api/audio` | Browser audio: 48 kHz Int16 PCM frames, microphone and tab audio interleaved |
| `/api/upload` | Raw file upload for audio import |
| `/api/meetings/<id>/files/` | A meeting's files and recording player |
| `/login`, `/api/login`, `/api/logout`, `/api/session` | Password sign-in |

`frontend/web-runtime/runtime.js` implements `window.__TAURI_INTERNALS__` (the
interface every `@tauri-apps/*` package uses), so the UI runs unmodified. It
answers browser-only commands itself: device listing, permissions, file pickers,
drag and drop, opening links, and starting/stopping capture around the recording
commands. Browser audio replaces the cpal devices through `audio::web_source`;
mixing, VAD, transcription and saving are the desktop code paths.

Server code: `frontend/src-tauri/src/server/` (entry point
`frontend/src-tauri/src/server_main.rs`). Build and run it without Docker (Linux):

```bash
cd frontend && pnpm install && pnpm run build && cd ..
cargo build --release -p llama-helper
mkdir -p frontend/src-tauri/binaries
cp target/release/llama-helper "frontend/src-tauri/binaries/llama-helper-$(rustc -vV | sed -n 's/^host: //p')"
cargo build --release -p meetily --features server --bin meetily-server
MEETILY_WEB_ROOT=frontend/out target/release/meetily-server
```

(`build.rs` downloads ffmpeg into `frontend/src-tauri/binaries` when missing.)

Tests: `cargo test -p meetily --features server --lib server::` and
`cd frontend && bun test tests/web-runtime`.
