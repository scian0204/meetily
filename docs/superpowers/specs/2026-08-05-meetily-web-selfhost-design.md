# Meetily 셀프호스팅 웹 버전 설계 (2026-08-05)

## 배경 / 목표

현재 Meetily는 Tauri 데스크톱 앱이다. 오디오 캡처, whisper/parakeet 추론, SQLite 저장, LLM 요약이 모두 사용자 기기에서 돈다.

전환 목표 두 개:

1. **성능이 필요한 작업을 서버로**: whisper/parakeet 추론, 요약, DB, 모델 다운로드를 서버 컨테이너에서 실행.
2. **데스크톱 설치 제거**: 브라우저만으로 사용. 배포는 `docker compose up`.

## 비목표 (이번 범위 밖)

- 멀티유저 데이터 분리 (사용자별 회의 소유권). DB 스키마에 `user_id` 없음.
- BuiltInAI (`llama-helper` sidecar) 요약 provider. Ollama/클라우드로 대체.
- retranscription, legacy DB import, 데스크톱 알림, updater, 트레이, 온보딩 위저드.
- 데스크톱 앱 기능 축소. Tauri 앱은 그대로 빌드되고 동작해야 한다.

## 핵심 제약 (조사 결과)

- Rust 143개 파일 중 96개가 Tauri 무관. `whisper_engine`, `parakeet_engine`, `database::repositories`, `summary::processor/llm_client`, `audio::vad`, `audio::decoder`는 그대로 재사용 가능.
- `tauri`는 `frontend/src-tauri/Cargo.toml:150`에서 무조건 dependency. 따라서 같은 crate에 서버 bin을 추가하면 Linux 빌드에 webkit2gtk dev 라이브러리가 필요하다. 44개 파일을 feature-gate하는 대안은 diff가 5배 크고 데스크톱 앱 회귀 위험이 있어 채택하지 않는다.
- 프론트엔드는 `output: 'export'` 정적 빌드, HTTP 호출 0건, `invoke` 145종, `@tauri-apps/*` import 66개 파일.
- 브라우저는 데스크톱처럼 시스템 오디오를 직접 캡처할 수 없다. Chrome/Edge의 `getDisplayMedia({audio:true})`(탭/화면 오디오)가 유일한 경로이고 Firefox/Safari는 마이크만.
- 이 저장소의 `backend/` Python/FastAPI는 archive이며 사용 금지 (CLAUDE.md).

## 아키텍처

```
브라우저                                컨테이너 (meetily-server 바이너리 1개)
┌──────────────────────────┐            ┌──────────────────────────────────────┐
│ Next.js static export    │  GET /     │ axum                                  │
│  + web-shim              │───────────>│  ServeDir(out/)                       │
│                          │            │                                       │
│ invoke(cmd,args) ────────┼─POST ─────>│ /api/invoke/:cmd  dispatch table      │
│ listen(evt) ─────────────┼─SSE ───────│ /api/events       broadcast bus       │
│ AudioWorklet 16k mono ───┼─WebSocket ─>│ /api/audio/:session  VAD + whisper   │
│ 파일 업로드 ──────────────┼─multipart ─>│ /api/upload       background job     │
└──────────────────────────┘            │                                       │
                                        │ app_lib 재사용:                        │
                                        │  WhisperEngine / ParakeetEngine       │
                                        │  DatabaseManager + repositories       │
                                        │  summary::generate_meeting_summary    │
                                        │  audio::vad::ContinuousVadProcessor   │
                                        └──────────────────────────────────────┘
                                             /data 볼륨: sqlite, models, recordings
                                        선택 컨테이너: ollama
```

### 서버 (`frontend/src-tauri/src/server/`, bin `meetily-server`)

| 파일 | 책임 |
|---|---|
| `src/bin/meetily-server.rs` | main: 환경변수 읽기, 상태 초기화, listener bind |
| `server/mod.rs` | `ServerState`, router 조립 |
| `server/auth.rs` | 로그인, 세션 토큰, 미들웨어 |
| `server/dispatch.rs` | `match cmd` → 핸들러. 단일 파일, 테이블형 |
| `server/events.rs` | `tokio::sync::broadcast` → SSE |
| `server/live.rs` | WebSocket 오디오 수신 → VAD → 전사 → 저장 → 이벤트 |
| `server/upload.rs` | multipart 저장 → decode → 전사 → 저장 |
| `server/transcribe.rs` | 엔진 소유 + semaphore, whisper/parakeet 선택 |

`ServerState`:
```rust
pub struct ServerState {
    pub db: DatabaseManager,
    pub whisper: Arc<WhisperEngine>,
    pub parakeet: Arc<ParakeetEngine>,
    pub events: broadcast::Sender<EventEnvelope>,
    pub sessions: DashMap<String, Instant>,       // 로그인 세션 토큰 → 만료
    pub live: DashMap<String, LiveMeeting>,       // 녹음 세션
    pub transcribe_lock: Arc<Semaphore>,          // 동시 추론 1
    pub data_dir: PathBuf,
    pub http: reqwest::Client,
}
```

### 전송 규약

- `POST /api/invoke/:cmd`, 본문은 프론트가 보내는 args 객체 그대로. 응답 `200 {"ok": <value>}` 또는 `4xx/5xx {"error": "<message>"}`. shim이 error를 throw하므로 기존 `try/catch`가 그대로 동작한다.
- `GET /api/events` SSE. 프레임: `data: {"event":"transcript-update","payload":{...}}`. shim이 이벤트 이름별 리스너에 분배.
- `WS /api/audio/:session_id`. 클라이언트 → 서버: binary 프레임 = little-endian Int16 PCM, 16000Hz, mono. 텍스트 프레임 = `{"type":"stop"}` 등 제어. 서버 → 클라이언트: 없음 (전사 결과는 SSE로).
- `POST /api/upload` multipart(`file`, `meeting_name`) → `{"ok":{"meeting_id":"..."}}`, 진행률은 SSE.
- `POST /api/login` `{password}` → `Set-Cookie: meetily_session=...`. `POST /api/logout`. `GET /api/auth/status`.

### 클라이언트 shim (`frontend/src/lib/web-shim/`)

`next.config.js`가 `NEXT_PUBLIC_TARGET=web`일 때 webpack alias로 교체한다. 호출부(66개 파일) 수정 없음.

| 원본 모듈 | 대체 |
|---|---|
| `@tauri-apps/api/core` | `web-shim/core.ts` — `invoke()` 3-tier 라우팅 |
| `@tauri-apps/api/event` | `web-shim/event.ts` — SSE 기반 `listen`/`emit`, 로컬 emit도 같은 버스 |
| `@tauri-apps/plugin-store` | `web-shim/store.ts` — localStorage 백엔드, 같은 API (`load`, `get`, `set`, `save`) |
| `@tauri-apps/plugin-os` | `web-shim/os.ts` — `platform()` → userAgent 추정 |
| `@tauri-apps/plugin-updater`, `plugin-process` | `web-shim/updater.ts` — no-op |
| `@tauri-apps/api/app` | `web-shim/app.ts` — `getVersion()` 상수 |
| `@tauri-apps/api/path` | `web-shim/path.ts` — 가상 경로 문자열 |

`invoke()` 3-tier:
1. **로컬 처리**: 녹음 제어, 디바이스 열거(`navigator.mediaDevices.enumerateDevices`), 오디오 레벨, analytics(no-op), 권한, 폴더 열기, 콘솔 토글, 온보딩 상태(localStorage). 브라우저에서 끝나고 네트워크를 타지 않는다.
2. **HTTP dispatch**: DB, 설정, 요약, 모델 관리 등.
3. **미지원**: 명시적으로 throw + `console.warn`. 조용히 삼키지 않는다.

`web-shim/capture.ts`: `getUserMedia`(마이크) + 선택적 `getDisplayMedia`(탭/시스템 오디오)를 `AudioContext`에서 합성, `AudioWorkletNode`가 48k→16k 다운샘플 + Int16 변환, WebSocket으로 전송. 데스크톱의 RMS ducking/전문 믹싱은 브라우저 gain 노드로 대체하고 `pipeline.rs`는 사용하지 않는다.

`src/services/recordingService.ts`가 유일한 실질 재작성 대상: 같은 함수 시그니처를 유지하되 내부를 브라우저 캡처 + 세션 API로 바꾼다. 기존 이벤트 이름(`recording-started`, `transcript-update` 등)을 shim 이벤트 버스로 그대로 발행하므로 UI 컴포넌트는 무변경.

### 오디오 → 전사 파이프라인 (서버)

1. WebSocket 프레임 → `Vec<i16>` → `f32` 정규화 (`/32768.0`).
2. `ContinuousVadProcessor::new(16000, 2000)` 에 `process_audio(&[f32])`. 반환된 `SpeechSegment`마다 전사.
3. `Semaphore` 획득 후 `whisper.transcribe_audio(segment.samples, language)` 또는 parakeet.
4. 결과를 메모리 세션 버퍼에 append + `transcript-update` 이벤트 발행.
5. 정지 시 `flush()` → 남은 세그먼트 전사 → `TranscriptsRepository::save_transcript(pool, meeting_title, &segments, folder_path)` → `transcription-complete` 이벤트.
6. 원본 오디오는 `/data/recordings/<meeting>/`에 raw f32 append 후 ffmpeg로 m4a 변환(기존 `audio::encode::encode_single_audio` 재사용).

### 인증

- `MEETILY_PASSWORD` 환경변수. 로그인 성공 시 32바이트 랜덤 토큰을 `DashMap`에 (토큰 → 만료) 저장하고 `Set-Cookie: meetily_session=<token>; HttpOnly; SameSite=Lax; Path=/`. 재시작 시 세션 소멸(재로그인) — 서명 키 관리 불필요.
- 비교는 길이 검사 + 상수시간 XOR 루프.
- `/api/*`는 미들웨어로 보호. 예외: `/api/login`, `/api/auth/status`.
- **비밀번호 미설정 + 바인드 주소가 loopback 아님 → 서버 시작 거부.** `MEETILY_ALLOW_NO_AUTH=1`로만 우회.
- `MEETILY_COOKIE_SECURE=1`이면 `Secure` 속성 추가 (HTTPS 리버스 프록시 뒤).
- 워크스페이스는 하나. 접속 가능한 사람은 모든 회의록을 본다. 이건 의도된 셀프호스팅 모델이며 README에 명시한다.

### Docker

`docker/Dockerfile` 3 stage:

1. `node:22-slim` — `pnpm install`, `NEXT_PUBLIC_TARGET=web pnpm run build` → `frontend/out`.
2. `rust:1.83-bookworm` — apt: `build-essential cmake clang libclang-dev pkg-config libasound2-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev libssl-dev`. `cargo build --release --bin meetily-server` (+`--features ${FEATURES}`).
3. `debian:bookworm-slim` — 런타임 라이브러리 + ffmpeg + 바이너리 + `out/`.

`docker-compose.yml`: `meetily`(8080, `./data:/data`), profile `ollama`로 ollama 컨테이너. GPU는 `--build-arg FEATURES=cuda` + `BASE_IMAGE` 교체, compose에 주석 예시.

환경변수: `MEETILY_PASSWORD`, `MEETILY_BIND`(기본 `0.0.0.0:8080`), `MEETILY_DATA_DIR`(기본 `/data`), `MEETILY_ALLOW_NO_AUTH`, `MEETILY_COOKIE_SECURE`, `OLLAMA_ENDPOINT`.

## 에러 처리

- Rust: 핸들러는 `Result<Json<Value>, ApiError>`. `ApiError`가 status + 메시지를 `{"error":...}`로 직렬화. `anyhow::Error`/`sqlx::Error`에서 `From` 구현.
- 미지원 command: 404 + 서버 warn 로그. shim은 throw. 무음 실패 금지.
- WebSocket: 프레임 파싱 실패 시 close + 이유 로그. 세션은 정리.
- 업로드: 지원하지 않는 확장자/디코드 실패는 400 + 이유. 부분 저장 파일은 삭제.
- 전사 실패: `transcription-error` 이벤트 발행. 부분 전사는 보존한다.

## 테스트

- Rust 통합 테스트 (`tests/server_api.rs`): `tower::ServiceExt::oneshot`으로 라우터에 직접 요청. (a) 쿠키 없이 `/api/invoke/api_get_meetings` → 401, (b) 로그인 후 → 200 + 배열, (c) 미지원 command → 404, (d) 임시 SQLite로 meeting 저장→조회 왕복.
- 프론트 vitest (`frontend/tests/lib/web-shim.test.ts`): `invoke`가 로컬 테이블/HTTP/throw로 올바르게 분기하는지, 이벤트 버스가 SSE 프레임을 리스너에 전달하는지.
- 수동 검증: `docker compose up` 후 로그인 → 회의 목록 → 업로드 전사 → 요약 → 라이브 녹음.

## 구현 단계

1. 서버 골격: bin, state, 정적 서빙, 인증, DB/설정 dispatch, SSE. → 브라우저에서 회의 목록/상세 열림.
2. 요약 + LLM provider dispatch (Ollama/OpenAI/Claude/Groq/OpenRouter).
3. 모델 관리 dispatch (whisper/parakeet 목록·다운로드·로드, 진행률 SSE).
4. 업로드 전사 (`/api/upload` + 백그라운드 job).
5. 라이브: WebSocket + VAD + 증분 전사, `recordingService` 재작성 + `capture.ts`.
6. Docker/compose/문서.

## 위험과 완화

| 위험 | 완화 |
|---|---|
| 이 환경에 cargo/docker 없음 → 컴파일 미검증 | 시그니처를 정확히 조사해 코드 작성. 검증은 `docker compose build`. 빌드 오류는 반복 수정 |
| webkit 라이브러리로 이미지 비대 | 수용. 필요 시 후속 작업으로 tauri optional feature-gate |
| 브라우저 시스템 오디오 미지원 | Chrome/Edge 안내 + 마이크 전용 fallback, UI에 경고 |
| 동시 사용자 추론 대기 | semaphore 1개, 큐. 필요 시 엔진 풀로 승격 (`ponytail:` 주석으로 표시) |
| 145개 command 중 누락 | 미지원은 404 + 로그. 실사용 로그로 보강 |
