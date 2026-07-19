//! The I2C worker thread.
//!
//! # Why a thread rather than a tokio task
//!
//! DDC transactions are blocking and sleep-heavy by nature (the timings are
//! hardware tolerances, not politeness), and `AvServiceTransport` is `Send` but
//! deliberately not `Sync`. Parking all of that on a dedicated OS thread keeps
//! the async runtime free and makes serialization structural: there is exactly
//! one thread that may touch I2C, so concurrent access cannot happen.
//!
//! # Deviation from the plan
//!
//! The plan specifies one worker *per display*. This is one worker for **all**
//! displays, which is stricter: it serializes across displays that could in
//! principle be driven in parallel. The cost is latency when driving several
//! monitors at once (each transaction is ~100 ms, so `--all` on four monitors
//! takes ~0.4 s). The benefit is that it cannot get the fragile case wrong.
//! Revisit once there is hardware to prove parallel buses are safe; the channel
//! interface here does not change if the inside becomes a worker pool.

use crate::automation::{Automation, WorldState};
use crate::engine::{Engine, EngineError};
use display_api::protocol as api;
use display_core::DisplayBackend;
use std::sync::mpsc;
use tokio::sync::oneshot;

type Reply<T> = oneshot::Sender<Result<T, EngineError>>;

pub enum Command {
    List(Reply<Vec<api::MonitorInfo>>),
    Get {
        display: String,
        code: String,
        reply: Reply<api::VcpValue>,
    },
    Set {
        display: String,
        code: String,
        value: u16,
        verify: bool,
        reply: Reply<api::SetResult>,
    },
    Caps {
        display: String,
        reply: Reply<api::CapsResult>,
    },
    ProfileList(Reply<Vec<api::ProfileSummary>>),
    ProfileApply {
        name: String,
        verify: bool,
        yes: bool,
        reply: Reply<api::ApplyResult>,
    },
    ProfileSave {
        name: String,
        codes: Vec<display_ddc::vcp::VcpCode>,
        force: bool,
        reply: Reply<api::ProfileSummary>,
    },
    ProfileDelete {
        name: String,
        reply: Reply<()>,
    },
    ProfileShow {
        name: String,
        reply: Reply<display_core::Profile>,
    },
    /// Poll the world and run any rules that fired. Driven by the daemon's
    /// timer; all automation state stays in the worker so nothing else needs a
    /// view of the hardware.
    AutomationTick(Reply<Vec<api::RuleFired>>),
    RulesList(Reply<Vec<api::RuleInfo>>),
    RulesReload(Reply<Vec<api::RuleInfo>>),
}

#[derive(Clone)]
pub struct WorkerHandle {
    tx: mpsc::Sender<Command>,
}

impl WorkerHandle {
    /// Send a command and await its reply.
    ///
    /// A dropped reply channel means the worker died — surfaced as an error
    /// rather than a hang.
    pub async fn send<T>(&self, make: impl FnOnce(Reply<T>) -> Command) -> Result<T, EngineError> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(make(tx)).map_err(|_| {
            EngineError::Backend(display_core::Error::Transport(
                "worker thread is gone".into(),
            ))
        })?;
        rx.await.unwrap_or_else(|_| {
            Err(EngineError::Backend(display_core::Error::Transport(
                "worker dropped the request".into(),
            )))
        })
    }
}

/// Gather the world and fire rules. Lives here so all hardware state stays on
/// the worker thread.
fn automation_tick<B: DisplayBackend>(
    engine: &mut Engine<B>,
    automation: &mut Automation,
) -> Result<Vec<api::RuleFired>, EngineError> {
    let displays = engine.display_names()?;
    let power = engine.power_source()?;
    let now = WorldState::now(displays, power);

    let mut out = Vec::new();
    for rule in automation.tick(&now) {
        // One failing rule must not stop the others: they are independent, and
        // a monitor that NACKs should not disable the user's whole automation.
        let (ok, detail) = match engine.execute(&rule) {
            Ok(d) => (true, d),
            Err(e) => {
                tracing::warn!("rule {:?} failed: {e}", rule.name);
                (false, e.to_string())
            }
        };
        tracing::info!("rule {:?} fired: {detail}", rule.name);
        out.push(api::RuleFired {
            name: rule.name,
            ok,
            detail,
        });
    }
    Ok(out)
}

fn rule_infos(set: &display_core::RuleSet) -> Vec<api::RuleInfo> {
    set.rules
        .iter()
        .map(|r| api::RuleInfo {
            name: r.name.clone(),
            trigger: r.trigger.to_string(),
            action: r.action.to_string(),
            enabled: r.enabled,
            force: r.force,
        })
        .collect()
}

/// Spawn the worker thread and return a handle to it.
pub fn spawn<B: DisplayBackend + Send + 'static>(backend: B) -> WorkerHandle {
    let (tx, rx) = mpsc::channel::<Command>();

    std::thread::Builder::new()
        .name("displayd-i2c".into())
        .spawn(move || {
            let mut engine = Engine::new(backend);
            if let Err(e) = engine.refresh() {
                tracing::warn!("initial enumeration failed: {e}");
            }

            let rules_store = display_core::RulesStore::default_location();
            if rules_store.is_insecure() {
                tracing::warn!(
                    "rules file {} is writable by other users; it can run shell commands — \
                     consider chmod 600",
                    rules_store.path().display()
                );
            }
            let rules = rules_store.load().unwrap_or_else(|e| {
                // Bad rules must not stop the daemon serving brightness.
                tracing::error!("rules disabled: {e}");
                Default::default()
            });
            tracing::info!("loaded {} automation rule(s)", rules.rules.len());
            let mut automation = Automation::new(rules);

            // Ends when every sender is dropped, i.e. at shutdown.
            for cmd in rx {
                match cmd {
                    Command::List(reply) => {
                        let _ = reply.send(engine.list());
                    }
                    Command::Get {
                        display,
                        code,
                        reply,
                    } => {
                        let _ = reply.send(engine.get(&display, &code));
                    }
                    Command::Set {
                        display,
                        code,
                        value,
                        verify,
                        reply,
                    } => {
                        let _ = reply.send(engine.set(&display, &code, value, verify));
                    }
                    Command::Caps { display, reply } => {
                        let _ = reply.send(engine.caps(&display));
                    }
                    Command::ProfileList(reply) => {
                        let _ = reply.send(engine.profile_list());
                    }
                    Command::ProfileApply {
                        name,
                        verify,
                        yes,
                        reply,
                    } => {
                        let _ = reply.send(engine.profile_apply(&name, verify, yes));
                    }
                    Command::ProfileSave {
                        name,
                        codes,
                        force,
                        reply,
                    } => {
                        let _ = reply.send(engine.profile_save(&name, &codes, force));
                    }
                    Command::ProfileDelete { name, reply } => {
                        let _ = reply.send(engine.profile_delete(&name));
                    }
                    Command::ProfileShow { name, reply } => {
                        let _ = reply.send(engine.profile_show(&name));
                    }
                    Command::AutomationTick(reply) => {
                        let _ = reply.send(automation_tick(&mut engine, &mut automation));
                    }
                    Command::RulesList(reply) => {
                        let _ = reply.send(Ok(rule_infos(automation.rules())));
                    }
                    Command::RulesReload(reply) => {
                        let _ = reply.send(match rules_store.load() {
                            Ok(set) => {
                                let infos = rule_infos(&set);
                                tracing::info!("reloaded {} rule(s)", set.rules.len());
                                automation.replace(set);
                                Ok(infos)
                            }
                            Err(e) => Err(EngineError::Rules(e)),
                        });
                    }
                }
            }
            tracing::info!("i2c worker exiting");
        })
        .expect("failed to spawn i2c worker");

    WorkerHandle { tx }
}
