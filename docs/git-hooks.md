# Local Git hooks

Enable the repository's Git hooks once in your checkout:

```sh
sh scripts/install-git-hooks.sh
```

- **Pre-commit:** reject whitespace errors in the *staged patch* using `git diff --cached --check`. It never reformats or stages files on your behalf.
- **Pre-push:** run the existing `./bazel test //:fast --test_output=errors` gate. The complete `//:test` acceptance gate remains in CI.

Use `./bazel run //:fix` when you want to format source deliberately; review and stage those changes yourself. Bazel provisions the project's language tools, so the hooks do not introduce another formatter or dependency manager.

To bypass the pre-push gate for a specific push (for example, when the same commit was already validated):

```sh
PULSEBEAM_SKIP_PUSH_CHECKS=1 git push
```

Use `git -c core.hooksPath=/dev/null commit ...` only when intentionally bypassing commit-time checks. Hooks are a local convenience, not security or CI enforcement. To disable the setup, run `git config --local --unset core.hooksPath`.

The installer refuses to replace an existing custom `core.hooksPath`. For linked worktrees, Git's local configuration may be shared; the relative `.githooks` path resolves from each checkout's working-tree root.
