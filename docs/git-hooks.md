# Local Git hooks

Enable the hooks once in your checkout:

```sh
sh scripts/install-git-hooks.sh
```

Every commit runs the following sequence:

1. Require all tracked edits to be staged. Partially staged files are rejected because Bazel validates the worktree while Git commits the index.
2. Run `./bazel run //:fix`, which applies Bazel-derived machine-applicable Clippy suggestions, then formats Rust and JavaScript. Stage the resulting tracked formatting changes automatically.
3. Reject whitespace errors in the resulting staged patch.
4. Require `./bazel test //:test --keep_going --test_output=errors --zip_undeclared_test_outputs` to pass before Git creates the commit.
5. Reject any working-tree changes introduced during verification.

The full gate includes Bazel-owned Clippy, source-formatting, architectural, build and test contracts. Ambiguous or unsafe Clippy suggestions are never auto-applied: those lint errors block the commit until manually corrected.

The pre-push hook is intentionally empty: a successful commit already passed complete local acceptance. CI independently repeats the complete gate. For changes to generated input/lockfiles, run their documented update process before committing.

Git hooks are local, voluntary controls; Git can bypass them with `--no-verify` or an alternate `core.hooksPath`. Protected-branch rules and required CI checks remain necessary for enforcement. To disable local hooks, run `git config --local --unset core.hooksPath`.

The installer preserves pre-existing custom `core.hooksPath` values; relative paths work with linked worktrees.
