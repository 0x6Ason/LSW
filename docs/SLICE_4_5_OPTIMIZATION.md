# beta.8 Slice 4.5: architecture and default-image optimization

Slice 4.5 is a temporary acceptance boundary between the first seamless HWND
and the wider desktop feature matrix. It exists because adding clipboard,
multi-window, DPI, audio, and secure-desktop behavior on top of the current
large modules would make ownership and regression testing worse. It also turns
`slim` from a small AppX allowlist into a measured developer workstation
profile.

This document describes the target state and records incremental acceptance.
Protocol, host-client, native-presenter, typed profile, and WinPE orchestration
extraction are implemented. `slim-v2` is now the source and sealed-image
preparation identity:
it performs bounded offline and first-boot mutation with persistent audit
evidence. The final native workflow now installs zero-mutation `vanilla` from
the same exact ISO, measures three cold boots for each profile, and fails closed
on the numerical thresholds below before running the GUI matrix last. This is
not yet a runtime acceptance claim: that comparison and the preservation
matrix must pass on the real Windows/KVM runner before Slice 4.5 closes.

## Measured starting point

The Slice 4 native-gate commit has several files far beyond a useful review or
ownership boundary:

| Area | Current file | Approximate lines |
| --- | --- | ---: |
| Windows capture and input | `crates/lsw-agent/src/windows_capture.rs` | 4,100 |
| Windows agent entry/runtime | `crates/lsw-agent/src/main.rs` | 3,400 |
| Linux CLI entry/commands | `crates/lsw-cli/src/main.rs` | 2,600 |
| Seamless development E2E | `scripts/check-windows-seamless-e2e.sh` | 2,300 |
| Shared agent protocol | `crates/lsw-core/src/agent_protocol.rs` | 2,300 |
| WinPE preparation/runtime | `crates/lsw-core/src/winpe_dism.rs` | 2,200 |
| Real Windows/KVM E2E | `scripts/check-windows-kvm-e2e.sh` | 1,900 |
| Native host presenter | `crates/lsw-cli/src/gui_presenter.rs` | 1,900 |

The immediate source-size gate is a ratchet: legacy exceptions cannot grow and
new files cannot exceed 1,000 lines. Slice completion removes the exceptions,
sets entry points below 300 lines, and keeps production modules below 1,000
lines. Splitting code only to satisfy a number is not acceptance; each module
must own one lifecycle or protocol responsibility and have focused tests.

## Applicable WSL and WSLg principles

Microsoft WSL separates Linux, Windows, and shared code, then separates the
Windows client, service, host, relay, installer, and GUI executables. WSLg is a
separate versioned GUI system component with its own supervisor and packaging,
but it is delivered with WSL so users do not assemble the graphical stack.

LSW adopts those management boundaries, not the direction of the VM or the RDP
implementation:

- `lsw` is a thin, one-shot user client and command router.
- `lswd` owns VM processes, QMP, state reconciliation, and idle lifecycle.
- shared crates own versioned wire types and pure domain state; neither side
  imports an executable's private modules.
- `lsw-agent.exe` contains guest service/session integrations and no host UI.
- `lswg` owns native Linux display integration and is bundled, version-locked,
  launched inside the calling graphical session, and replaceable as one product
  component.
- packaging and E2E code are first-class modules rather than appendages to a
  large command or shell file.

Official references are the
[WSL source tree](https://github.com/microsoft/WSL/tree/master/src),
[WSL technical component overview](https://github.com/microsoft/WSL/blob/master/doc/docs/technical-documentation/index.md),
and [WSLg architecture](https://github.com/microsoft/wslg#readme). LSW does not
copy Microsoft source or imply Microsoft affiliation.

## Target source tree

The exact crate names may change only if the resulting ownership is clearer,
but Slice 4.5 targets this shape:

```text
crates/
  lsw-cli/
    src/commands/       one module per public command family
    src/main.rs         parse, dispatch, render errors
  lsw-host/
    src/agent/          authenticated guest client and sessions
    src/daemon/         private lswd client
    src/install/        install orchestration and progress model
  lsw-core/
    src/media/          Microsoft media model and download
    src/profile/        declarative image customization
    src/image/          sealed-image and clone domain
    src/vm/             backend-independent launch planning
  lsw-protocol/
    src/agent/          bounded guest wire protocol
    src/daemon/         bounded local-daemon protocol
    src/gui/            window, input, damage, and future clipboard types
  lsw-daemon/
    src/runtime/        supervision, QMP, resource policy
  lsw-agent/
    src/service/        SCM and fixed privileged helpers
    src/session/        exec, ConPTY, transfer, and leases
    src/desktop/        registered-user companion lifecycle
    src/gui/            HWND discovery, capture, input, and recovery
  lswg/
    src/window/         Wayland surface and state mapping
    src/render/         damage and frame presentation
    src/input/          focus, keyboard, pointer, and IME
    src/session/        protocol client, reconnect, and cleanup
e2e/
  windows-kvm/          real install and cold-restart scenarios
  seamless/             native and WSLg adapters sharing one scenario model
  fixtures/             bounded guest applications and test assets
packaging/
  linux/                bundle, systemd, completion, and install layouts
```

Migration order preserves wire and release behavior:

1. Extract protocols and pure domain types without changing encoded bytes.
2. Split host clients from CLI command rendering.
3. Move the current presenter into `lswg` and make bundle discovery exact-hash
   and version checked.
4. Split agent service, session, desktop, capture, and input ownership.
5. Split media/profile/DISM planning from VM execution.
6. Replace duplicated E2E shell flows with shared scenario drivers and small
   environment adapters.

Step 1's agent-wire extraction is implemented. `lsw-protocol` is dependency
free and separates constants, framing, integration controls, user operations,
sessions, GUI messages, transfers, codecs, and protocol tests. The public
`lsw-core` surface remains compatible through re-exports and error-mapping
frame wrappers; the protocol version and encoded bytes did not change. The
local daemon protocol remains separately versioned while its typed command
model is extracted.

Step 2's client extraction is also implemented. `lsw-host` now owns the private
Unix daemon connection plus focused agent control, process/terminal, GUI, and
file-transfer sessions. The old 1,862-line CLI client and 216-line daemon
client are gone; every new production module is below 600 lines. `lsw` retains
command parsing, notices, and progress, so the host crate does not import
executable-private modules.

Step 3 is implemented. The old 2,001-line CLI presenter is now the separately
built `lswg` runtime, split into event-loop, rendering, input, WSLg-adapter, and
test modules. `lsw` performs readiness, verifies an exact lowercase SHA-256 and
the shared `gui-window-v3`/package version, and sends one bounded launch frame
over the child's stdin. `lswg` reloads private instance state and its agent
credential itself, so tokens never enter argv, the environment, or the handoff.
The release bundle, installer, verifier, native gate, and WSLg development gate
all attest the same helper binary.

The profile/DISM part of step 5 is implemented as schema version 2. Exact AppX
display names and package-family names, optional features, supported product
uninstallers, service startup values, registry policy values, and preservation
requirements are typed and validated before any VM starts. Offline WinPE and
first-boot PowerShell generation live in separate focused modules. WinPE seed
generation, QEMU planning, runtime/progress observation, control-media creation,
safe host I/O, and tests are also separately owned; no resulting file needs a
source-size exception. Both customization paths inventory before and after,
treat an absent build-specific target as not
applicable, and fail if a matched target survives. The E2E path checks the
persisted report on three boot cycles and records guest resource samples plus
host allocation after TRIM. Runtime comparison to `vanilla` remains open.

Installation-seed orchestration is also below the source limit. Unattended
answer/password generation and the guest setup/SCM PowerShell payload are
separately owned, while `InstallSeedBuilder` retains validation, atomic writes,
and the existing public contract.

Every step must keep ordinary headless commands and the current first-HWND
matrix passing. A giant rename-only commit is not acceptable.

## Bundled `lswg` contract

`lswg` is not an optional download. The standard archive and installer contain
`lsw`, `lswd`, `lswg`, and the matching Windows agent. The release manifest
records all four hashes and a shared GUI protocol/build identity. `lsw run
--gui` launches the adjacent trusted `lswg` in the calling Linux graphical
session. `lsw` asks display-agnostic `lswd` to make the VM ready, then `lswg`
opens the bounded authenticated agent session while the daemon continues to
supervise the VM. Socket activation never requires `WAYLAND_DISPLAY`.
Users never run an install command for the helper and no GUI process remains
resident when the last window and integration lease close.

A deliberately headless machine may disable GUI launch policy, but this only
prevents execution. It does not create a second incomplete release artifact.
The native Wayland implementation remains the correctness path; X11 is a later
adapter and WSLg remains a Windows-hosted developer smoke environment.

## Default `slim` contract

`vanilla` remains a faithful Microsoft installation. New installs continue to
default to `slim`, whose manifest becomes a versioned list of typed operations:

- exact provisioned AppX display names and package-family identities;
- removable optional features;
- supported product uninstallers such as OneDrive setup;
- service startup policies;
- machine and Default User policy values;
- explicit preservation contracts and compatibility probes.

The prepare phase inventories before mutation, writes a machine-readable before
and after report, and fails if a requested removal returns an error or remains
present. Names that are absent on a particular Windows build are recorded as
not applicable. Matching is case-insensitive but bounded to exact allowlisted
display names or package families; substring and global wildcard deletion are
forbidden.

### Default removal families

- AI and consumer shell experiences: Copilot, Recall payload, Click to Do and
  separately removable AI experience packages, Widgets/news, and consumer web
  experience packages that are not required by WebView2.
- Cloud and communication: OneDrive and its startup hooks, consumer Teams,
  Skype, Cortana, Phone Link, legacy Mail/Calendar, People, and Todos.
- Gaming: Xbox application, Game Bar, Gaming Overlay, Identity Provider, TCUI,
  speech overlay, and related background services.
- Promotional media: Clipchamp, Solitaire, Bing News/Weather/Sports/Finance,
  Feedback Hub, Get Started, Get Help, Tips, Mixed Reality Portal, legacy 3D
  Viewer/Paint 3D, Maps, and known third-party promotional stubs such as
  Spotify, Netflix, TikTok, and Candy Crush when the official image provisions
  them.

The manifest preserves Calculator, Notepad, current Paint, Snipping Tool,
Photos, Store, App Installer/winget, Windows Terminal, and WebView2 unless a
real compatibility gate proves a better replacement. It must distinguish a
legacy Paint 3D identity from the current Paint package.

### Services and policy

The default disabled set is DiagTrack, SysMain, WSearch, Xbox services, Fax,
Retail Demo, and obsolete diagnostic collectors. Legacy TabletInputService is
conditional on a guest with no touch/pen capability and passing physical
keyboard, IME, accessibility, and seamless input tests. Delivery Optimization
loses peer-to-peer delivery and runs only on demand; it is not allowed to break
Store or Windows Update downloads. The core Diagnostic Policy Service remains
until LSW has an independently gated repair path.

Documented policy disables consumer application suggestions, Spotlight and
lock-screen promotion, Widgets/news, Start recommendations, web suggestions in
local search, activity publishing/upload, advertising ID, tailored diagnostic
experiences, and optional diagnostic data. Policy is applied to the machine or
Default User as appropriate before the permanent user is registered.

### Preservation and acceptance

The real gate uses the same exact Windows ISO to create `vanilla` and `slim`,
then measures three settled cold boots. Results include guest used bytes and
allocated host qcow2 bytes after TRIM, process/service inventory, committed and
working memory, startup entries, AppX packages, optional features, and every
mutation result.

Acceptance requires:

- zero surviving targeted packages/features and zero running disabled services;
- no OneDrive process, installed client, or startup entry;
- at least 10 fewer settled background processes than `vanilla`;
- at least 256 MiB less settled idle committed memory than `vanilla`;
- at least 3 GiB less guest-used system-volume space than `vanilla`;
- passing Store/winget install, Windows Update scan, Defender status, MSI and
  MSIX install, PowerShell/Terminal/ConPTY, user registration, UAC secure
  desktop, hibernate/resume, TRIM, live share, and seamless GUI tests.

The comparison report becomes a release artifact. If a Windows build renames a
target or changes a preservation dependency, the gate fails for review instead
of silently claiming a slim image.

Current status: the `slim-v2` mutation/audit path, inventory-only `vanilla-v2`
report, three-boot verifiers, exact-identity comparison, thresholds, and bounded
cleanup are source-complete and locally parsed/tested. The numerical comparison
and network-dependent Store/Update preservation probes remain an unpassed
real-run gate, so they must not be described as achieved.
