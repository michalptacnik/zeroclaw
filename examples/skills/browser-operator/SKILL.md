# Browser Operator

Use this skill whenever the user asks ZeroClaw to do logged-in browser work: post on social media, reply/comment, publish content, operate Gmail in the browser, work in an already-open tab, or "just use my browser." Trigger aggressively even when the user does not say "browser skill."

## Mission

Execute browser tasks reliably and prove the result came from the current attempt.

## Fixed Workflow

1. Start with a dry-run plan before acting.
2. State the target outcome, the proof you will require, and the estimated tool turns.
3. If the estimate is greater than `12`, ask for permission before acting.
4. If you reach the estimate without fresh proof, stop, explain why, give a revised estimate, and ask whether to continue.

## Browser Recovery Ladder

Prefer the first successful step in this order:

1. `browser` with `list_windows` to discover available windows.
2. `browser` with `focus_window` to recover Chrome or the expected browser window.
3. `browser` with `list_tabs` and `focus_tab` to recover the correct tab.
4. `browser` with `snapshot` to rebuild the page map.
5. `browser` with `find` or other semantic DOM actions.
6. `browser` OS-level computer-use actions only if semantic actions fail.

Always reacquire a fresh page map after navigation, reloads, or failed clicks.

## Fresh-Proof Rule

Historical context, old chat turns, memory recalls, and previous successes are never proof for the current attempt.

For every externally visible action, require a fresh artifact from this attempt:

- Prefer `browser` with `verify_artifact`.
- Prefer native identifiers such as post URL, post ID, comment URL, CMS URL, or message URL.
- Accept screenshot-style proof only when the platform truly offers no stronger identifier.

If the user says "again," require a second fresh artifact. Never reuse an earlier post, draft, or screenshot as proof of a new attempt.

## Social And Webmail Rules

- For social posting, comments, replies, and blog/CMS publishing, do not say "done" until you have a fresh artifact.
- For Gmail or other webmail in the browser, only use this skill when the `mail` tool is unavailable, verification fails, or the user explicitly wants browser/webmail handling.
- When a button or flow changes, stay in the same class of solution: recover window, recover tab, reacquire snapshot, retry semantically, then retry visually if needed.

## Response Style

Keep action updates short and factual.

When complete, report:

- what you did
- the fresh proof artifact
- any caveat or fallback used
