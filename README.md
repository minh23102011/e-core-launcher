# ecore-launcher

`ecore-launcher` is an opt-in, Linux-only application launcher intended to let
users explicitly choose ordinary desktop applications to run on reliably
detected CPU efficiency cores. It will never classify or move arbitrary running
processes, system services, kernel threads, desktop components, or unrelated
workloads.

The completed CLI core combines conservative topology detection, safe desktop
discovery, an explicit user-controlled registry, acknowledged no-shell launch,
runtime scheduling policy, verified-descendant affinity supervision, optional
user-level graphical-session startup, and read-only diagnostics. Discovery
does **not** grant management consent.

> If reliable E-core detection is unavailable, ecore-launcher will not invent
> an E-core mask or launch an unpinned fallback.

## User-controlled application registry

The registry is the opt-in boundary: a discovered application is only an
available candidate; it is not selected or managed until the user adds its
stable desktop ID. The registry retains a display-name and desktop-file
snapshot for diagnostics, but every launch re-resolves the desktop ID and does
not trust cached executable data as launch authority.

By default, the registry path is:

1. `$XDG_CONFIG_HOME/ecore-launcher/config.toml`; or
2. `$HOME/.config/ecore-launcher/config.toml` if `XDG_CONFIG_HOME` is unset or
   empty.

`--config PATH` overrides this path for registry, launch, startup, and doctor
commands. It is useful for tests and diagnostics and does not alter any other
configuration location.
`config path` prints the resolved path without creating it:

```bash
cargo run -- config path
cargo run -- list --config /tmp/ecore-launcher/config.toml
```

An absent config represents a valid, empty registry. Read-only commands never
create it. Mutating commands create the parent directory and file only after a
successful validated mutation.

### Schema and policy snapshots

The current schema version is `1`. New apps receive a **snapshot** of the
current `[launcher]` defaults; changing defaults later does not silently alter
already selected applications. `configure --reset` restores one app's stored
policy from the current defaults.

```toml
schema_version = 1

[launcher]
default_delay_seconds = 0
default_nice = 5
default_io_class = "best-effort"
default_io_priority = 4
default_enforce_process_tree = false

[[apps]]
desktop_id = "discord.desktop"
name = "Discord"
enabled = true
delay_seconds = 2
nice = 5
io_class = "best-effort"
io_priority = 4
enforce_process_tree = false
desktop_file = "/usr/share/applications/discord.desktop"
```

`desktop_id` is an identifier, never a filesystem path. Registry application
arrays are canonicalized by ID on save. Unknown TOML keys are retained where
possible for forward compatibility; comments and original formatting are not
preserved by canonical rewrites.

### Registry commands

Add one or more currently discovered applications non-interactively:

```bash
cargo run -- add discord.desktop com.spotify.Client.desktop
```

Omit IDs in a terminal to choose explicit numbered entries interactively. A
non-terminal caller must provide IDs. The add command runs the existing safe
desktop discovery subsystem; it never adds every discovered application.
Unknown IDs fail before the registry is changed, and repeated/already-selected
IDs are reported without creating duplicates.

For fixture diagnostics, discovery roots can be supplied exactly as for
`discover`:

```bash
cargo run -- add --config /tmp/ecore-launcher/config.toml \
  --data-home fixtures/desktop/data-home \
  --data-dir fixtures/desktop/data-dir-1 \
  simple.desktop
```

Inspect selected apps, including optional current discovery availability:

```bash
cargo run -- list
cargo run -- list --json
cargo run -- list --check-availability
cargo run -- show discord.desktop
cargo run -- show discord.desktop --json --check-availability
```

An unavailable application remains explicitly registered and is reported as
`unavailable`; discovery never removes, renames, or replaces it. Without
`--check-availability`, list and show report availability as unchecked and do
not scan the host desktop.

Manage only stored registry state:

```bash
cargo run -- disable discord.desktop
cargo run -- enable discord.desktop
cargo run -- configure discord.desktop \
  --delay 5 --nice 5 --io-class best-effort --io-priority 4 \
  --enforce-process-tree false
cargo run -- configure discord.desktop --reset
cargo run -- remove --yes discord.desktop
```

`remove` asks for terminal confirmation unless `--yes` is provided. Enable,
disable, and configure are idempotent. `--reset` cannot be combined with
individual settings.

Configuration inspection is read-only:

```bash
cargo run -- config validate
cargo run -- config show
cargo run -- config show --json
```

## Running registered applications

`run` is the explicit one-shot launcher. With no IDs it selects every enabled
registry entry; with IDs, every requested ID must already be both
registered and enabled. IDs are deduplicated and launched in desktop-ID order.
Before any helper is started, every selected ID is freshly resolved through
desktop discovery, `Terminal=true` entries are rejected, and topology must be
conservatively classified as `Hybrid` with a non-empty E-core CPU list. An
empty enabled registry is a successful no-op.

```bash
cargo run -- run
cargo run -- run discord.desktop com.spotify.Client.desktop
cargo run -- run --dry-run --json
```

`run --dry-run` follows the same complete planning path but starts nothing.
Its plan includes the current executable and arguments, exact E-core list,
delay, nice value, I/O class and priority, and the process-tree enforcement
preference. It does not sleep or apply any runtime policy.
For reproducible fixture diagnostics it accepts `--config`, `--data-home`,
repeated `--data-dir`, `--ignore-desktop-filter`, and `--sysfs-root`. Launch
resolution includes `NoDisplay=true` applications, keeps `Hidden=true`
suppression, and continues to honor `OnlyShowIn`/`NotShowIn` unless the filter
is explicitly ignored.

Delays are deadlines relative to one run start and are ordered by delay then
desktop ID, so 0s, 2s, and 5s launch at approximately those times rather than
cumulatively. The parent starts a hidden internal helper at each deadline. The
helper applies the exact detected affinity, absolute nice value, and requested
Linux I/O policy to itself before directly `exec`ing the resolved executable.
The `none` I/O class leaves inherited I/O policy unchanged.

A bounded close-on-exec pipe carries typed affinity, nice, I/O, and exec
failures back to the parent. A ready message followed by pipe closure proves
that exec succeeded; it does not claim that the GUI application completed.
Earlier successful exec transitions remain reported if a later app fails. No
shell, `taskset`, `ionice`, or post-launch policy change is used, so shell
metacharacters remain literal arguments.

`run` exits after reporting confirmed exec transitions; it does not become a
permanent monitor. Use `supervise` when enabled applications with
`enforce_process_tree = true` need ongoing descendant affinity enforcement.

## Process-tree supervision

`supervise` uses the same complete planning, delay, helper, policy, and exec
acknowledgement pipeline as `run`:

```bash
cargo run -- supervise
cargo run -- supervise --json
```

After confirmed exec, only opted-in roots are enrolled. The supervisor records
each root PID and Linux start time, follows only `/proc/<pid>/task/*/children`
links from those known roots, verifies parentage and identities, and compares
each live thread's affinity before applying the plan's exact E-core set. It
never classifies or scans arbitrary processes by executable or name. Vanished
processes are normal, persistent warnings are bounded, and polling defaults to
one second. When all enrolled roots are gone it exits. SIGINT and SIGTERM stop
the supervisor cleanly, restore its signal mask, and send no signal to launched
applications.

Linux can reparent a deliberately daemonized descendant outside the still
verifiable root tree. Such a process is no longer enforceable and is never
claimed by name or guessed ancestry.

## User graphical-session startup

Startup integration is optional and user-level only:

```bash
cargo run -- startup enable
cargo run -- startup status
cargo run -- startup status --json
cargo run -- startup disable
```

Enable writes only the launcher-owned
`$XDG_CONFIG_HOME/systemd/user/ecore-launcher.service` (with the usual HOME
fallback), records a small ownership marker under `$XDG_STATE_HOME`, runs
direct `systemctl --user daemon-reload` and `enable`, and does **not** start the
service or launch applications immediately. The deterministic unit is tied to
`graphical-session.target`, disables systemd environment-variable expansion in
its safely quoted `ExecStart`, and uses `KillMode=process` so stopping the
supervisor does not kill launched desktop applications. Disable removes only
recognized launcher-owned state and preserves the registry and running apps.

Desktop environments differ in how they activate `graphical-session.target`
and import `DISPLAY`, `WAYLAND_DISPLAY`, `XDG_CURRENT_DESKTOP`, and the session
bus into the systemd user manager. `doctor` diagnoses the current environment;
users whose desktop does not activate or import the standard target/environment
must configure that desktop's user-manager integration explicitly.

Duplicate desktop autostart suppression is never implicit. The explicit form:

```bash
cargo run -- startup enable --suppress-autostart
```

detects matching enabled registry IDs in XDG system autostart locations and
the commonly used `/usr/share/autostart`, then creates only a marked user-owned
`Hidden=true` override under `$XDG_CONFIG_HOME/autostart`. Existing user files
are never overwritten. `startup disable` removes only exact launcher-generated
overrides, so suppression is reversible; adding an app to the registry alone
does not change autostart.

## Diagnostics

`doctor` is read-only and emits concise human checks or deterministic
structured JSON:

```bash
cargo run -- doctor
cargo run -- doctor --json
```

It reports registry validity and enabled IDs, fresh desktop resolution,
fail-closed topology and exact E-cores, affinity API/cpuset usability, policy
privilege warnings, startup unit ownership/state, graphical-session readiness,
autostart conflicts and owned overrides, procfs supervision prerequisites, and
runtime API assumptions. Checks are independently classified as `ok`,
`warning`, or `error`; warnings alone do not make the command fail.

### Validation and persistence

Existing config files must contain `schema_version = 1`. TOML syntax errors,
unsupported versions, duplicate or empty IDs, traversal-like IDs, invalid
policy values, unreadable files, and symlinked final config files fail clearly;
they are never silently replaced. The maximum registry size is 10,000 apps.

Delay is constrained to `0..=3600`. Nice is applied in Linux's `-20..=19`
range; a negative value may fail without sufficient privilege. I/O classes are typed `none`, `realtime`, `best-effort`,
or `idle`; `realtime` and `best-effort` require priority `0..=7`, while `none`
and `idle` require no numeric priority. Privilege failures are reported without
downgrading the policy. Process-tree enforcement is active only in supervised
launches whose stored setting is true.

Mutations create a `0700` parent when needed, acquire an advisory lock on a
same-directory lock file, reload the latest registry, validate one logical
change, write a unique same-directory `0600` temporary file, flush it, and
rename it atomically over the destination. Existing final config symlinks are
rejected. No backups, `.bak` files, or automatic recovery copies are created.

## Desktop application discovery

Run discovery against the process environment:

```bash
cargo run -- discover
```

The default scan reads the `applications` child of these XDG data roots, in
precedence order:

1. `XDG_DATA_HOME`, or `~/.local/share` when it is unset;
2. each root in `XDG_DATA_DIRS`, in listed order; or
3. `/usr/local/share` followed by `/usr/share` when `XDG_DATA_DIRS` is unset.

This corresponds to the common directories
`~/.local/share/applications`, `/usr/local/share/applications`, and
`/usr/share/applications`. Only configured `applications` trees are traversed;
unrelated data directories are not scanned.

Desktop-file IDs are the UTF-8 path relative to an `applications` directory,
with path separators replaced by `-`. Thus `vendor/example.desktop` has ID
`vendor-example.desktop`. A file at a higher-priority source owns its ID even
if it is malformed. Lower-priority files with that ID do not reappear, and a
higher-priority `Hidden=true` entry suppresses that ID completely.

The scanner then conservatively deduplicates separate IDs only when their
resolved executable path, fully processed static argument vector, and terminal
mode are identical. Different browser profiles or other entries with different
arguments remain distinct. Executable paths are not canonicalized, so valid
symlink paths are retained and aliases are not guessed to be equivalent.

### Filtering and diagnostics

Default discovery excludes:

- entries whose `Type` is not `Application`;
- `Hidden=true` and `NoDisplay=true` entries;
- entries without a usable localized `Name` or `Exec`;
- entries with malformed supported values or unsafe `Exec` syntax;
- entries whose executable or `TryExec` cannot be resolved;
- entries excluded by `OnlyShowIn` or `NotShowIn`; and
- overridden or conservatively deduplicated entries.

`XDG_CURRENT_DESKTOP` is treated as a colon-separated list and compared
case-insensitively with `OnlyShowIn` and `NotShowIn`. With no known current
desktop, `OnlyShowIn` entries are excluded, while `NotShowIn` alone does not
exclude an entry. This avoids claiming that a desktop-specific application is
visible when its required desktop cannot be established.

Useful diagnostic modes are:

```bash
cargo run -- discover --all
cargo run -- discover --ignore-desktop-filter
cargo run -- discover --json
```

`--all` adds `NoDisplay=true` entries. It does not resurrect `Hidden=true`
overrides, invalid applications, unavailable executables, or entries excluded
by desktop-environment filtering. `--ignore-desktop-filter` affects only
`OnlyShowIn` and `NotShowIn`.

Explicit roots are useful for diagnostics and reproducible tests:

```bash
cargo run -- discover \
  --data-home /mounted/user-share \
  --data-dir /mounted/local-share \
  --data-dir /mounted/system-share
```

These options name XDG data roots, not their `applications` children. If any
`--data-home` or `--data-dir` option is supplied, the complete default root set
is replaced: an omitted `--data-home` means no user root, and repeated
`--data-dir` values define system precedence. Explicit roots are required to
exist so a typo is a fatal diagnostic error. Skipped individual entries remain
non-fatal.

JSON output serializes deterministic `applications` and `warnings` arrays,
including desktop-file paths, source priority, parsed launch data, diagnostic
categories, severity, and whether the entry was skipped:

```bash
cargo run -- discover --json
```

### Safe `Exec=` handling

Desktop commands are never passed to `sh -c`, `bash -c`, `eval`, `which`, or
any other command interpreter. The scanner tokenizes double quotes,
backslash-escaped characters, spaces, and tabs itself. Shell metacharacters
such as `;`, `|`, `&&`, `$`, and backticks are ordinary argument characters;
they are never evaluated.

Supported field-code behavior is:

- `%f`, `%F`, `%u`, and `%U`: remove the complete argument because discovery
  supplies no file or URL;
- `%i`: remove the complete icon expansion;
- `%c`: replace the complete argument with the locale-resolved application
  name;
- `%k`: replace the complete argument with the desktop-file path; and
- `%%`: replace with a literal `%`, including inside a static argument.

Unsupported codes, a trailing `%`, dynamic codes embedded in larger tokens,
and dynamic codes in the executable token reject the entry with a typed
warning. This is intentionally stricter than guessing. For example,
`Exec=discord --start-minimized %U` becomes a resolved executable plus the
single static argument `--start-minimized`, while
`Exec=my-app --name %c --desktop-file %k` retains both flags and expands the
two following arguments without a shell.

Bare executable names are resolved in the explicitly configured `PATH` order
using Rust filesystem APIs. Commands containing `/` are checked directly.
Candidates must resolve to regular files with at least one executable bit.
Symlinks are accepted without canonicalizing the returned launch path.
`TryExec` is checked independently and never replaces the launch command.
Wrapper commands such as `flatpak run org.example.App` are preserved as the
resolved wrapper executable plus static arguments; Flatpak internals are not
inspected.

### Desktop-entry and locale limitations

Discovery parses only the main `[Desktop Entry]` group and ignores desktop action
groups and unknown keys. It supports `Type`, localized `Name` and
`GenericName`, `Exec`, `Icon`, `Hidden`, `NoDisplay`, `Terminal`, `TryExec`,
`OnlyShowIn`, `NotShowIn`, `Categories`, and `StartupWMClass`. Supported string
escapes are `\s`, `\n`, `\t`, `\r`, and `\\`; supported booleans are the
lowercase specification values `true` and `false`. Invalid UTF-8 and malformed
supported values produce warnings rather than panics.

Locale lookup tries the active locale without its encoding, then the
language/territory without a modifier, then the base language, and finally the
unlocalized value. It intentionally does not implement translation catalogs
or more elaborate locale aliasing.

## Discovery library API

The production scanner uses environment defaults, while tests and other
callers can supply every host-dependent input:

```rust
use std::path::PathBuf;
use ecore_launcher::{DesktopApplicationScanner, DiscoveryOptions};

let report = DesktopApplicationScanner::from_options(DiscoveryOptions {
    data_home: Some(PathBuf::from("/fixture/user-share")),
    data_dirs: vec![PathBuf::from("/fixture/system-share")],
    executable_path: vec![PathBuf::from("/fixture/bin")],
    locale: Some("en_US.UTF-8".to_owned()),
    current_desktops: vec!["GNOME".to_owned()],
    include_no_display: false,
    ignore_desktop_filter: false,
    require_existing_roots: true,
}).discover()?;
# Ok::<(), ecore_launcher::DiscoveryError>(())
```

`DiscoveryReport` separates sorted `DiscoveredApplication` values from sorted
`DiscoveryWarning` values. Each application retains its stable ID, localized
name, originating file, resolved executable, safe static arguments, icon,
terminal mode, categories, startup class, and XDG source priority.

## CPU topology detection

The package uses stable, safe Rust (MSRV 1.75) and supports Linux only.
Production topology detection reads `/sys/devices/system/cpu`; callers and the
CLI can supply an alternate CPU sysfs root for diagnostics and fixture tests.

The detector uses the global `online` list when available. If it is absent, it
falls back to `present` (or discoverable `cpuN` directories) and per-CPU
`online` flags. Linux may omit CPU0's per-CPU flag, and may omit it for other
CPUs which cannot be offlined; those CPUs are treated as online. CPUs absent
from the active online set are excluded from all physical-core and classified
CPU sets.

Physical cores are grouped from consistent `core_cpus_list` or
`thread_siblings_list` masks first, with the
`physical_package_id`/`core_id` pair as a fallback. Incomplete or contradictory
metadata remains visible as `Unknown` physical-core evidence rather than
causing a panic.

### Classification policy

Evidence is evaluated in this order:

1. Explicit kernel `topology/core_type` metadata.
2. Consistent physical-core and SMT topology distinctions.
3. Large, repeated, internally tight maximum-frequency clusters.
4. Cache layout observations.
5. `Uniform` or `Unknown` when the evidence is insufficient.

The optional x86 `core_type` interpretation recognizes only Intel's documented
CPUID leaf 0x1A type byte: `0x40`/64 (`Core`, treated as performance) and
`0x20`/32 (`Atom`, treated as efficiency). Unsupported, malformed, future, or
vendor-specific values are recorded but never guessed.

Without complete explicit metadata, a heuristic hybrid result requires exact
grouping and frequency data for every visible physical core, at least two
physical cores in each frequency cluster, at least 25% separation, at most 8%
spread within each cluster, a consistent SMT width per cluster, and wider SMT
in the higher-frequency cluster. Frequency, SMT, cache layout, and CPU
numbering alone never establish an E-core.

`Uniform` means visible cores are consistently grouped and no corroborated
heterogeneous distinction exists. `Unknown` means metadata is incomplete,
asymmetric without corroboration, or contradictory. Both results have an empty
`efficiency_cpus` set.

### Topology CLI and API

Topology commands remain available:

```bash
cargo run -- topology
cargo run -- topology --json
cargo run -- topology --sysfs-root fixtures/sysfs/intel-hybrid
```

The reusable entry point remains:

```rust
use ecore_launcher::CpuTopologyDetector;

let topology = CpuTopologyDetector::new("/sys/devices/system/cpu").detect()?;
# Ok::<(), ecore_launcher::DetectorError>(())
```

`CpuTopology` exposes sorted online logical CPU IDs, sorted physical-core
groups, performance and efficiency CPU IDs, overall classification, bounded
confidence, and structured deterministic evidence.

## Fixture-based tests

Tests do not inspect the host topology, installed desktop applications, host
locale, host desktop environment, or host `PATH`.

Synthetic sysfs trees under `fixtures/sysfs` cover explicit and heuristic
hybrid systems, uniform Intel and AMD systems, missing and malformed optional
metadata, offline CPUs, fallback online resolution, ambiguity, and malformed
required data.

Synthetic application trees under `fixtures/desktop` cover valid metadata,
static and quoted commands, every supported field code, shell metacharacters,
malformed and unavailable entries, visibility filters, localized names,
XDG overrides and hidden suppression, `TryExec`, conservative target
deduplication, and repeated data-directory precedence. Registry and integration tests use only
temporary configuration paths and synthetic discovery roots; they cover path
resolution, TOML syntax/version/semantic failures, canonical serialization,
unknown-value retention, private permissions, symlink rejection, atomic
mutation rollback, sequential locked updates, unavailable entries, and every
registry CLI command. Tests create temporary executable, absolute-path,
symlink, and invalid-UTF-8 fixtures when filesystem properties matter.
Supervisor tests use purpose-built child processes, exact synthetic E-core
masks, and temporary XDG-facing state; no real GUI application or live user
systemd configuration is used.

## Quality checks

Run:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
```

The package forbids unsafe Rust. Intentional limitations are Linux-only
operation, no TUI/GUI, no adaptive or automatic background-process
classification, no network or privileged daemon, no manual/fallback E-core
mask, and no enforcement after a descendant deliberately escapes a verifiable
managed tree.
