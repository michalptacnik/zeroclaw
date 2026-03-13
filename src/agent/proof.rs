use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ProofSignal {
    #[serde(default)]
    pub attempt_id: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub artifact_id: Option<String>,
    #[serde(default)]
    pub fresh: bool,
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Debug, Deserialize)]
struct ProofEnvelope {
    proof: ProofSignal,
}

pub fn extract_proof_signal(output: &str) -> Option<ProofSignal> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return None;
    }

    serde_json::from_str::<ProofEnvelope>(trimmed)
        .ok()
        .map(|envelope| envelope.proof)
        .filter(|proof| proof.fresh && proof.terminal)
        .or_else(|| {
            let start = trimmed.find('{')?;
            let end = trimmed.rfind('}')?;
            serde_json::from_str::<ProofEnvelope>(&trimmed[start..=end])
                .ok()
                .map(|envelope| envelope.proof)
                .filter(|proof| proof.fresh && proof.terminal)
        })
}

pub fn render_terminal_success_message(proof: &ProofSignal) -> String {
    let summary = proof
        .summary
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Fresh completion proof captured.");
    if let Some(artifact_id) = proof.artifact_id.as_deref() {
        format!("{summary} Proof: {artifact_id}.")
    } else {
        summary.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_terminal_proof() {
        let output = r#"{"proof":{"attempt_id":"a1","summary":"posted successfully","artifact_id":"post-123","fresh":true,"terminal":true}}"#;
        let proof = extract_proof_signal(output).expect("proof should parse");
        assert_eq!(proof.attempt_id.as_deref(), Some("a1"));
        assert_eq!(proof.artifact_id.as_deref(), Some("post-123"));
    }

    #[test]
    fn ignores_non_terminal_proof() {
        let output = r#"{"proof":{"fresh":true,"terminal":false}}"#;
        assert!(extract_proof_signal(output).is_none());
    }
}
