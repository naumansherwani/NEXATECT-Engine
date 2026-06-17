use anyhow::Result;
use axum::{extract::State, http::StatusCode, response::Json, routing::post, Router};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{env, sync::Arc, time::Duration};
use tokio::time::sleep;

#[derive(Clone)]
struct AppState {
    http: Client,
    or_key: String,
    groq_key: String,
    sb_url: String,
    sb_key: String,
}

#[derive(Deserialize)]
struct ChatReq { message: String }

#[derive(Serialize)]
struct ChatResp { agent: String, final_answer: String, candidates: Vec<Value> }

// --- ENSEMBLE ROSTERS (read from env, defaults match locked memory) ---
fn jimmy_slots() -> Vec<(&'static str, &'static str, String)> {
    vec![
        ("openrouter", "reasoning",        env::var("JIMMY_REASONING_MODEL").unwrap_or("nousresearch/hermes-3-llama-3.1-405b".into())),
        ("openrouter", "coding_primary",   env::var("JIMMY_CODING_MODEL").unwrap_or("qwen/qwen3-coder".into())),
        ("groq",       "speed_draft",      env::var("JIMMY_GROQ_MODEL").unwrap_or("qwen/qwen3-32b".into())),
        ("openrouter", "coding_diversity", env::var("JIMMY_DIVERSITY_MODEL").unwrap_or("qwen/qwen3-next-80b-a3b-instruct".into())),
    ]
}
fn sherlock_slots() -> Vec<(&'static str, &'static str, String)> {
    vec![
        ("openrouter", "infra_security",    env::var("SHERLOCK_INFRA_MODEL").unwrap_or("deepseek/deepseek-r1".into())),
        ("openrouter", "structured_verify", env::var("SHERLOCK_VERIFY_MODEL").unwrap_or("openai/gpt-oss-120b".into())),
        ("groq",       "speed_scan",        env::var("SHERLOCK_GROQ_MODEL").unwrap_or("llama-3.3-70b-versatile".into())),
        ("openrouter", "diversity_audit",   env::var("SHERLOCK_DIVERSITY_MODEL").unwrap_or("qwen/qwen3-next-80b-a3b-instruct".into())),
    ]
}
fn jimmy_judge() -> String { env::var("JIMMY_JUDGE_MODEL").unwrap_or("nousresearch/hermes-3-llama-3.1-405b".into()) }
fn sherlock_judge() -> String { env::var("SHERLOCK_JUDGE_MODEL").unwrap_or("deepseek/deepseek-r1".into()) }

async fn call_model(http: &Client, provider: &str, model: &str, or_key: &str, groq_key: &str, system: &str, user: &str) -> Result<String> {
    let (url, key) = match provider {
        "groq" => ("https://api.groq.com/openai/v1/chat/completions", groq_key),
        _      => ("https://openrouter.ai/api/v1/chat/completions", or_key),
    };
    let body = json!({
        "model": model,
        "messages": [{"role":"system","content":system},{"role":"user","content":user}],
        "max_tokens": 1200, "temperature": 0.4
    });
    let r = http.post(url).bearer_auth(key).json(&body).send().await?;
    let v: Value = r.json().await?;
    Ok(v["choices"][0]["message"]["content"].as_str().unwrap_or("").to_string())
}

async fn log_activity(state: &AppState, agent: &str, event: &str, payload: Value) {
    if state.sb_url.is_empty() || state.sb_key.is_empty() { return; }
    let url = format!("{}/rest/v1/agent_activity", state.sb_url);
    let _ = state.http.post(&url)
        .header("apikey", &state.sb_key)
        .bearer_auth(&state.sb_key)
        .header("Content-Type","application/json")
        .header("Prefer","return=minimal")
        .json(&json!({"agent_slug": agent, "event_type": event, "payload": payload}))
        .send().await;
}

async fn run_ensemble(state: &AppState, agent: &str, message: &str) -> ChatResp {
    let (slots, judge, system) = if agent == "jimmy" {
        (jimmy_slots(), jimmy_judge(), "You are Jimmy — CEO autopilot of AXONETIS. Reason deeply, write production-grade code, and protect the founder's vision.")
    } else {
        (sherlock_slots(), sherlock_judge(), "You are Sherlock — chief security & infra auditor. Veto anything risky. Be precise.")
    };

    let mut handles = vec![];
    for (provider, role, model) in slots {
        let http = state.http.clone();
        let or = state.or_key.clone();
        let gq = state.groq_key.clone();
        let m = model.clone();
        let msg = message.to_string();
        let sys = system.to_string();
        handles.push(tokio::spawn(async move {
            let out = call_model(&http, provider, &m, &or, &gq, &sys, &msg).await
                .unwrap_or_else(|e| format!("[error: {}]", e));
            json!({"provider": provider, "role": role, "model": m, "output": out})
        }));
    }
    let mut candidates = vec![];
    for h in handles { if let Ok(v) = h.await { candidates.push(v); } }

    // Judge merge
    let merged = candidates.iter().enumerate()
        .map(|(i,c)| format!("--- Candidate {} ({} / {})\n{}\n", i+1, c["role"].as_str().unwrap_or(""), c["model"].as_str().unwrap_or(""), c["output"].as_str().unwrap_or("")))
        .collect::<String>();
    let judge_prompt = format!("Founder asked:\n{}\n\nCandidate answers from your ensemble:\n{}\n\nMerge into ONE final answer. Keep founder's tone (short Roman Urdu/English mix). If any candidate flags critical risk, surface it.", message, merged);
    let final_answer = call_model(&state.http, "openrouter", &judge, &state.or_key, &state.groq_key,
        if agent=="jimmy" {"You are the Jimmy judge. Score-then-merge."} else {"You are the Sherlock judge. Veto-then-consensus."},
        &judge_prompt).await.unwrap_or_else(|e| format!("[judge error: {}]", e));

    log_activity(state, agent, "ensemble_cycle", json!({
        "message": message, "candidates": candidates, "final": final_answer
    })).await;

    ChatResp { agent: agent.into(), final_answer, candidates }
}

async fn chat_jimmy(State(s): State<Arc<AppState>>, Json(req): Json<ChatReq>) -> (StatusCode, Json<ChatResp>) {
    let r = run_ensemble(&s, "jimmy", &req.message).await;
    (StatusCode::OK, Json(r))
}
async fn chat_sherlock(State(s): State<Arc<AppState>>, Json(req): Json<ChatReq>) -> (StatusCode, Json<ChatResp>) {
    let r = run_ensemble(&s, "sherlock", &req.message).await;
    (StatusCode::OK, Json(r))
}

#[tokio::main]
async fn main() -> Result<()> {
    let state = Arc::new(AppState {
        http: Client::builder().timeout(Duration::from_secs(180)).build()?,
        or_key:   env::var("OPENROUTER_API_KEY").unwrap_or_default(),
        groq_key: env::var("GROQ_API_KEY").unwrap_or_default(),
        sb_url:   env::var("SUPABASE3_URL").unwrap_or_default(),
        sb_key:   env::var("SUPABASE3_SERVICE_KEY").unwrap_or_default(),
    });

    // Heartbeat loop (background)
    let s2 = state.clone();
    tokio::spawn(async move {
        loop {
            println!("\n========== [HEARTBEAT] JIMMY+SHERLOCK STANDING BY ==========");
            let j = run_ensemble(&s2, "jimmy", "Status check: company kaisi chal rahi hai? 1 line.").await;
            println!("[JIMMY] {}", j.final_answer);
            let sh = run_ensemble(&s2, "sherlock", "Security check: koi anomaly? 1 line.").await;
            println!("[SHERLOCK] {}", sh.final_answer);
            sleep(Duration::from_secs(60)).await;
        }
    });

    let app = Router::new()
        .route("/chat/jimmy", post(chat_jimmy))
        .route("/chat/sherlock", post(chat_sherlock))
        .with_state(state);

    let addr: std::net::SocketAddr = "0.0.0.0:8088".parse()?;
    println!("🚀 AXONETIS Rust runtime → http://{}  (POST /chat/jimmy, /chat/sherlock)", addr);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
