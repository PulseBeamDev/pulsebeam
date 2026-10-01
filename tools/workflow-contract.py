"""Guard the supported workflow against reintroducing retired orchestration."""

from pathlib import Path
import re
import sys


COMMANDS = re.compile(
    r"(?m)^\s*(?:\$\s*)?(?:just\s+|(?:brew|rustup)\s+(?:install|toolchain|component)|"
    r"cargo\s+(?:build|run|test|nextest)|pnpm\s+(?:run|build|test))"
)


def main():
    failures = []
    for name in " ".join(sys.argv[1:]).split():
        path = Path(name)
        if path.name == "Justfile":
            failures.append(f"retired workflow file: {path}")
        text = path.read_text()
        for match in COMMANDS.finditer(text):
            line = text.count("\n", 0, match.start()) + 1
            failures.append(f"{path}:{line}: retired build/setup command: {match.group().strip()}")
        if "tool: just" in text:
            failures.append(f"{path}: retired CI tool setup")
    if failures:
        print("\n".join(failures), file=sys.stderr)
        return 1
    print("Supported workflow has no retired build/setup consumers.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
