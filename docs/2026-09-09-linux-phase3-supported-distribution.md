# Linux Phase 3 (supported distribution): packaging, updates, and what is proven

Date: 2026-09-09

Source task: `13573eac` (Linux desktop support, Phase 3 — "Supported
distribution"), continuing task `9673bede` (Phase 2, GUI preview).

**Status: partial, and the parts are named.** The packaging contract, the
dependency audit, the installed-and-upgrade acceptance lane, the Linux release
channel design, the apt archive, the package-manager update UI and the CI build
lane are implemented with tests. The Bazel Linux release graph, the Phase 2 E2E
gaps, and every piece of acceptance that needs hardware or a person are **not**
delivered; §7 lists them individually with what each needs. Nothing in this
document reports an untested claim as measured.

## 1. Owner directives that shaped this phase

Two arrived during the work and both changed scope.

**Architectures (superseding the x86-only plan).** Verbatim: *"well we have
arm64 because most of the macs I have are arm64. let's just release both x86 and
arm."* Both architectures are required for a Linux publish;
`release-policy.json` declares `linux.requiredArchitectures:
["x86_64","arm64"]` and `optionalArchitectures: []`.

Where that directive lives matters, because it is easy to reach the wrong
conclusion from the record. It was given directly in the terminal, so it is
**not** a `task_input` row — `kanna_task_inputs` for this task shows only the
earlier manager decision and the x86-only directive it superseded. A reader who
checks the input ledger, finds the optional-ARM instruction and no later row
will conclude ARM is optional. It is not. The quote above is the later
instruction; the plan that was recorded and approved after it explicitly
replaces the x86-only one, and a subsequent manager message restated "Both
architectures still ship". Do not revert the policy to optional ARM, and do not
read the ledger's silence as absence of owner instruction.

**x86-64 acceptance hardware, 2026-09-09.** Verbatim: *"We won't have intel mac
access for a while. It's in Japan and i'm in canada for a month. Don't let that
gate."* The approved plan's workstream 1 — a native x86-64 Ubuntu 24.04
acceptance VM on the owner's Intel Mac — is unavailable for at least a month.
The substitute chosen, and its limits, are §5. The release gate wording changes
from "native x86-64 VM acceptance" to "x86-64 acceptance on the `ubuntu-24.04`
hosted runner"; the Intel-Mac pass is a named follow-up for when the hardware
returns. The apt signing host is unaffected: it is the owner's MacBook Pro,
which travels with the owner.

## 2. The dependency contract, measured

`packaging/linux/runtime-policy.json` is the Linux reading of the repository's
vendoring rule: an enumerated allowlist of sonames, each with the distribution
package that supplies it and why it cannot be vendored, plus a glibc/GLIBCXX
floor and an explicit list of things that must stay bundled.

The list is not a guess. On 2026-09-09 `readelf --wide -d` was read off the
seven Linux binaries Phase 2 left on the ARM64 VM (Ubuntu 26.04.1 aarch64):

| Artifact | `NEEDED` |
| --- | --- |
| `kanna-cli` | `libssl.so.3` `libcrypto.so.3` `libgcc_s.so.1` `libc.so.6` |
| `kanna-daemon` | `libc++.so.1` `libc++abi.so.1` `libgcc_s.so.1` `libm.so.6` `libc.so.6` |
| `kanna-mcp` | `libssl.so.3` `libcrypto.so.3` `libgcc_s.so.1` `libc.so.6` |
| `kanna-server` | `libc++.so.1` `libc++abi.so.1` `libssl.so.3` `libcrypto.so.3` `libgcc_s.so.1` `libm.so.6` `libc.so.6` |
| `kanna-task-transfer` | `libc++.so.1` `libc++abi.so.1` `libgcc_s.so.1` `libm.so.6` `libc.so.6` |
| `kanna-terminal-recovery` | `libc++.so.1` `libc++abi.so.1` `libgcc_s.so.1` `libc.so.6` |
| `kanna-desktop` | `libgio-2.0` `libgobject-2.0` `libglib-2.0` `libz.so.1` `libgdk-3` `libpango-1.0` `libgdk_pixbuf-2.0` `libcairo-gobject` `libcairo` `libwebkit2gtk-4.1` `libgtk-3` `libsoup-3.0` `libjavascriptcoregtk-4.1` `libgcc_s.so.1` `libm.so.6` `libc.so.6` `ld-linux-aarch64.so.1` |

That measurement corrected three things the written policy had wrong: `libz` was
missing from the allowlist, the dynamic loader appeared as a `NEEDED` entry and
would have been rejected as undeclared, and — the substantive one — **`libc++`
and `libc++abi` are linked dynamically today**, because `libghostty-vt-sys`'s
build script emits `cargo:rustc-link-lib=c++`.

The plan preferred static linking, and it is partly demonstrated: the pinned
Zig toolchain's own cache has been observed to produce `libc++.a` and
`libc++abi.a` (`~/.cache/zig/o/…`). Recording this as "cannot be vendored"
would therefore be false. It is a **conditional exception** carrying the
measurement, the evidence that static linking is partly reachable, and a named
follow-up — the same treatment OpenSSL gets — and every build reports by name
which artifacts still use it. The exception sets are asserted in
`tools/kd/tests/linux-elf-audit.test.ts` so they shrink deliberately rather
than growing by forgetting.

**Update, 2026-09-10:** the question this measurement left open — whether
`libc++1`/`libc++abi1` are actually available on the supported floor, not just
the 26.04 development VM — is now answered by real evidence: CI run
`34439249468` ran `./kd build linux-package` on `ubuntu-24.04` and
`ubuntu-24.04-arm` hosted runners (the actual 24.04/glibc 2.39 floor) and
confirmed `libc++1`/`libc++abi1` resolve to `libc++1-18`/`libc++abi1-18` from
`llvm-toolchain-18`, noble universe, on both architectures. That same run is
also the first real-floor `readelf` audit of the closure, and it is stricter
than the VM measurement: the VM's own libc++/libc++abi entries were measured
against only **four** artifacts on 2026-09-09; the CI run shows **five** —
`kanna-worker` links this too and had never been included in either the policy
measurement or `tools/kd/tests/linux-elf-audit.test.ts`'s closure. The same run
also surfaced a dependency the VM measurement missed entirely: Noble's
`libc++abi1-18` package itself depends on `libunwind-18 (>= 1:18.1.3)`, so
`libunwind.so.1` is now a third, direct `NEEDED` entry in the same five
binaries — declared as its own conditional exception in
`packaging/linux/runtime-policy.json`, transitive in provenance (it comes from
the already-accepted libc++abi package, not from anything Kanna's own build
script asks for) but audited directly since it appears in the shipped ELF
closure regardless of provenance. `libunwind-18` is noble universe, built from
the same `llvm-toolchain-18` source as the already-accepted libc++/libc++abi
exception.

None of this is clean-install proof yet. `kd build linux-package` audits the
staged closure before `dpkg-deb` runs (below), and CI run `34439249468` failed
that audit before the libunwind entry existed — no `.deb` was produced by that
run, and none has been produced from a fresh-host `apt install` since (the
CI runners themselves preinstall the `-dev` packages the audit is checking
for, so a green build there is not clean-install evidence either). That
remains open; see the fresh-host install proof requirement below.

`kd build linux-package` compiles, audits the staged artifacts' own ELF headers,
derives `Depends` from what survives, then packages. The order is the point: a
library the build host happened to have cannot become a silent runtime
requirement, because it fails the audit rather than being declared.

## 3. The installed layout

Declared once in `crates/runtime-defaults/src/linux_install.rs` and mirrored in
`tools/kd/src/runtime/linux-package.ts`, held together by a contract test. The
failure they guard is invisible on any build machine: the package installs
perfectly and a user's desktop cannot find a sidecar.

```
/usr/lib/kanna/kanna-desktop           the real executable
/usr/lib/kanna/{kanna-worker,kanna-daemon,kanna-cli,kanna-mcp,
                kanna-server,kanna-task-transfer,kanna-terminal-recovery}
/usr/lib/kanna/.kanna/{agents,workflows,tasks}/   built-in definitions
/usr/bin/kanna                          symlink -> ../lib/kanna/kanna-desktop
/usr/share/applications/build.kanna.desktop
/usr/share/icons/hicolor/{32,64,128}x*/apps/build.kanna.png
```

Everything Kanna owns is in one directory because the runtime's *existing*
sidecar search looks beside the running executable first — so an installed
desktop needs no installed-only code path. `/usr/bin/kanna` is a symlink and not
a copy for the same reason: a copy resolves `current_exe()` into `/usr/bin`,
where no sidecar lives, and every task spawn would fail on a user's machine
while passing on every builder. `kanna-staging` shares no path, no desktop entry
and no worker unit name, so both install at once.

Maintainer scripts never stop a daemon, kill a cgroup, enable linger or start a
service. Live agent sessions surviving a package replacement is the daemon's
reason to exist, and an unattended `apt upgrade` that killed them would destroy
an operator's work. Removal keeps user data.

## 4. The installed live-upgrade proof

Phase 1 chose a byte-exact executable-path identity rule for daemon handoff with
no `(deleted)` tolerance, arguing that a package replaces binaries by
rename-into-place and the operator then restarts the launcher — so a fresh
readable executable always exists at the same path. It recorded that the
installed proof belonged to Phase 3. `tests/linux-installed` is that lane.

It runs against `/usr/lib/kanna` under the user's real `systemd --user` manager,
which is the only place the argument is testable: the headless-worker lane
spawns its supervisor as a child of vitest from `.build/`, where the launcher
trust root is not an installed path, no bytes are replaced underneath a live
daemon, and the restart is not `KillMode=process`.

`upgrade.e2e.test.ts` is the sequence, in order: A installed with a live agent
session; B installed *underneath it* with nothing restarted; the operator's unit
restart; then the assertions — the agent is the same process by pid **and start
time** (pids are reused, and a pid-only check would pass against a different
agent that inherited one), the surviving daemon is re-adopted rather than
replaced, and a message delivered afterwards is submitted exactly once with a
durable `task_input` row. That last one is what makes it an upgrade proof rather
than a liveness check: terminal bytes are not a record.

A missing package or an unusable host fails loudly with a reason. A gate that
cannot distinguish "the upgrade works" from "nothing was installed" is not a
gate.

**This lane has not yet been run against real packages.** §7.

## 4a. A real package was built (2026-09-09, ARM64 VM)

The packaging code was run against real binaries rather than only against unit
fixtures. On the Phase 0/1/2 VM (Ubuntu 26.04.1 aarch64), the seven Linux
binaries Phase 2 left in `~/kanna-p2/.build` were staged through
`stageLinuxPackageTree` and built with `dpkg-deb --root-owner-group --build`.

Result: `kanna_0.0.68-1_arm64.deb`, 186 MB, accepted by `dpkg-deb --info` and
`--contents`. The properties that only exist after a real build all held:

- `./usr/bin/kanna -> ../lib/kanna/kanna-desktop` survived as a **symlink**, not
  a copy — the property the whole sibling layout depends on.
- `postinst` and `prerm` are recorded as maintainer scripts (`*` in
  `dpkg-deb --info`) with their executable bits.
- The built-in `.kanna/` definitions, the desktop entry and all three icon sizes
  are present at their declared paths.
- `Depends`, `Installed-Size` and the version parsed cleanly.

**And it found a defect.** The desktop entry shipped as `0664` —
group-writable. `writeFileSync`, `mkdirSync` and `cpSync` take their modes from
the *builder's* umask, so a build machine with a group-writable default would
have installed group-writable files onto every user's machine. Invisible in the
staged tree; visible only in `dpkg-deb --contents`. `stageLinuxPackageTree` now
normalizes every packaged path to 0755/0644 (including the tree root, which
`dpkg-deb` records as `./`), a test asserts nothing is group- or
world-writable whatever the umask, and the fix was re-verified on the VM:
`find … -perm /022` returns nothing.

Two caveats on this evidence, so it is not read as more than it is:

- The binaries are **Phase 2 debug builds**, not release builds, and
  `kanna-worker` was never built on that VM — a placeholder script stood in for
  it, so the layout check was exercised but that binary was not.
- `dpkg-deb`'s default xz compression took 3m28s wall / 26m CPU for these
  (large, unstripped) debug binaries. Release artifacts will be far smaller;
  if it stays slow, `-Zzstd` is supported on the 24.04 floor and is the
  next thing to try.

This closes "a package has never been built" as a *packaging* question. It does
**not** make anything publishable: the binaries came from Cargo, not the Bazel
release graph (§7.1).

## 5. The support matrix, and what each row is evidenced by

| Dimension | Position | Evidence today |
| --- | --- | --- |
| Distribution floor | Ubuntu 24.04 LTS, glibc 2.39, kernel 6.8 | **Partially verified.** CI run `34439249468` built and audited on real `ubuntu-24.04`/`ubuntu-24.04-arm` hosted runners (§2) — package availability confirmed both architectures, but the audit found an undeclared dependency and no `.deb` was produced; no clean-install proof exists yet. |
| x86-64 build | Required | CI lane written and run: CI `34439249468` compiled and linked all eight binaries on `ubuntu-24.04`. **The release package was not produced** — the runtime audit vetoed before `dpkg-deb` (§2), so this is a compiled-binary result, not a package result. |
| arm64 build | Required | Phase 2 built all seven binaries natively (debug) on the dev VM. CI `34439249468` separately compiled and linked all eight binaries on `ubuntu-24.04-arm`. **The release package was not produced**, for the same audit-veto reason. |
| x86-64 installed acceptance | `ubuntu-24.04` hosted runner (substitute) | Lane written; **not wired into CI and never run** — see §7.3. |
| arm64 installed acceptance | Native guest on an Apple Silicon Mac | **Not provisioned.** The existing VM is 26.04, not the 24.04 floor, and has no passwordless sudo. |
| Display | GNOME Wayland primary; X11/XWayland runs, not performance-certified | Phase 2 evidence only, under headless GNOME. |
| Rendering | Software rendering must be usable; GPU optional | Phase 2 measured software only. The DMA-BUF decision now lives in the binary (§6). |
| Paired mobile over LAN | Launch requirement | **No evidence.** Needs a phone-reachable network; the VM is on UTM NAT. |
| Human interaction acceptance | Required before review | **None.** |

### The x86-64 substitute, and its limits

Per the 2026-09-09 directive, the `ubuntu-24.04` hosted runner is the intended
x86-64 installed-acceptance host. What it can supply: the supported floor's
userspace, a real `systemd --user` manager (with `loginctl enable-linger`), real
`apt`, and a real two-version upgrade.

**It does not run the lane yet.** The first version of
`linux-release-check.yml` had an `installed-acceptance` job, and review found it
could never have worked: it was gated on a `workflow_dispatch`/`workflow_call`
input that is simply absent on `pull_request` and `push`, so it silently skipped
on both, and on the one trigger where it did run it failed its own two-package
check — the job built one package and the upgrade needs two, from two source
revisions. That job is removed rather than half-fixed with an untested
merge-base build: the workflow now builds and audits both architectures, which
it really does, and running the lane in CI is §7.3. Until then the lane is run
by hand:
`./kd test linux-installed --old-artifact <deb> --new-artifact <deb>`.

What it does **not** supply, recorded rather than glossed:

- **It is not a clean machine.** A hosted runner ships with Rust, Node and
  compilers preinstalled, so it is the *opposite* of the clean-machine baseline.
  The lane records which developer tools were on `PATH` and asserts only that
  Kanna's own binaries start with a bare `PATH` — which is a weaker claim than
  "no developer tools are installed", and is labelled as such.
- **Its kernel is not 6.8.** The 24.04 image runs a newer Azure kernel, so the
  kernel floor is not exercised.
- **No phone-reachable LAN, and no human.** Mobile pairing acceptance and the
  IME/scrolling/paste/focus/scaling verdicts stay with the arm64 guest and, for
  x86-64, with the Intel Mac when it returns.
- **The alternative was considered and not chosen:** an x86-64 24.04 guest under
  QEMU TCG emulation on Apple Silicon would give a real login session at the cost
  of being far slower. It remains the fallback for anything needing a graphical
  session on x86-64 before the Intel Mac returns; nothing in this phase's
  correctness evidence requires it.

**Named follow-up:** re-run the installed and upgrade lanes on the native x86-64
Ubuntu 24.04 VM on the Intel Mac when it is available, and record whether the
kernel floor and clean-machine claims hold there.

## 6. Decisions implemented

**Graphics.** WebKitGTK's DMA-BUF renderer needs an openable DRM render node;
without one it does not degrade, it never starts a web process, and the app is
alive with no window and nothing distinctive in the log. Phase 2 put the probe
in `kd`, which covers a preview and not a package — an installed Kanna never
goes through `kd`. It now runs in the desktop binary, first thing in `main`
while the process is still single-threaded, and still probes rather than
blanket-disabling, so a machine with a working GPU keeps the accelerated path.
`kd` keeps only the job the binary cannot do: carrying an operator's explicit
setting into a tmux window a respawn would strip.

**Updates are the package manager's.** The Tauri self-updater plugin is *not
registered* on Linux. Hiding the button would leave `plugin:updater` reachable,
and an install over a dpkg-managed tree would replace files the package manager
believes it owns. `linux_package_status` is read-only by construction —
`dpkg-query` and `apt-cache policy`, `LC_ALL=C`, `dpkg --compare-versions` for
ordering — and a test asserts the module never learns to install. The UI gets
its own states, because "Update available" above a button the app cannot honour
is how a person ends up believing they clicked something; an unreadable package
index is shown as itself rather than rounded down to silence.

**Release lineage.** Linux ships on `desktop-linux-staging` / `desktop-linux`
with `linux-v…` tags and `release/linux/X.Y` series branches. The tag shape is
load-bearing: a bare `vX.Y.Z` is the macOS release and GitHub resolves its own
"latest" from those, so each platform's tags are invisible to the other's
version-floor and ancestry lookups. Linux has its own soak.

**The apt archive.** apt does not verify a `.deb` — it verifies `InRelease` and
follows a checksum chain — so every property that matters is the write order:
immutable pool artifacts first, then indexes, then `InRelease` last as the
single commit point. Checksums are verified against storage before signing,
because signing is what makes the index's claim true. `Valid-Until` so an
abandoned channel stops being trusted, `Acquire-By-Hash` so a mid-publish client
does not hit a hash-sum mismatch, `Signed-By` so the key authenticates this
repository and nothing else.

## 7. Not delivered

Each item, with what it needs.

1. **The Bazel Linux release graph (M2).** `MODULE.bazel` still declares only
   Darwin triples in all eight `supported_platform_triples` lists, and no Linux
   platform or toolchain is registered. `kd build linux-package` uses Cargo,
   which the plan explicitly labels a prototype path: **no artifact it produces
   may be published.** Needs the crate universes repinned for both Linux
   triples, Linux platforms/toolchains registered, Darwin SDK actions scoped to
   Darwin, and a `kanna-worker` Bazel target.
2. **No package has been built from the release graph.** One real `.deb` was
   built and validated on the ARM64 VM (§4a) — from Cargo debug binaries, with
   a placeholder `kanna-worker`. A publishable artifact needs (1).
3. **The installed and upgrade lanes have never been run, and CI does not run
   them.** They need a host where the test user can become root; the ARM64 VM
   has no passwordless sudo and no container runtime. Wiring them into
   `linux-release-check.yml` needs a second package built from another source
   revision (the merge base) with a distinct version so the two `.deb` names do
   not collide, uploaded alongside the candidate. The first attempt at this job
   was removed for promising acceptance it could not perform; see §5.
4. **Phase 2's E2E gaps (M4) are untouched.** Two simultaneous isolated
   instances, the Linux mock and real desktop lanes, real WebKitGTK credential
   tests, clipboard/drag/scaling, and paired-mobile LAN acceptance all remain
   as Phase 2 left them. They are not inherited as passed.
5. **The apt signing key does not exist**, and no archive has been published.
   Needs the owner's MacBook Pro, key custody, and the storage bucket.
6. **`kd release --platform linux` is not threaded through the release
   commands.** The descriptor, policy and archive exist; `release.ts` still has
   the macOS constants inline. A test pins the descriptor to those constants so
   introducing it cannot silently repoint production.
7. **The cross-version handoff fixture is still skipped on Linux.**
   `previous_daemon.rs`'s archived-tag gate needs a real Linux-capable release
   tag, which requires (1) and (5).
8. **`libc++`/`libc++abi`/`libunwind` static linking**, per §2.
9. **Support documentation for users** — install, key bootstrap, update and
   restart sequence, service semantics, uninstall, recovery, logs, graphics
   troubleshooting — is not written. It should not be written before there is a
   package a person can install.

## 7a. Visual verification (2026-09-09, ARM64 VM)

AGENTS.md requires changed UI to be rendered in the real app. The update prompt
gained two Linux states, and both were rendered under real WebKitGTK on the
Phase 0/1/2 VM (Ubuntu 26.04.1 aarch64, GNOME/Wayland console session), driven
over the app's own W3C WebDriver endpoint and captured from the webview:

| Capture | State |
| --- | --- |
| `package-update-light.png` / `package-update-dark.png` | `packageManagerUpdate` |
| `package-unknown-light.png` / `package-unknown-dark.png` | `packageManagerUnknown` |

Saved under the gitignored `docs/task-screenshots/13573eac-screenshots/`; this
paragraph and the task result are the durable record, since the worktree goes
away.

What the captures show. The update state renders the headline "Update available
from your package manager", the ownership sentence, `Installed: 0.0.68-1` and
`Available: 0.0.71-1` from the injected status, and the `apt` command in
monospace — and **its only action is Dismiss**. No Install, no Restart, no
Retry. That negative is the whole design: the app cannot perform this upgrade,
and a button that looked like it could is how a person ends up believing they
started one. The unknown state renders "Package information unavailable" plus
the reason and the `sudo apt update` remedy, again with only Dismiss.

**A finding from doing this.** The review expected the unknown state to render
on a Linux dev binary without an installed package. It does not, and no package
state can: `useAppUpdate`'s `ensureEnabled()` returns false for
`import.meta.env.MODE === "development"` — and again for a `KANNA_WORKTREE=1`
instance — *before* any package check runs. A dev build therefore shows nothing
whatever `dpkg` and `apt` report. Rendering these states in the real app needs
either a production build installed from a package, or a fixture; the review
sanctioned the latter, so `__e2eInjectPackageStatus` was added beside the
existing `__e2eInjectUpdate`, under the same `import.meta.env.DEV &&
window.__KANNA_E2E__` guard. It feeds the real `applyPackageStatus` — extracted
from the check rather than copied — so what the captures show is the mapping the
product uses, not a hand-set status.

Two limits, stated rather than implied. The versions in the update capture are
injected, not read from a real `apt` index, so these prove the rendering and the
absent actions, not the `dpkg-query`/`apt-cache` parsing (that is covered by the
Rust unit tests in `commands/linux_package.rs`). And the captures are of the
webview, not the window: GNOME's Screenshot D-Bus API is access-denied to
ordinary callers on this session and no capture tool is installed, which needs
root. Window chrome is not what changed.

One cosmetic observation, left alone deliberately because it is outside this
revision's scope: in the unknown state the fixed hint and the backend `detail`
say nearly the same thing twice. It reads as redundant rather than wrong.

## 7b. The one gate failure, its cause, and the fix carried from main

Before the fix below was applied, `./kd test all` on this branch stopped in the
`rust` lane on exactly one test:
`terminal_watcher::tests::watcher_applies_attached_busy_as_working` in
`kanna-server` (`terminal_watcher.rs:1898`, `left: Some("unread")` /
`right: Some("working")`).

It is not this branch's. It reproduces byte-identically at the branch base
`7f8991f0e` — checked in a throwaway worktree — and this branch never touches
`kanna-server`. The cause is upstream of both: PR #1380 (`9d0679cc3`) made a
busy runtime frame keep `unread` activity, updated the adjacent spurious-idle
test, and missed this one, which still seeded `unread` and asserted `working`.

**The fix is on `origin/main`: `fefec7071`**, "test(server): seed the
attached-busy watcher test as idle, not unread" — a one-line change to the
test's seed (`update_pipeline_item_activity("task-child", "idle")`), plus the
comment explaining it. Test-only.

**That exact change is applied on this branch**, on manager direction: recording
the attribution alone would have left the next required gate deterministically
red, so the causally necessary one-line baseline fix is carried here rather than
waiting for a rebase. The changed lines are byte-identical to `fefec7071`'s —
verified by diffing the two patches' content lines — and nothing else in
`kanna-server` is touched. This is not adjacent subsystem cleanup: it is the
single seed value whose staleness causes the failure.

Attribution: the fix is `fefec7071`'s, authored on `main`. When this branch
merges, the two changes are the same edit and resolve trivially; if a rebase
lands `fefec7071` first, this hunk simply disappears.

The root cause is worth keeping visible because it is a class of bug, not an
accident: a contract change (`unread` survives a busy frame) updated one test
that encoded the old contract and missed its neighbour. The seed, not the
assertion, was the stale part.

## 8. E2E coverage note

Per AGENTS.md, behaviour crossing component boundaries needs E2E coverage. The
installed lane (`tests/linux-installed`) is that coverage for packaging,
installation and upgrade, and it exists but has not run — items 2 and 3 above.
This document is the dated note for that gap. What is testable today is tested:
`tools/kd/tests/linux-package.test.ts`, `linux-elf-audit.test.ts`,
`linux-apt.test.ts`, `release-platform.test.ts`, `ci-workflow.test.ts`,
`tests/linux-installed/src/installedLayout.test.ts`,
`apps/desktop/src-tauri/tests/linux_update_ownership.rs`, and the desktop
update composable and component suites.
