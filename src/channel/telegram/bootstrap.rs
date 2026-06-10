//! Telegram bootstrap — init_from_config, attach_registry, resolve_fleet_binding.

use crate::agent::AgentRegistry;
use crate::channel::telegram::inbound::*;
use crate::channel::telegram::state::*;
use crate::channel::telegram::topic_registry::*;
use crate::fleet::ChannelConfig;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use teloxide::prelude::Requester;

/// Wire the in-process [`AgentRegistry`] into an already-initialized
/// [`TelegramState`].
pub fn attach_registry(state: &Arc<Mutex<TelegramState>>, registry: AgentRegistry) {
    let mut s = lock_state(state);
    s.registry = Some(registry);
}

/// Initialize Telegram from fleet config.
pub fn init_from_config(
    config: &crate::fleet::FleetConfig,
    home: &Path,
    submit_keys: HashMap<String, String>,
) -> Option<Arc<Mutex<TelegramState>>> {
    let (bot_token_env, group_id, user_allowlist, fleet_binding) = match config.channel.as_ref()? {
        ChannelConfig::Telegram {
            bot_token_env,
            group_id,
            user_allowlist,
            fleet_binding,
            ..
        } => (bot_token_env, group_id, user_allowlist, fleet_binding),
        ChannelConfig::Discord { .. } => return None,
    };
    let token = match std::env::var(bot_token_env) {
        Ok(t) => t,
        Err(_) => match std::env::var("AGEND_BOT_TOKEN") {
            Ok(t) => {
                tracing::warn!(
                    "AGEND_BOT_TOKEN is deprecated — migrate to {bot_token_env} in fleet.yaml"
                );
                t
            }
            Err(_) => {
                tracing::info!(env = %bot_token_env, "bot token env not set, skipping");
                return None;
            }
        },
    };
    match user_allowlist {
        None => tracing::warn!(
            "telegram channel.user_allowlist is not set — Sprint 21 Phase 2 fail-closed default: \
             ALL inbound messages and outbound notifications are dropped. \
             Set `user_allowlist: [123, 456]` in fleet.yaml to enable the channel \
             (see docs/USAGE.md \"Channel: Telegram\" migration section)."
        ),
        Some(list) if list.is_empty() => {
            tracing::info!(
                "telegram channel.user_allowlist is empty — all inbound messages will be rejected"
            )
        }
        Some(list) => tracing::info!(count = list.len(), "telegram user_allowlist active"),
    }
    let allowlist = user_allowlist.clone();

    // Clean up orphaned topics
    let mut reg = load_topic_registry(home);
    let instance_names: std::collections::HashSet<&String> = config.instances.keys().collect();
    let mut orphan_count = 0;
    let mut stale_general = false;
    for (tid, inst_name) in reg.clone() {
        if inst_name == FLEET_BINDING_SENTINEL || instance_names.contains(&inst_name) {
            continue;
        }
        if tid == 1 {
            // General can never be deleted on Telegram (TOPIC_ID_INVALID) —
            // a stale binding is registry-only cleanup, freeing the topic for
            // a configured claimant (t-20260610083024814227-0).
            tracing::info!(instance = %inst_name, "stale General-topic binding (instance gone) — dropping registry entry");
            reg.remove(&tid);
            stale_general = true;
        } else {
            tracing::info!(topic_id = tid, instance = %inst_name, "orphaned topic, deleting");
            delete_topic(home, tid);
            orphan_count += 1;
        }
    }
    if orphan_count > 0 {
        reg = load_topic_registry(home);
        if stale_general {
            // delete_topic reloads from disk — re-apply the registry-only drop.
            reg.remove(&1);
        }
        tracing::info!(count = orphan_count, "cleaned up orphaned topics");
    }

    let bot = teloxide::Bot::new(&token);
    let chat_id = teloxide::types::ChatId(*group_id);

    // t-20260610083024814227-0: planning extracted to a pure fn so the
    // General-claim rules are unit-testable (see general_topic_plan_tests).
    let (mut topic_map, general_claimant, to_create) = plan_topic_assignments(config, &reg);

    // Auto-create topics for instances without topic_id.
    //
    // Sprint 59 Wave 2 PR-IMPL (F2 — α-a' track-on-create refactor):
    // route through `create_topic_for_instance` instead of inline
    // `bot.create_forum_topic`. This closes S1 (duplicate accumulation
    // on registry-state loss): `create_topic_for_instance` at
    // topic_registry.rs:74-79 already implements idempotent same-
    // name reuse — if a topic with the same instance name is in
    // `topics.json`, it returns the existing topic_id rather than
    // calling the create API again. Bootstrap now benefits from
    // the same dedup. Combined with the existing orphan-cleanup at
    // bootstrap.rs:71-78 (which scans topics.json for retired
    // instances), the (α-a)+(α-b) pair from RCA design collapses
    // into a single track-on-create flow + existing orphan scan.
    //
    // Note: chat-side enumeration to detect "duplicate-named topic
    // already exists in chat but not in topics.json" remains
    // technically impossible per teloxide 0.11.2 + Telegram Bot API
    // gap (no list_forum_topics method). That edge case requires
    // operator intervention via the (γ) `agend-terminal doctor
    // topics` surface (Sprint 60+ candidate: teloxide upgrade
    // evaluation if a future Bot API version exposes enumeration).
    if let Some(name) = general_claimant {
        // Config-driven General binding (explicit `topic_id: 1`, or the
        // legacy literal name "general"). Outbound already handles id 1 by
        // omitting message_thread_id; nothing to create on Telegram's side —
        // the General topic always exists and cannot be deleted.
        tracing::info!(instance = %name, "binding instance to the permanent General topic (id 1)");
        topic_map.insert(name.clone(), 1);
        if let Err(e) = register_topic(home, 1, &name) {
            tracing::warn!(error = %e, instance = %name, "failed to register General-topic binding");
        }
    }
    for name in &to_create {
        tracing::info!(instance = %name, "auto-creating topic via track-on-create");
        if let Some(tid) = create_topic_for_instance(home, name) {
            topic_map.insert(name.clone(), tid);
        }
    }

    // Ensure topic registry reflects any auto-created entries
    for (name, tid) in &topic_map {
        reg.insert(*tid, name.clone());
    }
    if let Err(e) = save_topic_registry(home, &reg) {
        tracing::warn!(error = %e, "failed to save topic registry");
    }

    let fleet_binding_topic_id =
        resolve_fleet_binding(&bot, chat_id, home, &mut reg, fleet_binding);

    let mut raw_state = TelegramState::new(
        &token,
        *group_id,
        topic_map,
        home.to_path_buf(),
        submit_keys,
        allowlist,
    );
    raw_state.fleet_binding_topic_id = fleet_binding_topic_id;
    let state = Arc::new(Mutex::new(raw_state));
    start_polling(Arc::clone(&state));
    Some(state)
}

/// Pure planning for the bootstrap topic-assignment pass
/// (t-20260610083024814227-0: config-driven General-topic binding).
///
/// Returns `(topic_map, general_claimant, to_create)`:
/// - `topic_map` — instance → topic id for every binding the registry already
///   carries, INCLUDING the permanent General topic (id 1) when its bound
///   instance still exists in the fleet. The previous code filtered id 1 out
///   unconditionally, which evicted any non-"general"-named instance from
///   General at every boot and fed the 2026-06-10 duplicate-topic cascade.
/// - `general_claimant` — the instance to bind to General when nobody holds
///   it: an explicit `topic_id: 1` in fleet.yaml wins (lexicographically
///   first if several claim, with a warn); the legacy hardcoded instance
///   NAME "general" stays as a fallback so existing deployments keep their
///   behavior.
/// - `to_create` — instances with no binding → track-on-create.
///
/// A registry General entry pointing at an instance no longer in the fleet is
/// stale: it is dropped from the returned map (the caller's registry save
/// rewrites it away). Unlike ordinary orphans there is no `delete_topic` to
/// call — Telegram's General topic cannot be deleted (TOPIC_ID_INVALID).
pub(super) fn plan_topic_assignments(
    config: &crate::fleet::FleetConfig,
    reg: &HashMap<i32, String>,
) -> (HashMap<String, i32>, Option<String>, Vec<String>) {
    // Registry bindings load as-is — including General (id 1), but only when
    // its bound instance still exists in the fleet (a stale General binding
    // must yield so a configured claimant can take over).
    let topic_map: HashMap<String, i32> = reg
        .iter()
        .filter(|(tid, name)| {
            name.as_str() != FLEET_BINDING_SENTINEL
                && (**tid != 1 || config.instances.contains_key(name.as_str()))
        })
        .map(|(tid, name)| (name.clone(), *tid))
        .collect();

    let general_taken = topic_map.values().any(|t| *t == 1);
    let general_claimant = if general_taken {
        None
    } else {
        // Explicit `topic_id: 1` wins; lexicographic order makes the winner
        // deterministic across boots (HashMap iteration order is not).
        let mut explicit: Vec<&String> = config
            .instances
            .iter()
            .filter(|(name, inst)| {
                inst.topic_id == Some(1) && !topic_map.contains_key(name.as_str())
            })
            .map(|(name, _)| name)
            .collect();
        explicit.sort();
        if explicit.len() > 1 {
            tracing::warn!(
                claimants = ?explicit,
                "multiple instances claim the General topic (topic_id: 1) — first (sorted) wins"
            );
        }
        explicit
            .first()
            .map(|s| (*s).to_string())
            // Legacy fallback: an instance literally named "general" keeps
            // its historical General-topic home when nobody claims it.
            .or_else(|| {
                config
                    .instances
                    .contains_key("general")
                    .then(|| "general".to_string())
                    .filter(|n| !topic_map.contains_key(n.as_str()))
            })
    };

    let to_create: Vec<String> = config
        .instances
        .keys()
        .filter(|n| {
            !topic_map.contains_key(n.as_str()) && Some(n.as_str()) != general_claimant.as_deref()
        })
        .cloned()
        .collect();
    (topic_map, general_claimant, to_create)
}

/// Resolve the `fleet_binding` block to a concrete Telegram forum topic id.
pub(super) fn resolve_fleet_binding(
    bot: &teloxide::Bot,
    chat_id: teloxide::types::ChatId,
    home: &Path,
    reg: &mut HashMap<i32, String>,
    fleet_binding: &Option<crate::fleet::FleetBindingConfig>,
) -> Option<i32> {
    let name = match fleet_binding.as_ref()? {
        crate::fleet::FleetBindingConfig::Struct(crate::fleet::FleetBindingStruct::Topic {
            name,
        }) => name.clone(),
        crate::fleet::FleetBindingConfig::Shorthand(raw) => {
            tracing::warn!(
                shorthand = %raw,
                "telegram channel.fleet_binding shorthand ignored — Telegram requires \
                 `{{type: topic, name: ...}}` (shorthand is Discord/Slack only). \
                 Fleet events will not be mirrored on this channel."
            );
            return None;
        }
    };

    // Fast path: previously-resolved topic still present in registry.
    for (tid, inst) in reg.iter() {
        if inst == FLEET_BINDING_SENTINEL {
            tracing::info!(topic_id = *tid, %name, "reusing existing fleet_binding topic");
            return Some(*tid);
        }
    }

    // Slow path: create the forum topic once and pin it into the registry.
    tracing::info!(%name, "creating fleet_binding topic");
    match block_on_value(async { bot.create_forum_topic(chat_id, &name).await }) {
        Ok(topic) => {
            let tid = topic.thread_id.0 .0;
            tracing::info!(topic_id = tid, %name, "created fleet_binding topic");
            reg.insert(tid, FLEET_BINDING_SENTINEL.to_string());
            let _ = save_topic_registry(home, reg);
            Some(tid)
        }
        Err(e) => {
            tracing::error!(error = %e, %name, "failed to create fleet_binding topic");
            None
        }
    }
}

#[cfg(test)]
mod general_topic_plan_tests {
    use super::*;

    fn cfg(yaml: &str) -> crate::fleet::FleetConfig {
        serde_yaml_ng::from_str(yaml).expect("test fleet yaml")
    }

    fn reg(pairs: &[(i32, &str)]) -> HashMap<i32, String> {
        pairs.iter().map(|(t, n)| (*t, n.to_string())).collect()
    }

    /// The operator-facing contract: `topic_id: 1` in fleet.yaml binds that
    /// instance to the permanent General topic — no new topic is created.
    #[test]
    fn explicit_topic_id_1_claims_general() {
        let config = cfg("instances:\n  AgendTerminal:\n    topic_id: 1\n  Other: {}\n");
        let (_map, claimant, to_create) = plan_topic_assignments(&config, &HashMap::new());
        assert_eq!(
            claimant.as_deref(),
            Some("AgendTerminal"),
            "explicit topic_id: 1 must claim the General topic"
        );
        assert!(
            !to_create.contains(&"AgendTerminal".to_string()),
            "the General claimant must NOT get a track-on-create topic"
        );
        assert!(to_create.contains(&"Other".to_string()));
    }

    /// Legacy behavior preserved: an instance literally named "general"
    /// claims General when nobody claims it explicitly.
    #[test]
    fn legacy_general_name_still_claims() {
        let config = cfg("instances:\n  general: {}\n");
        let (_map, claimant, _) = plan_topic_assignments(&config, &HashMap::new());
        assert_eq!(claimant.as_deref(), Some("general"));
    }

    /// Explicit config beats the legacy name when both are present.
    #[test]
    fn explicit_claimant_beats_legacy_name() {
        let config = cfg("instances:\n  general: {}\n  AgendTerminal:\n    topic_id: 1\n");
        let (_map, claimant, to_create) = plan_topic_assignments(&config, &HashMap::new());
        assert_eq!(
            claimant.as_deref(),
            Some("AgendTerminal"),
            "explicit topic_id: 1 must take precedence over the legacy name"
        );
        assert!(
            to_create.contains(&"general".to_string()),
            "the displaced legacy instance gets an ordinary topic instead"
        );
    }

    /// An EXISTING registry binding to General must be honored — the
    /// pre-fix filter dropped it, evicting the instance every boot (the
    /// 2026-06-10 cascade).
    #[test]
    fn registry_general_binding_survives_for_live_instance() {
        let config = cfg("instances:\n  AgendTerminal:\n    topic_id: 1\n");
        let (map, claimant, to_create) =
            plan_topic_assignments(&config, &reg(&[(1, "AgendTerminal")]));
        assert_eq!(
            map.get("AgendTerminal"),
            Some(&1),
            "an existing General binding must load into the topic map"
        );
        assert_eq!(claimant, None, "already bound — nothing to claim");
        assert!(to_create.is_empty(), "bound instance must not be recreated");
    }

    /// A STALE registry General binding (instance gone from the fleet) is
    /// dropped from the map so a configured claimant can take over —
    /// without any delete_topic call (General is undeletable on Telegram).
    #[test]
    fn stale_general_binding_yields_to_new_claimant() {
        let config = cfg("instances:\n  AgendTerminal:\n    topic_id: 1\n");
        let (map, claimant, _) = plan_topic_assignments(&config, &reg(&[(1, "ghost")]));
        assert!(
            !map.values().any(|t| *t == 1),
            "stale General binding must not occupy the topic map"
        );
        assert_eq!(claimant.as_deref(), Some("AgendTerminal"));
    }

    /// Multiple explicit claimants: deterministic winner (lexicographic),
    /// the rest fall through to track-on-create.
    #[test]
    fn multiple_explicit_claimants_first_sorted_wins() {
        let config = cfg("instances:\n  Zeta:\n    topic_id: 1\n  Alpha:\n    topic_id: 1\n");
        let (_map, claimant, to_create) = plan_topic_assignments(&config, &HashMap::new());
        assert_eq!(claimant.as_deref(), Some("Alpha"));
        assert!(to_create.contains(&"Zeta".to_string()));
    }
}
