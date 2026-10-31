//! QuickJS host loop for workflow scripts.

use std::cell::RefCell;
use std::rc::Rc;

use rquickjs::{Context, Ctx, Function, Promise, Runtime};
use serde::{Deserialize, Serialize};

use super::meta::{wrap_source, Meta, MetaError};

/// The JS half of the API. Promises and their resolvers stay in JS; the
/// host only sees ids and JSON strings.
const PRELUDE: &str = r#"
(() => {
  const pending = new Map();
  let currentPhase = null;
  globalThis.phase = (title) => { __clud_phase(String(title)); currentPhase = String(title); };
  globalThis.agent = (prompt, opts = {}) => new Promise((resolve, reject) => {
    const o = Object.assign({}, opts);
    if (o.phase === undefined && currentPhase !== null) o.phase = currentPhase;
    const id = __clud_agent(String(prompt), JSON.stringify(o));
    pending.set(id, [resolve, reject]);
  });
  globalThis.parallel = (thunks) =>
    Promise.all(thunks.map((t) => (typeof t === "function" ? t() : t)));
  globalThis.pipeline = (items, ...stages) =>
    Promise.all(items.map(async (item) => {
      let value = item;
      for (const stage of stages) value = await stage(value);
      return value;
    }));
  globalThis.__clud_resolve = (id, ok, err) => {
    const entry = pending.get(id);
    if (!entry) return;
    pending.delete(id);
    if (err === null || err === undefined) entry[0](JSON.parse(ok)); else entry[1](new Error(err));
  };
})();
"#;

/// One `agent()` call, handed to the runner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRequest {
    pub id: u32,
    pub prompt: String,
    pub label: Option<String>,
    pub phase: Option<String>,
    pub model: Option<String>,
    /// JSON Schema the result must satisfy; `None` means plain text.
    pub schema: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct AgentOpts {
    label: Option<String>,
    phase: Option<String>,
    model: Option<String>,
    schema: Option<serde_json::Value>,
}

/// Executes a batch of agent requests (possibly concurrently) and returns
/// one result per request, in order: the structured value (or the final
/// text as a JSON string) on success, an error message on failure.
pub type AgentBatchRunner<'a> =
    dyn FnMut(&[AgentRequest]) -> Vec<Result<serde_json::Value, String>> + 'a;

/// What a finished run produced.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkflowOutcome {
    pub meta: Meta,
    /// The script's `return` value (JSON `null` when it returns nothing).
    pub result: serde_json::Value,
    /// Every agent call with its result, in issue order.
    pub agents: Vec<(AgentRequest, Result<serde_json::Value, String>)>,
    /// Phases entered with `phase()`, in order.
    pub phases: Vec<String>,
}

#[derive(Default)]
struct State {
    meta: Option<Meta>,
    queued: Vec<AgentRequest>,
    next_id: u32,
    phases: Vec<String>,
    host_error: Option<String>,
}

fn js_err(ctx: &Ctx<'_>, e: rquickjs::Error) -> String {
    if matches!(e, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        if let Some(obj) = caught.as_object() {
            let field = |k: &str| {
                obj.get::<_, Option<String>>(k)
                    .ok()
                    .flatten()
                    .unwrap_or_default()
            };
            let (name, msg, stack) = (field("name"), field("message"), field("stack"));
            if !msg.is_empty() || !name.is_empty() {
                return format!("{name}: {msg}\n{stack}").trim().to_string();
            }
        }
        return format!("uncaught JS value: {caught:?}");
    }
    e.to_string()
}

fn register(ctx: &Ctx<'_>, state: &Rc<RefCell<State>>) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let s = Rc::clone(state);
    globals.set(
        "__clud_meta",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, text: String| {
            let parsed = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
            match Meta::from_json(parsed) {
                Ok(meta) => {
                    s.borrow_mut().meta = Some(meta);
                    Ok(())
                }
                Err(e) => Err(rquickjs::Exception::throw_message(&ctx, &e.to_string())),
            }
        })?,
    )?;
    let s = Rc::clone(state);
    globals.set(
        "__clud_phase",
        Function::new(ctx.clone(), move |ctx: Ctx<'_>, title: String| {
            let mut st = s.borrow_mut();
            if let Some(Err(e)) = st.meta.as_ref().map(|m| m.check_phase(&title)) {
                return Err(rquickjs::Exception::throw_message(&ctx, &e.to_string()));
            }
            st.phases.push(title);
            Ok(())
        })?,
    )?;
    let s = Rc::clone(state);
    globals.set(
        "__clud_agent",
        Function::new(
            ctx.clone(),
            move |ctx: Ctx<'_>, prompt: String, opts: String| {
                let opts: AgentOpts = serde_json::from_str(&opts).unwrap_or_default();
                let mut st = s.borrow_mut();
                if let (Some(meta), Some(phase)) = (st.meta.as_ref(), opts.phase.as_deref()) {
                    if let Err(e) = meta.check_phase(phase) {
                        return Err(rquickjs::Exception::throw_message(&ctx, &e.to_string()));
                    }
                }
                st.next_id += 1;
                let id = st.next_id;
                st.queued.push(AgentRequest {
                    id,
                    prompt,
                    label: opts.label,
                    phase: opts.phase,
                    model: opts.model,
                    schema: opts.schema,
                });
                Ok(id)
            },
        )?,
    )?;
    Ok(())
}

enum Settled {
    Pending,
    Done(serde_json::Value),
    Failed(String),
}

fn main_state(ctx: &Ctx<'_>) -> Result<Settled, String> {
    let promise: Promise<'_> = ctx
        .globals()
        .get("__clud_main")
        .map_err(|e| js_err(ctx, e))?;
    match promise.state() {
        rquickjs::promise::PromiseState::Pending => Ok(Settled::Pending),
        rquickjs::promise::PromiseState::Resolved => {
            let value: rquickjs::Value<'_> =
                promise.result().unwrap().map_err(|e| js_err(ctx, e))?;
            let text = ctx
                .json_stringify(value)
                .map_err(|e| js_err(ctx, e))?
                .map(|s| s.to_string())
                .transpose()
                .map_err(|e| js_err(ctx, e))?;
            Ok(Settled::Done(
                text.and_then(|t| serde_json::from_str(&t).ok())
                    .unwrap_or(serde_json::Value::Null),
            ))
        }
        rquickjs::promise::PromiseState::Rejected => {
            let err = ctx
                .globals()
                .get::<_, Option<String>>("__clud_error")
                .ok()
                .flatten()
                .unwrap_or_else(|| "workflow rejected".to_string());
            Ok(Settled::Failed(err))
        }
    }
}

/// Run `source` with `args` exposed as the global `args`, sending every
/// batch of queued `agent()` calls to `runner`.
pub fn run_script(
    source: &str,
    args: &serde_json::Value,
    runner: &mut AgentBatchRunner<'_>,
) -> Result<WorkflowOutcome, String> {
    let wrapped = wrap_source(source).map_err(|e| e.to_string())?;
    let rt = Runtime::new().map_err(|e| e.to_string())?;
    let context = Context::full(&rt).map_err(|e| e.to_string())?;
    let state = Rc::new(RefCell::new(State::default()));
    let mut agents: Vec<(AgentRequest, Result<serde_json::Value, String>)> = Vec::new();

    context.with(|ctx| -> Result<(), String> {
        register(&ctx, &state).map_err(|e| js_err(&ctx, e))?;
        ctx.eval::<(), _>(PRELUDE).map_err(|e| js_err(&ctx, e))?;
        let args_js = ctx
            .json_parse(args.to_string())
            .map_err(|e| js_err(&ctx, e))?;
        ctx.globals()
            .set("args", args_js)
            .map_err(|e| js_err(&ctx, e))?;
        ctx.eval::<(), _>(wrapped).map_err(|e| js_err(&ctx, e))
    })?;

    loop {
        while rt.is_job_pending() {
            if rt.execute_pending_job().is_err() {
                break;
            }
        }
        let settled = context.with(|ctx| main_state(&ctx))?;
        match settled {
            Settled::Done(result) => {
                let st = state.borrow();
                let meta = st
                    .meta
                    .clone()
                    .ok_or_else(|| MetaError::Missing.to_string())?;
                return Ok(WorkflowOutcome {
                    meta,
                    result,
                    agents,
                    phases: st.phases.clone(),
                });
            }
            Settled::Failed(err) => {
                let host = state.borrow_mut().host_error.take();
                return Err(host.unwrap_or(err));
            }
            Settled::Pending => {}
        }
        let batch = std::mem::take(&mut state.borrow_mut().queued);
        if batch.is_empty() {
            return Err("workflow is waiting on a promise that no agent() call will settle".into());
        }
        let mut results = runner(&batch);
        if results.len() != batch.len() {
            return Err(format!(
                "agent runner returned {} results for {} requests",
                results.len(),
                batch.len()
            ));
        }
        context.with(|ctx| -> Result<(), String> {
            let resolve: Function<'_> = ctx
                .globals()
                .get("__clud_resolve")
                .map_err(|e| js_err(&ctx, e))?;
            for (req, res) in batch.iter().zip(results.iter()) {
                let (ok, err) = match res {
                    Ok(v) => (Some(v.to_string()), None),
                    Err(e) => (None, Some(e.clone())),
                };
                resolve
                    .call::<_, ()>((req.id, ok, err))
                    .map_err(|e| js_err(&ctx, e))?;
            }
            Ok(())
        })?;
        agents.extend(batch.into_iter().zip(results.drain(..)));
    }
}
