# Mail CLI Operator

Use this skill whenever the user asks ZeroClaw to send, verify, inspect, or re-send email through the configured mail account. Prefer this skill over browser email whenever the `mail` tool is available.

## Mission

Handle email through first-party mail tooling and prove the send happened during the current attempt.

## Fixed Workflow

1. Start with a dry-run plan before acting.
2. State the recipient, subject/body intent, proof target, and estimated tool turns.
3. If the estimate is greater than `12`, ask for permission before acting.
4. If the run overruns the estimate without fresh proof, stop, explain why, give a revised estimate, and ask whether to continue.

## Preferred Tool Path

Use the `mail` tool first:

- `send` to create the message
- `verify_sent` immediately after to produce fresh proof
- `list_recent` or `get` only for inspection or debugging

The result is complete only after `verify_sent` confirms the message from the current attempt.

## Fresh-Proof Rule

Historical messages, previous sends, inbox screenshots, and memory recalls are not proof for the current attempt.

Completion requires a fresh proof artifact from this attempt:

- preferred: verified `message_id`
- acceptable: fresh Sent-folder confirmation tied to the same `message_id`

If the user asks to send "again," require a second verified `message_id`. Never reuse a prior successful send as proof of a new send.

## Browser Fallback

Fall back to `browser-operator` only when:

- `mail` is unavailable
- `verify_sent` fails
- the user explicitly wants browser/webmail handling

If you fall back, say so explicitly and switch to browser-style proof rules.

## Response Style

Keep updates short and concrete.

When complete, report:

- recipient and subject
- fresh `message_id`
- how it was verified
