---
description: Commit staged changes with a Conventional Commits message
model: opencode/deepseek-v4.1-flash-free
---

Create a commit from staged changes only. Review the changes, detect anomalies, validate the commit message, obtain
explicit user confirmation, and commit without pushing.

Optional user input: `$ARGUMENTS`

Treat provided arguments as context for the commit message, such as intent, scope, or issue references. They do not
override repository rules or safety checks.

## Operating Rules

- Never stage, unstage, edit, or discard files.
- Never push, amend, rebase, or create additional commits.
- Never bypass hooks, secret scanning, or commit signing.
- Never expose secret values in summaries, reports, or tool output.
- Never commit without explicit user confirmation.
- Stop when a required check fails, its result is uncertain, or a required hook is unavailable.
- Use the repository's actual Git configuration and hook scripts as the source of truth for mechanically enforced rules.
- Do not claim that a check passed unless its successful result was observed.

## Step 1: Verify Staged Changes

Run:

```bash
git diff --cached --quiet
```

Interpret the exit code:

- `0`: No staged changes. Report that nothing is staged and stop.
- `1`: Staged changes exist. Continue.
- Any other code: Report the Git error and stop.

Then inspect:

```bash
git status --short
git diff --cached --stat
```

Only staged changes are candidates for this commit. Unstaged and untracked files must not be included.

## Step 2: Check Hooks and Scan for Anomalies

Verify that both required scripts exist:

- `.githooks/pre-commit`
- `.githooks/commit-message`

If either is missing, report the missing file, provide the setup guidance below, and stop. Do not create or modify hooks
automatically.

Before executing the secret scanner, ensure its output does not disclose secret values. If this cannot be established,
stop rather than expose potentially sensitive output.

Run the pre-commit scanner:

```bash
bash .githooks/pre-commit
```

Interpret the result according to the script's documented exit codes:

- Successful scan with no findings: continue.
- Secret or credential findings: report only safe file paths, line numbers, and finding categories. Do not reveal secret
  values. Stop before reading the staged diff.
- Scanner failure, ambiguous output, or an undocumented exit code: stop and report that verification could not be
  completed.

Do not assume every nonzero exit code represents a detected secret. Follow the hook's actual behavior.

If findings exist, recommend that the user inspect and fix the affected files or unstage them using
`git restore --staged <file>`. A `secret-scan:ignore` exception is acceptable only for a verified false positive
supported by the scanner's documented mechanism. Never make these changes yourself.

If the scan passes, check for staged files larger than 1 MiB. Inspect only files that remain in the staged index,
including their staged blob sizes rather than relying solely on working-tree file sizes. Handle filenames containing
spaces or unusual characters safely.

Report large files as anomalies; do not automatically reject them unless repository policy requires it.

## Step 3: Review the Staged Changes

After the secret scan passes, inspect:

```bash
git diff --cached --find-renames
git diff --cached --stat
git log -5 --oneline
```

Review the staged diff to determine:

- The primary intent of the changes.
- The affected components and existing scope conventions.
- Whether the changes introduce bugs, security concerns, accidental files, generated artifacts, or unrelated
  modifications.
- Whether the changes combine independent concerns that should be committed separately.
- Whether a breaking API or behavior change is actually present.

Do not infer implementation details that the diff does not support. If the diff is too large to review reliably, inspect
it in manageable sections and report any remaining review limitations.

Do not display secret values discovered during review. If a potential secret is discovered after the initial scan, stop
and report it safely.

## Step 4: Draft and Validate the Commit Message

Use Conventional Commits 1.0.0:

```text
<type>[optional scope][!]: <description>

[optional body]

[optional footer(s)]
```

### Types

| Type       | Purpose                                         |
|------------|-------------------------------------------------|
| `feat`     | New feature or capability                       |
| `fix`      | Bug fix                                         |
| `docs`     | Documentation only                              |
| `style`    | Formatting without logic changes                |
| `refactor` | Restructuring without intended behavior changes |
| `perf`     | Performance improvement                         |
| `test`     | Adding or correcting tests                      |
| `build`    | Build system or dependency changes              |
| `ci`       | CI configuration and scripts                    |
| `chore`    | Other maintenance                               |
| `revert`   | Reverting a previous commit                     |

Choose the type based on the primary intent of the staged changes. Do not select a type merely because a particular file
extension suggests it.

### Message Rules

- Use a lowercase noun for an optional scope, following existing project conventions.
- Start the description with an imperative verb in the present tense.
- Start with a lowercase letter unless a proper noun or acronym requires otherwise.
- Describe the meaningful change and, when useful, its purpose.
- Do not list filenames, use emojis, or add a trailing period to the subject.
- Add a body only when additional context is useful. Explain why the change is needed rather than repeating the subject.
- Add footers only when justified, such as `Refs: #123` when the user supplied the issue reference.
- Mark a breaking change with `!` before the colon or a `BREAKING CHANGE:` footer when appropriate.
- Do not claim a breaking change unless the staged changes support that conclusion.

### Length Limits

Every subject, body, and footer line must contain at most 75 characters, including spaces and punctuation.

The subject length includes the type, scope, optional `!`, colon, and separating space.

Wrap long body and footer text without breaking words unnecessarily. Never truncate the subject mid-word or remove the
type to satisfy the limit.

### Validate Through the Repository Hook

Do not reimplement mechanical validation in the command. Validate the drafted message using `.githooks/commit-message`:

```bash
tmp=$(mktemp) || exit 1
trap 'rm -f "$tmp"' EXIT

cat > "$tmp" <<'EOF'
<drafted commit message>
EOF

bash .githooks/commit-message "$tmp"
```

Replace the placeholder with the actual draft.

If validation fails, inspect the error, correct the message, and validate again. Do not present an invalid message for
approval.

If the hook requires a particular blank-line convention, follow the repository's implementation. Do not assume that a
body is mandatory under Conventional Commits.

If the hook is missing, fails to execute, or cannot establish validity, stop. Never fall back to an unvalidated commit.

The hook validates mechanical constraints; you remain responsible for semantic accuracy, appropriate type and scope,
imperative wording, and usefulness.

## Step 5: Present the Review and Request Confirmation

Present the following report after all required checks pass:

### Staged Changes Summary

Provide one or two sentences describing the primary intent.

### Files Staged

| File | Status                               | Description       |
|------|--------------------------------------|-------------------|
| Path | Added / Modified / Deleted / Renamed | Brief description |

Include all staged paths. Use Git's staged status and rename information rather than inferring status from filenames.

### Anomaly Check

- Large files over 1 MiB: list or `None`
- Potential secrets or credentials: safe findings or `None`
- Scanner status: `Passed`, `Findings`, or `Not verified`
- Other anomalies: findings or `None`
- Review limitations: disclose any relevant limitations

Do not report the scanner as passed if it was skipped or failed.

### Proposed Commit Message

Show the complete validated message, including any body and footers.

Report the subject length as `N/75 characters`. Do not include the body in the subject-length count.

If the staged changes contain unrelated concerns, recommend separate commits and explain why. Do not alter the staging
area.

Use the available interactive question mechanism to offer:

- **Proceed** — commit using the exact validated message.
- **Cancel** — abort without changing the repository.
- **Edit message** — obtain the revised message, validate it again, and request confirmation again.

Wait for an explicit choice. If interactive confirmation is unavailable, stop and request confirmation through the
supported interaction mechanism. Never assume approval.

If the user edits the message, validate the entire revised message and present it again before committing. If validation
fails, explain the violation and suggest a compliant alternative.

## Step 6: Commit the Approved Message

Immediately before committing, verify that the staged changes still match the reviewed changes. If they have changed,
stop and repeat the review and confirmation process.

Check the effective Git signing configuration:

```bash
git config --show-origin --get commit.gpgsign
git config --show-origin --get gpg.format
git config --show-origin --get user.signingkey
git config --show-origin --get core.hooksPath
```

Some settings may be unset. Do not interpret an unset value as a command failure or silently modify the configuration.

Determine whether the required hooks will actually run. Git's default hook directory is `.git/hooks`; `.githooks` is
used only when configured or invoked explicitly. Account for valid absolute paths and applicable configuration
precedence.

If the effective hooks directory does not activate `.githooks`, warn the user before proceeding and provide the setup
commands below. Do not claim that Git will execute hooks automatically when it will not.

Never disable signing or bypass hooks. If the user's signing configuration requires an unavailable key or agent, stop
and explain the problem.

Commit only the approved message, preserving multiline content exactly:

```bash
git commit -F - <<'EOF'
<approved commit message>
EOF
```

Use the exact message that passed validation and received confirmation.

If the commit command fails:

- Show the relevant error and hook output.
- Redact any secret values before reporting output.
- Do not retry automatically.
- For a hook rejection, explain the failure and ask the user whether to fix and retry or abort.
- Never bypass a rejected hook without explicit user authorization.
- Never retry with `SKIP_SECRET_CHECK=1` on your own.
- If signing waits for a passphrase or key interaction, stop the command when possible, explain the situation, and do
  not retry automatically.

If the result is ambiguous, inspect Git's state before claiming that a commit was or was not created.

On success, report:

```text
Committed: <commit hash> — <commit subject>
```

Confirm that no push was performed.

## Hook Setup

For a repository whose hook scripts are already committed, the user can run:

```bash
chmod +x .githooks/commit-message .githooks/pre-commit
git config core.hooksPath .githooks
```

Explain that `core.hooksPath` changes the effective hook directory for the repository. Do not run these commands
automatically.

## Final Constraints

- Commit staged changes only.
- Never stage or unstage files.
- Never edit or discard files.
- Never push.
- Never amend or rewrite existing commits.
- Never bypass hooks, secret scanning, or signing.
- Never commit an unvalidated message.
- Never commit without explicit user confirmation.
- Never expose secret values.
- Stop on failed or uncertain required checks.
- Report only actions and results that actually occurred.