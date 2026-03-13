use crate::agent::classifier::{classify, classify_with_decision};
use crate::config::{PlannerExecutionConfig, QueryClassificationConfig};
use crate::providers::{ChatMessage, Provider};
use crate::util::truncate_with_ellipsis;
use anyhow::Result;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};

const PLANNER_SYSTEM_PROMPT: &str = concat!(
    "You are a planning router for an autonomous agent. ",
    "Return one JSON object only. ",
    "Do not call tools. ",
    "Do not include chain-of-thought or hidden reasoning. ",
    "Fields required: goal, is_simple, is_external_action, turn_estimate, ",
    "needs_user_confirmation, execution_brief, success_proof_expected."
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanningPath {
    Default,
    SimpleExecutor,
    PlannedExecutor,
    PlannerFallback,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PlannerExecutionBrief {
    pub goal: String,
    pub is_simple: bool,
    pub is_external_action: bool,
    pub turn_estimate: u32,
    pub needs_user_confirmation: bool,
    pub execution_brief: String,
    pub success_proof_expected: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanningOutcome {
    pub path: PlanningPath,
    pub effective_model: String,
    pub visible_summary: Option<String>,
    pub extra_system_context: Option<String>,
}

fn hint_model(hint: &str) -> String {
    format!("hint:{hint}")
}

fn route_exists(
    hint: &str,
    available_hints: &HashSet<String, impl std::hash::BuildHasher>,
) -> bool {
    available_hints.contains(hint)
}

fn fallback_classified_model(
    default_model: &str,
    query_classification: &QueryClassificationConfig,
    available_hints: &HashSet<String, impl std::hash::BuildHasher>,
    user_message: &str,
) -> String {
    if let Some(decision) = classify_with_decision(query_classification, user_message) {
        if route_exists(&decision.hint, available_hints) {
            return hint_model(&decision.hint);
        }
    }
    default_model.to_string()
}

fn render_history_digest(history: &[ChatMessage]) -> String {
    let mut digest = String::new();
    for msg in history
        .iter()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        if msg.role == "system" {
            continue;
        }
        digest.push_str("- ");
        digest.push_str(&msg.role);
        digest.push_str(": ");
        digest.push_str(&truncate_with_ellipsis(msg.content.trim(), 300));
        digest.push('\n');
    }
    digest
}

fn render_executor_context(brief: &PlannerExecutionBrief) -> String {
    format!(
        concat!(
            "[Planner execution brief]\n",
            "Goal: {goal}\n",
            "Estimated tool turns: {turn_estimate}\n",
            "Execution plan: {execution_brief}\n",
            "Fresh proof required: {success_proof_expected}\n",
            "Critical rules:\n",
            "- Prior conversation and memory are context only, not proof.\n",
            "- Only current-attempt evidence counts as success.\n",
            "- If a tool yields fresh verified proof of completion, stop immediately and report success.\n",
            "- Verification should be the last action before completion whenever possible.\n\n"
        ),
        goal = brief.goal.trim(),
        turn_estimate = brief.turn_estimate,
        execution_brief = brief.execution_brief.trim(),
        success_proof_expected = brief.success_proof_expected.trim(),
    )
}

fn render_visible_summary(brief: &PlannerExecutionBrief) -> String {
    format!(
        "Plan: {} Estimated tool turns: {}. Fresh proof expected: {}.",
        brief.execution_brief.trim(),
        brief.turn_estimate,
        brief.success_proof_expected.trim()
    )
}

fn parse_planner_brief(raw: &str) -> Option<PlannerExecutionBrief> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }

    serde_json::from_str::<PlannerExecutionBrief>(trimmed)
        .ok()
        .or_else(|| {
            let start = trimmed.find('{')?;
            let end = trimmed.rfind('}')?;
            serde_json::from_str::<PlannerExecutionBrief>(&trimmed[start..=end]).ok()
        })
}

pub async fn plan_execution(
    provider: &dyn Provider,
    history: &[ChatMessage],
    user_message: &str,
    default_model: &str,
    query_classification: &QueryClassificationConfig,
    planner_config: &PlannerExecutionConfig,
    available_hints: &HashSet<String, impl std::hash::BuildHasher>,
    temperature: f64,
) -> Result<PlanningOutcome> {
    let fallback_model = fallback_classified_model(
        default_model,
        query_classification,
        available_hints,
        user_message,
    );

    if !planner_config.enabled {
        return Ok(PlanningOutcome {
            path: PlanningPath::Default,
            effective_model: fallback_model,
            visible_summary: None,
            extra_system_context: None,
        });
    }

    if let Some(decision) = classify_with_decision(query_classification, user_message) {
        if planner_config
            .simple_hints
            .iter()
            .any(|hint| hint.eq_ignore_ascii_case(&decision.hint))
        {
            let effective_model = if route_exists(&planner_config.executor_hint, available_hints) {
                hint_model(&planner_config.executor_hint)
            } else {
                fallback_model.clone()
            };
            return Ok(PlanningOutcome {
                path: PlanningPath::SimpleExecutor,
                effective_model,
                visible_summary: None,
                extra_system_context: None,
            });
        }
    }

    if classify(&planner_config.external_action_classification, user_message).is_none() {
        return Ok(PlanningOutcome {
            path: PlanningPath::Default,
            effective_model: fallback_model,
            visible_summary: None,
            extra_system_context: None,
        });
    }

    if !route_exists(&planner_config.planner_hint, available_hints) {
        let effective_model = if route_exists(&planner_config.executor_hint, available_hints) {
            hint_model(&planner_config.executor_hint)
        } else {
            fallback_model.clone()
        };
        return Ok(PlanningOutcome {
            path: PlanningPath::PlannerFallback,
            effective_model,
            visible_summary: None,
            extra_system_context: None,
        });
    }

    let planner_input = format!(
        "Conversation context:\n{history}\nCurrent user request:\n{user_message}\n",
        history = render_history_digest(history),
    );

    let planner_raw = provider
        .chat_with_system(
            Some(PLANNER_SYSTEM_PROMPT),
            &planner_input,
            &hint_model(&planner_config.planner_hint),
            temperature.min(0.4),
        )
        .await;

    match planner_raw {
        Ok(raw) => {
            if let Some(brief) = parse_planner_brief(&raw) {
                let effective_model =
                    if route_exists(&planner_config.executor_hint, available_hints) {
                        hint_model(&planner_config.executor_hint)
                    } else {
                        fallback_model.clone()
                    };
                let show_summary = brief.turn_estimate
                    > planner_config.show_plan_when_turn_estimate_over
                    || (brief.needs_user_confirmation
                        && planner_config.show_plan_when_approval_required);
                return Ok(PlanningOutcome {
                    path: PlanningPath::PlannedExecutor,
                    effective_model,
                    visible_summary: show_summary.then(|| render_visible_summary(&brief)),
                    extra_system_context: Some(render_executor_context(&brief)),
                });
            }
        }
        Err(error) => {
            tracing::warn!(error = %error, "planner call failed; considering executor fallback");
        }
    }

    let effective_model = if route_exists(&planner_config.executor_hint, available_hints) {
        hint_model(&planner_config.executor_hint)
    } else {
        fallback_model.clone()
    };
    Ok(PlanningOutcome {
        path: PlanningPath::PlannerFallback,
        effective_model,
        visible_summary: None,
        extra_system_context: None,
    })
}

pub fn available_hint_set(
    route_model_by_hint: &HashMap<String, String, impl std::hash::BuildHasher>,
) -> HashSet<String> {
    route_model_by_hint.keys().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClassificationRule, QueryClassificationConfig};
    use crate::providers::{ChatRequest, ChatResponse, Provider};
    use async_trait::async_trait;

    struct MockProvider {
        planner_response: String,
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> Result<String> {
            Ok(self.planner_response.clone())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> Result<ChatResponse> {
            unreachable!("planner test does not call chat")
        }
    }

    #[tokio::test]
    async fn simple_hint_bypasses_planner() {
        let provider = MockProvider {
            planner_response: String::new(),
        };
        let query = QueryClassificationConfig {
            enabled: true,
            rules: vec![ClassificationRule {
                hint: "fast".into(),
                keywords: vec!["quick".into()],
                ..Default::default()
            }],
        };
        let planner = PlannerExecutionConfig {
            enabled: true,
            ..PlannerExecutionConfig::default()
        };
        let hints = HashSet::from([
            planner.planner_hint.clone(),
            planner.executor_hint.clone(),
            "fast".to_string(),
        ]);
        let outcome = plan_execution(
            &provider,
            &[ChatMessage::user("hello")],
            "quick answer please",
            "default-model",
            &query,
            &planner,
            &hints,
            0.7,
        )
        .await
        .unwrap();

        assert_eq!(outcome.path, PlanningPath::SimpleExecutor);
        assert_eq!(outcome.effective_model, "hint:deepseek-chat");
    }
}
