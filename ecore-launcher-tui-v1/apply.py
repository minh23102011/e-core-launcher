#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import os
import subprocess
import sys
from pathlib import Path

PROJECT_FALLBACK = Path.home() / "Downloads" / "project"
BASELINE_SHA = "85023ad682c257b6e2b8536cc00c5838fa8e25dd"
PATCH_NAME = "ecore-launcher-tui-v1"


def fail(message: str) -> "NoReturn":
    print(f"{PATCH_NAME}: ERROR: {message}", file=sys.stderr)
    raise SystemExit(1)


def is_project(path: Path) -> bool:
    cargo = path / "Cargo.toml"
    cli = path / "src" / "cli" / "mod.rs"
    if not cargo.is_file() or not cli.is_file():
        return False
    try:
        text = cargo.read_text(encoding="utf-8")
    except OSError:
        return False
    return 'name = "ecore-launcher"' in text


def find_project() -> Path:
    cwd = Path.cwd().resolve()
    if is_project(cwd):
        return cwd
    fallback = PROJECT_FALLBACK.expanduser().resolve()
    if is_project(fallback):
        return fallback
    fail(
        "could not find the ecore-launcher repo in the current directory or "
        f"{PROJECT_FALLBACK}"
    )


def read_text(path: Path) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        fail(f"failed to read {path}: {error}")


def atomic_write(path: Path, data: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{PATCH_NAME}.{os.getpid()}.tmp")
    if temporary.exists():
        fail(f"temporary path already exists: {temporary}")
    try:
        temporary.write_bytes(data)
        os.replace(temporary, path)
    except OSError as error:
        try:
            temporary.unlink(missing_ok=True)
        except OSError:
            pass
        fail(f"failed to write {path}: {error}")


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        fail(f"{label}: expected exactly one stable anchor, found {count}")
    return text.replace(old, new, 1)


def prepare_cargo(text: str) -> tuple[str, bool]:
    desired = 'ratatui = { version = "=0.28.1", default-features = false, features = ["crossterm"] }'
    if desired in text:
        return text, False
    if "\nratatui =" in text:
        fail("Cargo.toml already contains a different ratatui dependency; refusing to guess")
    anchor = 'rustix = { version = "=1.1.4", default-features = false, features = ["process", "std"] }\n'
    return (
        replace_once(text, anchor, anchor + desired + "\n", "Cargo.toml"),
        True,
    )


def prepare_cli_mod(text: str) -> tuple[str, bool]:
    changed = False
    if "mod tui;" not in text:
        text = replace_once(text, "mod topology;\n", "mod topology;\nmod tui;\n", "src/cli/mod.rs module list")
        changed = True
    if "use self::tui::TuiArgs;" not in text:
        text = replace_once(
            text,
            "use self::topology::TopologyArgs;\n",
            "use self::topology::TopologyArgs;\nuse self::tui::TuiArgs;\n",
            "src/cli/mod.rs imports",
        )
        changed = True
    tui_variant = "    /// Open the keyboard-only full-control terminal dashboard.\n    Tui(TuiArgs),\n"
    if "    Tui(TuiArgs)," not in text:
        anchor = "    /// Manage user-level graphical-session startup integration.\n    Startup(StartupArgs),\n"
        text = replace_once(text, anchor, anchor + tui_variant, "src/cli/mod.rs command enum")
        changed = True
    dispatch = "        Command::Tui(arguments) => tui::run(&arguments, cli.config.as_deref()),\n"
    if "Command::Tui(arguments)" not in text:
        anchor = "        Command::Startup(arguments) => startup::run(&arguments, cli.config.as_deref()),\n"
        text = replace_once(text, anchor, anchor + dispatch, "src/cli/mod.rs dispatch")
        changed = True
    return text, changed


def prepare_readme(text: str) -> tuple[str, bool]:
    if "## Terminal UI" in text:
        return text, False
    section = '''## Terminal UI

The optional keyboard-only terminal dashboard keeps the existing CLI as the
source of truth while exposing full control in one screen:

```bash
cargo run -- tui
```

The main dashboard shows the explicit application registry, current desktop
availability, detected topology/E-core set, stored launch policy, and cached
user-startup state. Keys are intentionally simple: `j`/`k` move, `Space`
enables/disables, `a` adds discovered applications, `c` configures launch
policy, `r` starts a one-shot run request, `s` starts a supervised request,
`d` opens read-only doctor diagnostics, `u` manages user startup/autostart,
`x` removes a registry entry, `f` refreshes, `?` opens help, and `q` quits.

Startup/autostart mutations always require an in-TUI confirmation. The TUI does
not run a shell, does not bypass registry/topology safety boundaries, and does
not classify or move unrelated processes. It uses the existing registry,
doctor, startup, run, and supervise behavior rather than implementing a second
policy engine. Terminals smaller than 72x20 receive a resize notice instead of
a broken layout.

'''
    marker = "### Validation and persistence\n"
    return (
        replace_once(text, marker, section + marker, "README.md TUI insertion"),
        True,
    )


def git_head(project: Path) -> str | None:
    try:
        result = subprocess.run(
            ["git", "rev-parse", "HEAD"],
            cwd=project,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
        )
    except OSError:
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def main() -> None:
    project = find_project()
    bundle_root = Path(__file__).resolve().parent
    source = bundle_root / "payload" / "src" / "cli" / "tui" / "mod.rs"
    if not source.is_file():
        fail(f"bundle payload is missing {source}")

    cargo_path = project / "Cargo.toml"
    cli_path = project / "src" / "cli" / "mod.rs"
    readme_path = project / "README.md"
    tui_path = project / "src" / "cli" / "tui" / "mod.rs"

    # Preflight every source and anchor before the first write.
    cargo_new, cargo_changed = prepare_cargo(read_text(cargo_path))
    cli_new, cli_changed = prepare_cli_mod(read_text(cli_path))
    readme_new, readme_changed = prepare_readme(read_text(readme_path))
    tui_bytes = source.read_bytes()
    tui_changed = not tui_path.exists()
    if tui_path.exists() and tui_path.read_bytes() != tui_bytes:
        fail(f"{tui_path} already exists with different contents; refusing to overwrite")

    print(f"{PATCH_NAME}: project={project}")
    head = git_head(project)
    if head:
        if head == BASELINE_SHA:
            print(f"{PATCH_NAME}: baseline HEAD verified ({head[:12]})")
        else:
            print(
                f"{PATCH_NAME}: note: HEAD is {head[:12]}, baseline was {BASELINE_SHA[:12]}; "
                "stable source anchors were validated before writing"
            )

    changes = []
    if tui_changed:
        atomic_write(tui_path, tui_bytes)
        changes.append("added src/cli/tui/mod.rs")
    if cargo_changed:
        atomic_write(cargo_path, cargo_new.encode("utf-8"))
        changes.append("updated Cargo.toml")
    if cli_changed:
        atomic_write(cli_path, cli_new.encode("utf-8"))
        changes.append("updated src/cli/mod.rs")
    if readme_changed:
        atomic_write(readme_path, readme_new.encode("utf-8"))
        changes.append("updated README.md")

    if changes:
        for change in changes:
            print(f"{PATCH_NAME}: {change}")
    else:
        print(f"{PATCH_NAME}: already applied; no changes needed")

    print(f"{PATCH_NAME}: TUI source sha256={sha256_bytes(tui_bytes)[:16]}")
    print(f"{PATCH_NAME}: apply complete")


if __name__ == "__main__":
    main()
