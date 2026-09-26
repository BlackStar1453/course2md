# Add a Codex CLI Provider instead of fixing the Codex Web Provider

The Codex Web Provider copies the Codex CLI's ChatGPT sign-in and refreshes it itself. OpenAI refresh tokens are single-use and rotate, and the Codex CLI reports "your refresh token was already used" when its copy has been spent elsewhere — so whichever program refreshes first signs the other one out, roughly every token lifetime (~8 days).

We add a separate Codex CLI Provider that runs `codex exec` and leaves the sign-in entirely to the Codex CLI, recommend it over the Web Provider, and leave the Web Provider's code untouched apart from a warning in its settings.

## Considered options

- **Make the Web Provider read `~/.codex/auth.json` every time and never refresh** — fixes the conflict for web calls, but rewrites upstream's login module (merge conflicts with `mizorewww/course2md`), and still fails whenever the token is stale and the Codex CLI hasn't run recently.
- **Hide the Web Provider** — breaks users who have a ChatGPT sign-in but no Codex CLI installed.

## Consequences

- Codex CLI calls are slower than web calls (process start-up, ~18 s per proofreading batch measured with `--ignore-user-config`), so CLI Providers run with lower default concurrency.
- Two Codex Providers exist side by side; the glossary names them explicitly.
