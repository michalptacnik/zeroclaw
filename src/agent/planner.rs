use crate::agent::classifier;
use crate::config::{PlannerExecutionConfig, QueryClassificationConfig};
use crate::providers::{ChatMessage, ChatRequest, Provider};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const PLANNER_SYSTEM_PROMPT: &str = r#"You are ZeroClaw's execution planner.

Do not solve the task. Do not call tools. Do not explain your reasoning.
Return exactly one JSON object with these fields:
- goal: string
- is_simple: boolean
- is_external_action: boolean
- turn_estimate: integer
- needs_user_confirmation: boolean
- execution_brief: string
- success_proof_expected: string

Rules:
- Output JSON only, no markdown fences.
- execution_brief must be concise and action-oriented for an executor model.
- success_proof_expected must describe the fresh proof artifact that ends the run.
- If the task is already complete or only needs a short response, set is_simple=true.
"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
pub enum PlanningPath {
    Default,
    QueryHint(String),
    SimpleHint(String),
    PlannerExecutor {
        planner_hint: String,
        executor_hint: String,
    },
    PlannerFallback {
        planner_hint: String,
        executor_hint: String,
    },
}

#[derive(Debug, Clone)]
pub struct PlanningOutcome {
    pub effective_model: String,
    pub extra_system_context: Option<String>,
    pub visible_summary: Option<String>,
    pub brief: Option<PlannerExecutionBrief>,
    pub path: PlanningPath,
}

fn route_model_for_hint(
    hint: &str,
    available_hints: &HashSet<String, impl std::hash::BuildHasher>,
) -> Option<String> {
    available_hints
        .contains(hint)
        .then(|| format!("hint:{hint}"))
}

fn parse_json_object(input: &str) -> Option<&str> {
    let trimmed = input.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return Some(trimmed);
    }

    let without_fence = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim();
    let without_fence = without_fence
        .strip_suffix("```")
        .unwrap_or(without_fence)
        .trim();
    if without_fence.starts_with('{') && without_fence.ends_with('}') {
        return Some(without_fence);
    }

    let start = trimmed.find('{')?;
    let end = trimmed.rfind('}')?;
    (start < end).then_some(&trimmed[start..=end])
}

fn parse_planner_brief(raw: &str) -> Result<PlannerExecutionBrief> {
    let json_str = parse_json_object(raw)
        .ok_or_else(|| anyhow::anyhow!("planner response did not contain a JSON object"))?;
    let mut brief: PlannerExecutionBrief = serde_json::from_str(json_str)?;
    brief.turn_estimate = brief.turn_estimate.max(1);
    Ok(brief)
}

fn render_executor_context(brief: &PlannerExecutionBrief) -> String {
    let json = serde_json::to_string_pretty(brief).unwrap_or_else(|_| "{}".into());
    format!(
        "[Planner brief]\nThis brief is internal orchestration context for this attempt. It is not proof of completion.\nUse it to execute efficiently, then stop as soon as you have a fresh verified proof artifact.\n{json}"
    )
}

fn render_visible_summary(brief: &PlannerExecutionBrief) -> String {
    format!(
        "Execution plan: {}\nEstimated turns: {}\nSuccess proof: {}",
        brief.goal, brief.turn_estimate, brief.success_proof_expected
    )
}

async fn call_planner(
    provider: &dyn Provider,
    messages: &[ChatMessage],
    planner_model: &str,
    temperature: f64,
) -> Result<PlannerExecutionBrief> {
    let mut planner_messages = Vec::with_capacity(messages.len() + 1);
    planner_messages.push(ChatMessage::system(PLANNER_SYSTEM_PROMPT));
    planner_messages.extend_from_slice(messages);
    let response = provider
        .chat(
            ChatRequest {
                messages: &planner_messages,
                tools: None,
            },
            planner_model,
            temperature,
        )
        .await?;
    parse_planner_brief(response.text_or_empty())
}

pub async fn plan_execution(
    provider: &dyn Provider,
    provider_messages: &[ChatMessage],
    user_message: &str,
    default_model: &str,
    temperature: f64,
    query_classification: &QueryClassificationConfig,
    planner_config: &PlannerExecutionConfig,
    available_hints: &HashSet<String, impl std::hash::BuildHasher>,
) -> PlanningOutcome {
    let matched_hint = classifier::classify(query_classification, user_message)
        .filter(|hint| available_hints.contains(hint));

    if let Some(simple_hint) = matched_hint
        .as_ref()
        .filter(|hint| planner_config.simple_hints.iter().any(|item| item == *hint))
    {
        return PlanningOutcome {
            effective_model: format!("hint:{simple_hint}"),
            extra_system_context: None,
            visible_summary: None,
            brief: None,
            path: PlanningPath::SimpleHint(simple_hint.clone()),
        };
    }

    let external_action = planner_config.enabled
        && classifier::classify(&planner_config.external_action_classification, user_message)
            .is_some();

    if external_action {
        let planner_model = route_model_for_hint(&planner_config.planner_hint, available_hints);
        let executor_model = route_model_for_hint(&planner_config.executor_hint, available_hints)
            .unwrap_or_else(|| default_model.to_string());

        if let Some(planner_model) = planner_model {
            match call_planner(provider, provider_messages, &planner_model, temperature).await {
                Ok(brief) => {
                    if !brief.is_external_action || brief.is_simple {
                        return PlanningOutcome {
                            effective_model: executor_model,
                            extra_system_context: None,
                            visible_summary: None,
                            brief: Some(brief),
                            path: PlanningPath::SimpleHint(planner_config.executor_hint.clone()),
                        };
                    }

                    let visible_summary = (brief.turn_estimate
                        > planner_config.show_plan_when_turn_estimate_over
                        || (brief.needs_user_confirmation
                            && planner_config.show_plan_when_approval_required))
                        .then(|| render_visible_summary(&brief));

                    return PlanningOutcome {
                        effective_model: executor_model,
                        extra_system_context: Some(render_executor_context(&brief)),
                        visible_summary,
                        brief: Some(brief),
                        path: PlanningPath::PlannerExecutor {
                            planner_hint: planner_config.planner_hint.clone(),
                            executor_hint: planner_config.executor_hint.clone(),
                        },
                    };
                }
                Err(_) if planner_config.fallback_to_executor_on_planner_error => {
                    return PlanningOutcome {
                        effective_model: executor_model,
                        extra_system_context: None,
                        visible_summary: None,
                        brief: None,
                        path: PlanningPath::PlannerFallback {
                            planner_hint: planner_config.planner_hint.clone(),
                            executor_hint: planner_config.executor_hint.clone(),
                        },
                    };
                }
                Err(_) => {
                    return PlanningOutcome {
                        effective_model: default_model.to_string(),
                        extra_system_context: None,
                        visible_summary: None,
                        brief: None,
                        path: PlanningPath::Default,
                    };
                }
            }
        }

        if planner_config.fallback_to_executor_on_planner_error {
            return PlanningOutcome {
                effective_model: executor_model,
                extra_system_context: None,
                visible_summary: None,
                brief: None,
                path: PlanningPath::PlannerFallback {
                    planner_hint: planner_config.planner_hint.clone(),
                    executor_hint: planner_config.executor_hint.clone(),
                },
            };
        }
    }

    if let Some(hint) = matched_hint {
        return PlanningOutcome {
            effective_model: format!("hint:{hint}"),
            extra_system_context: None,
            visible_summary: None,
            brief: None,
            path: PlanningPath::QueryHint(hint),
        };
    }

    PlanningOutcome {
        effective_model: default_model.to_string(),
        extra_system_context: None,
        visible_summary: None,
        brief: None,
        path: PlanningPath::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ClassificationRule;
    use crate::providers::{ChatResponse, Provider};
    use async_trait::async_trait;

    struct StaticPlannerProvider {
        response: String,
    }

    #[async_trait]
    impl Provider for StaticPlannerProvider {
        async fn chat_with_system(
            &self,
            _system_prompt: Option<&str>,
            _message: &str,
            _model: &str,
            _temperature: f64,
        ) -> Result<String> {
            Ok(self.response.clone())
        }

        async fn chat(
            &self,
            _request: ChatRequest<'_>,
            _model: &str,
            _temperature: f64,
        ) -> Result<ChatResponse> {
            Ok(ChatResponse {
                text: Some(self.response.clone()),
                tool_calls: vec![],
                usage: None,
                reasoning_content: None,
            })
        }
    }

    #[tokio::test]
    async fn simple_hint_bypasses_planner() {
        let provider = StaticPlannerProvider {
            response: "{}".into(),
        };
        let outcome = plan_execution(
            &provider,
            &[ChatMessage::user("quick help")],
            "quick help",
            "default-model",
            0.0,
            &QueryClassificationConfig {
                enabled: true,
                rules: vec![ClassificationRule {
                    hint: "fast".into(),
                    keywords: vec!["quick".into()],
                    ..Default::default()
                }],
            },
            &PlannerExecutionConfig::default(),
            &HashSet::from(["fast".to_string()]),
        )
        .await;
        assert_eq!(outcome.effective_model, "hint:fast");
        assert_eq!(outcome.path, PlanningPath::SimpleHint("fast".into()));
    }

    #[tokio::test]
    async fn planner_adds_executor_context_when_external_action_is_complex() {
        let provider = StaticPlannerProvider {
            response: serde_json::json!({
                "goal": "Post on X",
                "is_simple": false,
                "is_external_action": true,
                "turn_estimate": 14,
                "needs_user_confirmation": false,
                "execution_brief": "Open X, compose the post, verify the post URL, then stop.",
                "success_proof_expected": "Fresh X post URL"
            })
            .to_string(),
        };
        let outcome = plan_execution(
            &provider,
            &[ChatMessage::user("post hello world on x")],
            "post hello world on x",
            "default-model",
            0.0,
            &QueryClassificationConfig::default(),
            &PlannerExecutionConfig {
                enabled: true,
                ..PlannerExecutionConfig::default()
            },
            &HashSet::from(["planner".to_string(), "executor".to_string()]),
        )
        .await;
        assert_eq!(outcome.effective_model, "hint:executor");
        assert!(outcome.extra_system_context.is_some());
        assert!(outcome.visible_summary.is_some());
    }

    #[tokio::test]
    async fn planner_falls_back_to_executor_when_planner_hint_is_missing() {
        let provider = StaticPlannerProvider {
            response: "{}".into(),
        };
        let outcome = plan_execution(
            &provider,
            &[ChatMessage::user("send an email")],
            "send an email",
            "default-model",
            0.0,
            &QueryClassificationConfig::default(),
            &PlannerExecutionConfig {
                enabled: true,
                ..PlannerExecutionConfig::default()
            },
            &HashSet::from(["executor".to_string()]),
        )
        .await;
        assert_eq!(outcome.effective_model, "hint:executor");
        assert!(matches!(outcome.path, PlanningPath::PlannerFallback { .. }));
    }
}
