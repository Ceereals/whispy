//! `whispy-stt`: whispy's speech-to-text server on [transcribe.cpp], for models
//! whisper.cpp can't run (NVIDIA Parakeet and friends, as GGUF).
//!
//! It is a drop-in for whisper-server from whispy-daemon's point of view: same
//! command line (`-m MODEL -l LANG --host H --port P`), same `POST /inference`
//! (multipart WAV in, `verbose_json` subset out), and it only starts listening
//! once the model is loaded, so the daemon's "port open = ready" check and its
//! supervise-and-restart logic apply unchanged. Running the engine in this child
//! process also keeps a GPU/driver crash from taking the daemon down.
//!
//! One request is transcribed at a time (the library computes one run per model
//! at a time anyway); whispy sends one clip per dictation.
//!
//! [transcribe.cpp]: https://github.com/handy-computer/transcribe.cpp

mod multipart;
mod reply;

use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use clap::Parser;
use tiny_http::{Header, Method, Request, Response, Server};
use transcribe_cpp::{Backend, Model, ModelOptions, RunOptions, Session, SessionOptions};

use reply::Outcome;

/// Largest request body accepted: a 60 s clip is ~2 MB; this leaves headroom.
const MAX_BODY_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "whispy-stt",
    version,
    about = "Speech-to-text server for whispy (transcribe.cpp; whisper-server compatible)"
)]
struct Args {
    /// GGUF model file (e.g. parakeet-ultra-Q8_0.gguf).
    #[arg(short = 'm', long = "model")]
    model: PathBuf,
    /// Default language: auto, or an ISO code (a request's `language` field wins).
    #[arg(short = 'l', long = "language", default_value = "auto")]
    language: String,
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on.
    #[arg(long, default_value_t = 8181)]
    port: u16,
    /// Compute backend: auto (GPU when this build has one, else CPU), vulkan, or cpu.
    #[arg(long, default_value = "auto")]
    backend: String,
    /// CPU threads (0 = library default).
    #[arg(short = 't', long = "threads", default_value_t = 0)]
    threads: i32,
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("whispy-stt: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &Args) -> Result<(), String> {
    let backend = parse_backend(&args.backend)?;
    let started = Instant::now();
    let model = Model::load_with(
        &args.model,
        &ModelOptions {
            backend,
            ..ModelOptions::default()
        },
    )
    .map_err(|e| format!("loading {}: {e}", args.model.display()))?;
    let mut session = model
        .session_with(&SessionOptions {
            n_threads: args.threads,
            ..SessionOptions::default()
        })
        .map_err(|e| format!("creating a session: {e}"))?;
    let timestamps = model
        .capabilities()
        .map_err(|e| format!("reading model capabilities: {e}"))?
        .max_timestamp_kind;
    eprintln!(
        "whispy-stt: loaded {} ({} on {}) in {} ms",
        args.model.display(),
        model.arch(),
        model.backend(),
        started.elapsed().as_millis()
    );

    // Bind only now: an open port tells the daemon the model is ready.
    let server = Server::http((args.host.as_str(), args.port))
        .map_err(|e| format!("listening on {}:{}: {e}", args.host, args.port))?;
    eprintln!("whispy-stt: listening on {}:{}", args.host, args.port);

    let engine = Engine {
        session: &mut session,
        timestamps,
        default_language: &args.language,
    };
    serve(&server, engine);
    Ok(())
}

/// Map the `--backend` flag (same values as whispy's `stt.backend`).
fn parse_backend(name: &str) -> Result<Backend, String> {
    match name {
        "auto" => Ok(Backend::Auto),
        "vulkan" => Ok(Backend::Vulkan),
        "cpu" => Ok(Backend::Cpu),
        other => Err(format!("unknown --backend {other:?} (auto, vulkan, cpu)")),
    }
}

/// The loaded model's session plus per-run defaults.
struct Engine<'a> {
    session: &'a mut Session,
    timestamps: transcribe_cpp::TimestampKind,
    default_language: &'a str,
}

impl std::fmt::Debug for Engine<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("default_language", &self.default_language)
            .finish_non_exhaustive()
    }
}

/// Answer requests one at a time until the process is killed.
fn serve(server: &Server, mut engine: Engine<'_>) {
    for mut request in server.incoming_requests() {
        let (status, body) = route(&mut request, &mut engine);
        let mut response = Response::from_string(body.to_string()).with_status_code(status);
        // A constant ASCII header always builds; the daemon parses the body as JSON
        // either way.
        if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]) {
            response.add_header(header);
        }
        if let Err(e) = request.respond(response) {
            eprintln!("whispy-stt: failed to send a response: {e}");
        }
    }
}

/// Dispatch one request; returns the status code and JSON body.
fn route(request: &mut Request, engine: &mut Engine<'_>) -> (u16, serde_json::Value) {
    match (request.method(), request.url()) {
        (Method::Post, "/inference") => match inference(request, engine) {
            Ok(body) => (200, body),
            Err((status, msg)) => {
                eprintln!("whispy-stt: {msg}");
                (status, serde_json::json!({ "error": msg }))
            }
        },
        (Method::Get, "/" | "/health") => (200, serde_json::json!({ "status": "ok" })),
        _ => (404, serde_json::json!({ "error": "not found" })),
    }
}

/// `POST /inference`: multipart with a WAV `file` and an optional `language`.
fn inference(
    request: &mut Request,
    engine: &mut Engine<'_>,
) -> Result<serde_json::Value, (u16, String)> {
    let content_type = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    let boundary = multipart::boundary(&content_type).ok_or_else(|| {
        (
            400,
            "expected multipart/form-data with a boundary".to_string(),
        )
    })?;
    let mut body = Vec::new();
    request
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_end(&mut body)
        .map_err(|e| (400, format!("reading the request body: {e}")))?;
    let parts = multipart::parse(&body, &boundary).map_err(|e| (400, e))?;
    let field = |name: &str| parts.iter().find(|p| p.name == name);
    let wav = field("file").ok_or_else(|| (400, "missing the `file` part".to_string()))?;
    let language = field("language").map(|p| String::from_utf8_lossy(&p.data).into_owned());
    let hint = reply::language_hint(Some(language.as_deref().unwrap_or(engine.default_language)));

    let pcm = reply::decode_wav(&wav.data).map_err(|e| (400, format!("bad WAV: {e}")))?;
    let started = Instant::now();
    let transcript = engine
        .session
        .run(
            &pcm,
            &RunOptions {
                language: hint.clone(),
                timestamps: engine.timestamps,
                ..RunOptions::default()
            },
        )
        .map_err(|e| (500, format!("transcription failed: {e}")))?;
    eprintln!(
        "whispy-stt: {:.1} s of audio in {} ms",
        pcm.len() as f32 / reply::SAMPLE_RATE as f32,
        started.elapsed().as_millis()
    );
    let outcome = Outcome {
        text: transcript.text,
        language: transcript.language,
        token_probs: transcript.tokens.iter().map(|t| t.p).collect(),
    };
    Ok(reply::verbose_json(&outcome, hint.as_deref()))
}
