//! JSON-RPC dispatch.

use crate::worker::{Command, WorkerHandle};
use crate::DAEMON_VERSION;
use display_api::protocol::*;
use display_ddc::vcp::{self, VcpCode};
use serde_json::json;

/// Codes captured by `profiles.save` when the caller names none.
///
/// Deliberately small: these are what monitors most commonly *honour*, and a
/// snapshot of codes a display ignores would produce a profile that silently
/// does nothing on apply.
pub const DEFAULT_SNAPSHOT_CODES: &[VcpCode] = &[
    VcpCode::Brightness,
    VcpCode::Contrast,
    VcpCode::Volume,
    VcpCode::RedGain,
    VcpCode::GreenGain,
    VcpCode::BlueGain,
];

fn resolve_codes(names: &[String]) -> Result<Vec<VcpCode>, String> {
    if names.is_empty() {
        return Ok(DEFAULT_SNAPSHOT_CODES.to_vec());
    }
    names
        .iter()
        .map(|n| vcp::parse_code(n).ok_or_else(|| format!("unknown VCP code {n:?}")))
        .collect()
}

/// Handle one request. Never panics on bad input: a malformed frame from any
/// client must not be able to take the daemon down.
pub async fn dispatch(worker: &WorkerHandle, req: Request) -> Response {
    let id = req.id;

    macro_rules! params {
        ($t:ty) => {
            match req.params.clone().map(serde_json::from_value::<$t>) {
                Some(Ok(p)) => p,
                Some(Err(e)) => return Response::err(id, INVALID_PARAMS, e.to_string()),
                None => return Response::err(id, INVALID_PARAMS, "missing params"),
            }
        };
    }

    match req.method.as_str() {
        method::VERSION => Response::ok(
            id,
            json!(VersionInfo {
                protocol: PROTOCOL_VERSION,
                daemon: DAEMON_VERSION.to_string(),
            }),
        ),

        method::LIST => match worker.send(Command::List).await {
            Ok(v) => Response::ok(id, json!(v)),
            Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
        },

        method::GET => {
            let p = params!(GetParams);
            let r = worker
                .send(|reply| Command::Get {
                    display: p.display,
                    code: p.code,
                    reply,
                })
                .await;
            match r {
                Ok(v) => Response::ok(id, json!(v)),
                Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        method::SET => {
            let p = params!(SetParams);
            let r = worker
                .send(|reply| Command::Set {
                    display: p.display,
                    code: p.code,
                    value: p.value,
                    verify: p.verify,
                    reply,
                })
                .await;
            match r {
                Ok(v) => Response::ok(id, json!(v)),
                Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        method::CAPS => {
            let p = params!(CapsParams);
            let r = worker
                .send(|reply| Command::Caps {
                    display: p.display,
                    reply,
                })
                .await;
            match r {
                Ok(v) => Response::ok(id, json!(v)),
                Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        method::PROFILE_LIST => match worker.send(Command::ProfileList).await {
            Ok(v) => Response::ok(id, json!(v)),
            Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
        },

        method::PROFILE_APPLY => {
            let p = params!(ProfileApplyParams);
            let r = worker
                .send(|reply| Command::ProfileApply {
                    name: p.name,
                    verify: p.verify,
                    yes: p.yes,
                    reply,
                })
                .await;
            match r {
                Ok(v) => Response::ok(id, json!(v)),
                Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        method::PROFILE_SAVE => {
            let p = params!(ProfileSaveParams);
            let codes = match resolve_codes(&p.codes) {
                Ok(c) => c,
                Err(e) => return Response::err(id, INVALID_PARAMS, e),
            };
            let r = worker
                .send(|reply| Command::ProfileSave {
                    name: p.name,
                    codes,
                    force: p.force,
                    reply,
                })
                .await;
            match r {
                Ok(v) => Response::ok(id, json!(v)),
                Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        method::PROFILE_DELETE => {
            let p = params!(ProfileNameParams);
            let r = worker
                .send(|reply| Command::ProfileDelete {
                    name: p.name,
                    reply,
                })
                .await;
            match r {
                Ok(()) => Response::ok(id, json!({ "ok": true })),
                Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        method::PROFILE_SHOW => {
            let p = params!(ProfileNameParams);
            let r = worker
                .send(|reply| Command::ProfileShow {
                    name: p.name,
                    reply,
                })
                .await;
            match r {
                Ok(v) => Response::ok(id, json!(v)),
                Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        method::RULES_LIST => match worker.send(Command::RulesList).await {
            Ok(v) => Response::ok(id, json!(v)),
            Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
        },

        method::RULES_RELOAD => match worker.send(Command::RulesReload).await {
            Ok(v) => Response::ok(id, json!(v)),
            Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
        },

        // Exposed so the CLI can force an evaluation rather than waiting for the
        // next poll — the difference between testing a rule and guessing.
        method::RULES_TICK => match worker.send(Command::AutomationTick).await {
            Ok(v) => Response::ok(id, json!(v)),
            Err(e) => Response::err(id, INTERNAL_ERROR, e.to_string()),
        },

        other => Response::err(id, METHOD_NOT_FOUND, format!("unknown method {other:?}")),
    }
}
