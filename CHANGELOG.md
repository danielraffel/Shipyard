# Changelog

All notable changes to Shipyard are documented here. Each entry links
to its [GitHub Release](https://github.com/danielraffel/Shipyard/releases).

<a id="v0310"></a>
## [0.31.0] - 2026-04-22

- ssh-windows: decode PowerShell CLIXML error envelope (#188) ([#189](https://github.com/danielraffel/Shipyard/pull/189))
- release: resolve signing identity by Team ID; require all 5 secrets (#177) ([#184](https://github.com/danielraffel/Shipyard/pull/184))

<a id="v0300"></a>
## [0.30.0] - 2026-04-22

- fix: escape dynamic text before Rich markup interpolation (Codex on #170) ([#180](https://github.com/danielraffel/Shipyard/pull/180))

<a id="v0290"></a>
## [0.29.0] - 2026-04-22

- perf: defer rich import + bump PyInstaller Python to 3.13 (#28) ([#174](https://github.com/danielraffel/Shipyard/pull/174))
- release: sign embedded dylibs during PyInstaller build to fix Team-ID mismatch ([#172](https://github.com/danielraffel/Shipyard/pull/172))
- ship: heartbeat during long validation phase so stdout isn't silent ([#171](https://github.com/danielraffel/Shipyard/pull/171))

<a id="v0280"></a>
## [0.28.0] - 2026-04-22

- cli: surface target error_message under the run/ship summary table ([#170](https://github.com/danielraffel/Shipyard/pull/170))
- release: sign + notarize the macOS CLI binary ([#165](https://github.com/danielraffel/Shipyard/pull/165))

<a id="v0272"></a>
## [0.27.2] - 2026-04-22

- fix/22 reconcile skip aged terminal ([#164](https://github.com/danielraffel/Shipyard/pull/164))
- fix/157 preflight before pr ([#163](https://github.com/danielraffel/Shipyard/pull/163))

<a id="v0271"></a>
## [0.27.1] - 2026-04-22

- fix/155 worktree shipyard local fallback ([#162](https://github.com/danielraffel/Shipyard/pull/162))

<a id="v0270"></a>
## [0.27.0] - 2026-04-22

- feat/daemon tunnel supervisor ([#161](https://github.com/danielraffel/Shipyard/pull/161))

<a id="v0262"></a>
## [0.26.2] - 2026-04-22

- fix/tailscale funnel probe retry ([#160](https://github.com/danielraffel/Shipyard/pull/160))
- fix/install macos codesign refresh ([#159](https://github.com/danielraffel/Shipyard/pull/159))

<a id="v0261"></a>
## [0.26.1] - 2026-04-22

- fix/141 ssh probe backoff retry ([#158](https://github.com/danielraffel/Shipyard/pull/158))

<a id="v0260"></a>
## [0.26.0] - 2026-04-22

- feat/daemon version drift check ([#156](https://github.com/danielraffel/Shipyard/pull/156))

<a id="v0250"></a>
## [0.25.0] - 2026-04-22

- feat/ship state list ipc ([#154](https://github.com/danielraffel/Shipyard/pull/154))
- fix/version bump override authoritative ([#152](https://github.com/danielraffel/Shipyard/pull/152))

<a id="v0240"></a>
## [0.24.0] - 2026-04-22

- fix/pr text walk past bump commit ([#151](https://github.com/danielraffel/Shipyard/pull/151))

<a id="v0230"></a>
## [0.23.0] - 2026-04-22

- feat/wait primitive ([#150](https://github.com/danielraffel/Shipyard/pull/150))

<a id="v0229"></a>
## [0.22.9] - 2026-04-21

- ship: quality PR titles + bodies from commit subject/body (0.22.9) ([#148](https://github.com/danielraffel/Shipyard/pull/148))
- chore(plugin): bump to 0.11.8 + raise min_shipyard_version floor to 0.22.8 ([#147](https://github.com/danielraffel/Shipyard/pull/147))

<a id="v0228"></a>
## [0.22.8] - 2026-04-21

- daemon: periodic reconcile loop — permanently closes state-drift gap (0.22.8) ([#146](https://github.com/danielraffel/Shipyard/pull/146))

<a id="v0227"></a>
## [0.22.7] - 2026-04-21

- ship: reconcile must mirror healed status into evidence_snapshot (0.22.7) ([#145](https://github.com/danielraffel/Shipyard/pull/145))

<a id="v0226"></a>
## [0.22.6] - 2026-04-21

- ship: shipyard ship-state reconcile — heal drifted CI state (0.22.6) ([#144](https://github.com/danielraffel/Shipyard/pull/144))

<a id="v0225"></a>
## [0.22.5] - 2026-04-21

- daemon: status must read past hello + CI smoke test (0.22.5) ([#143](https://github.com/danielraffel/Shipyard/pull/143))

<a id="v0224"></a>
## [0.22.4] - 2026-04-21

- daemon: bundle encodings.idna + verify child alive after spawn (0.22.4) ([#142](https://github.com/danielraffel/Shipyard/pull/142))

<a id="v0223"></a>
## [0.22.3] - 2026-04-21

- ship: drop Shipyard branding + 'Ship' term from auto-opened PR bodies ([#140](https://github.com/danielraffel/Shipyard/pull/140))

<a id="v0222"></a>
## [0.22.2] - 2026-04-21

- daemon: really fix spawn_detached for standalone binaries (0.22.1 → 0.22.2) ([#139](https://github.com/danielraffel/Shipyard/pull/139))
- feat(plugin): emit SessionStart JSON with systemMessage banner ([#138](https://github.com/danielraffel/Shipyard/pull/138))
- fix(plugin): wrap hooks.json events under 'hooks' key (Claude Code schema) ([#137](https://github.com/danielraffel/Shipyard/pull/137))
- feat(plugin): reframe staleness as advisory, not AskUserQuestion directive ([#136](https://github.com/danielraffel/Shipyard/pull/136))
- feat(plugin): add /shipyard:upgrade command for explicit CLI upgrades ([#134](https://github.com/danielraffel/Shipyard/pull/134))

<a id="v0221"></a>
## [0.22.1] - 2026-04-21

- daemon: fix spawn_detached for standalone-binary installs (0.22.0 → 0.22.1) ([#130](https://github.com/danielraffel/Shipyard/pull/130))
- fix(plugin): drop invalid `agents` manifest field (unblocks `claude plugin install shipyard`) ([#133](https://github.com/danielraffel/Shipyard/pull/133))
- ci skill: add "Iterating on a single-platform failure" section ([#129](https://github.com/danielraffel/Shipyard/pull/129))

<a id="v0220"></a>
## [0.22.0] - 2026-04-21

- Add `shipyard daemon`: webhook receiver with Tailscale Funnel + local IPC (#125) ([#127](https://github.com/danielraffel/Shipyard/pull/127))

<a id="v0212"></a>
## [0.21.2] - 2026-04-20

- feat(doctor): detect shadowed `shipyard` binaries on PATH ([#122](https://github.com/danielraffel/Shipyard/pull/122))

<a id="v0211"></a>
## [0.21.1] - 2026-04-20

- fix(ssh): robust Windows probe + missing-host guard (#119, #120) ([#121](https://github.com/danielraffel/Shipyard/pull/121))
- fix(ship-state): close B1-B4 audit-discovered bugs (#108-#111) ([#117](https://github.com/danielraffel/Shipyard/pull/117))
- feat: Phase C doc-sync hook + dedicated state-machine CI lane (#101) ([#116](https://github.com/danielraffel/Shipyard/pull/116))

<a id="v0210"></a>
## [0.21.0] - 2026-04-20

- fix(queue): atomic writes with fsync + rename ([#105](https://github.com/danielraffel/Shipyard/pull/105))
- test: Phase B ship-state transition tests (#101) ([#115](https://github.com/danielraffel/Shipyard/pull/115))

<a id="v0200"></a>
## [0.20.0] - 2026-04-20

- fix(preflight): SSH fail-fast with rich diagnosis + exit code 3 ([#106](https://github.com/danielraffel/Shipyard/pull/106))

<a id="v0191"></a>
## [0.19.1] - 2026-04-20

- fix(pr): resolve gate scripts from tools/scripts/ or scripts/ ([#104](https://github.com/danielraffel/Shipyard/pull/104))
- docs: ship-state-machine audit (#101 Phase A) ([#107](https://github.com/danielraffel/Shipyard/pull/107))

<a id="v0190"></a>
## [0.19.0] - 2026-04-19

- feat: warm-pool runner reuse across PRs (closes #82) ([#98](https://github.com/danielraffel/Shipyard/pull/98))

<a id="v0180"></a>
## [0.18.0] - 2026-04-19

- Ship feat/degrade-mode ([#97](https://github.com/danielraffel/Shipyard/pull/97))

<a id="v0170"></a>
## [0.17.0] - 2026-04-19

- feat: runner liveness heartbeat + watch UI polish (#84, #93) ([#96](https://github.com/danielraffel/Shipyard/pull/96))

<a id="v0160"></a>
## [0.16.0] - 2026-04-19

- feat/cross pr reuse ([#95](https://github.com/danielraffel/Shipyard/pull/95))

<a id="v0150"></a>
## [0.15.0] - 2026-04-19

- feat: failure classification + flaky-target quarantine (closes #83) ([#94](https://github.com/danielraffel/Shipyard/pull/94))

<a id="v0140"></a>
## [0.14.0] - 2026-04-19

- Mid-flight lane add: shipyard cloud add-lane (#86) ([#90](https://github.com/danielraffel/Shipyard/pull/90))
- docs(ci): add agent decision guide for shipyard watch ([#92](https://github.com/danielraffel/Shipyard/pull/92))

<a id="v0130"></a>
## [0.13.0] - 2026-04-19

- Incremental git bundles for SSH delivery (#81) ([#91](https://github.com/danielraffel/Shipyard/pull/91))

<a id="v0120"></a>
## [0.12.0] - 2026-04-19

- Ship feat/locality-routing ([#89](https://github.com/danielraffel/Shipyard/pull/89))

<a id="v0110"></a>
## [0.11.0] - 2026-04-18

- Ship feature/post-tag-sync-bash-fix ([#75](https://github.com/danielraffel/Shipyard/pull/75))

<a id="v0100"></a>
## [0.10.0] - 2026-04-18

- Ship feature/changelog-header-comment ([#74](https://github.com/danielraffel/Shipyard/pull/74))

<a id="v090"></a>
## [0.9.0] - 2026-04-18

- Ship feature/post-release-docs-sync ([#73](https://github.com/danielraffel/Shipyard/pull/73))
- feat/patch auto apply rollup ([#72](https://github.com/danielraffel/Shipyard/pull/72))
- docs/surface v0.8.0 commands ([#71](https://github.com/danielraffel/Shipyard/pull/71))

<a id="v080"></a>
## [0.8.0] - 2026-04-17

- chore/bump 0.8.0 ([#69](https://github.com/danielraffel/Shipyard/pull/69))
- fix/codex 67 followup ([#68](https://github.com/danielraffel/Shipyard/pull/68))
- fix/codex 66 followup ([#67](https://github.com/danielraffel/Shipyard/pull/67))
- feat/cloud retarget ([#66](https://github.com/danielraffel/Shipyard/pull/66))
- fix/codex 64 followup ([#65](https://github.com/danielraffel/Shipyard/pull/65))
- feat/ship auto merge ([#64](https://github.com/danielraffel/Shipyard/pull/64))
- fix/codex 62 followup ([#63](https://github.com/danielraffel/Shipyard/pull/63))
- feat/watch stream ([#62](https://github.com/danielraffel/Shipyard/pull/62))
- fix/codex 60 followup ([#61](https://github.com/danielraffel/Shipyard/pull/61))
- fix/codex 59 followup ([#60](https://github.com/danielraffel/Shipyard/pull/60))
- feat/trailer shortcuts ([#59](https://github.com/danielraffel/Shipyard/pull/59))
- fix/codex 57 followup ([#58](https://github.com/danielraffel/Shipyard/pull/58))

<a id="v070"></a>
## [0.7.0] - 2026-04-16

- fix/codex 56 followup ([#57](https://github.com/danielraffel/Shipyard/pull/57))

<a id="v060"></a>
## [0.6.0] - 2026-04-16

- fix/codex 55 followup ([#56](https://github.com/danielraffel/Shipyard/pull/56))

<a id="v050"></a>
## [0.5.0] - 2026-04-16

- fix/codex release pipeline batch ([#55](https://github.com/danielraffel/Shipyard/pull/55))
- feat/cloud run require sha ([#54](https://github.com/danielraffel/Shipyard/pull/54))
- feat/doctor release chain ([#52](https://github.com/danielraffel/Shipyard/pull/52))

<a id="v040"></a>
## [0.4.0] - 2026-04-16

- feat/release bot setup ([#51](https://github.com/danielraffel/Shipyard/pull/51))
- docs/releasing secret drift guidance ([#46](https://github.com/danielraffel/Shipyard/pull/46))
- docs/releasing multi repo pat ([#45](https://github.com/danielraffel/Shipyard/pull/45))

<a id="v030"></a>
## [0.3.0] - 2026-04-15

- chore/bump 0.3.0 ([#44](https://github.com/danielraffel/Shipyard/pull/44))
- fix/versioning public api root files ([#43](https://github.com/danielraffel/Shipyard/pull/43))
- feat/ship resume ([#42](https://github.com/danielraffel/Shipyard/pull/42))
- commands: add /pr slash command; route through shipyard pr ([#40](https://github.com/danielraffel/Shipyard/pull/40))
- doctor + pr: surface missing RELEASE_BOT_TOKEN; doc one-time setup ([#39](https://github.com/danielraffel/Shipyard/pull/39))
- Fix Codex P1 + P2 review on PR #37 ([#38](https://github.com/danielraffel/Shipyard/pull/38))
- Fix Codex P1 + P2 review on PR #36 (versioning-sync) ([#37](https://github.com/danielraffel/Shipyard/pull/37))
- Port versioning & skill-sync gates to Shipyard + shipyard pr wrapper ([#36](https://github.com/danielraffel/Shipyard/pull/36))

<a id="v020"></a>
## [0.2.0] - 2026-04-12

- Harden Shipyard: incremental bundles, SSH resume, cloud timeout, slim README ([#35](https://github.com/danielraffel/Shipyard/pull/35))

<a id="v0114"></a>
## [0.1.14] - 2026-04-09

- Skip bundle upload when remote already has the SHA ([#30](https://github.com/danielraffel/Shipyard/pull/30))

<a id="v0113"></a>
## [0.1.13] - 2026-04-09

- Fix Windows SSH multi-line script drop (Stage 1 false-green root cause) ([#29](https://github.com/danielraffel/Shipyard/pull/29))

<a id="v0112"></a>
## [0.1.12] - 2026-04-09

- Release v0.1.12: Windows false-green fix (mutex wrapper + nested overrides) ([#28](https://github.com/danielraffel/Shipyard/pull/28))
- Fix: Windows false-green (mutex wrapper + nested platform overrides) ([#27](https://github.com/danielraffel/Shipyard/pull/27))

<a id="v0111"></a>
## [0.1.11] - 2026-04-09

- Release v0.1.11: Codex P1 batch (apply timeout + fallback kwargs + quoting + streaming cap) ([#26](https://github.com/danielraffel/Shipyard/pull/26))
- Fix Codex review P1 batch (via RepoPrompt) — before Stage 1 attempt 5 ([#25](https://github.com/danielraffel/Shipyard/pull/25))

<a id="v0110"></a>
## [0.1.10] - 2026-04-09

- Release v0.1.10: bundle upload 30min timeout + per-target override ([#24](https://github.com/danielraffel/Shipyard/pull/24))
- Fix: upload_bundle timeout raised to 30min + per-target override ([#23](https://github.com/danielraffel/Shipyard/pull/23))
- CI: bump actions to Node.js 24 compatible versions ([#22](https://github.com/danielraffel/Shipyard/pull/22))

<a id="v019"></a>
## [0.1.9] - 2026-04-09

- Release v0.1.9: Windows bundle path defaults to home-relative ([#21](https://github.com/danielraffel/Shipyard/pull/21))
- Fix: Windows bundle path defaults to home-relative, not C:\Temp ([#20](https://github.com/danielraffel/Shipyard/pull/20))

<a id="v018"></a>
## [0.1.8] - 2026-04-09

- Release v0.1.8: bundle apply into refs/shipyard-bundles/* ([#19](https://github.com/danielraffel/Shipyard/pull/19))
- Fix: apply_bundle must fetch into refs/shipyard-bundles/*, not refs/* ([#18](https://github.com/danielraffel/Shipyard/pull/18))

<a id="v017"></a>
## [0.1.7] - 2026-04-09

- Release v0.1.7: executor kwargs + branch create local-ref + ship --json fixes ([#17](https://github.com/danielraffel/Shipyard/pull/17))
- Fix: every executor must accept resume_from + mode kwargs ([#16](https://github.com/danielraffel/Shipyard/pull/16))

<a id="v016"></a>
## [0.1.6] - 2026-04-09

- Release v0.1.6: branch apply --create + ship --base auto-create-base ([#15](https://github.com/danielraffel/Shipyard/pull/15))
- Phase 6 final: branch apply --create + ship --base auto-create-base ([#14](https://github.com/danielraffel/Shipyard/pull/14))

<a id="v015"></a>
## [0.1.5] - 2026-04-09

- Release v0.1.5: Codex fixes + Phase 6 follow-ups ([#13](https://github.com/danielraffel/Shipyard/pull/13))
- Phase 6 follow-ups: governance use / export / apply --from snapshot ([#12](https://github.com/danielraffel/Shipyard/pull/12))
- Fix Codex review findings on PRs #3/#5/#8/#10 + README differentiator framing ([#11](https://github.com/danielraffel/Shipyard/pull/11))
- README: drop release-roadmap table, trim governance Commands to reality ([#10](https://github.com/danielraffel/Shipyard/pull/10))

<a id="v014"></a>
## [0.1.4] - 2026-04-09

- Release v0.1.4: governance profiles + branch protection CLI (Phase 6 minimal) ([#9](https://github.com/danielraffel/Shipyard/pull/9))
- Phase 6: governance profiles (solo/multi) + branch protection CLI ([#8](https://github.com/danielraffel/Shipyard/pull/8))

<a id="v013"></a>
## [0.1.3] - 2026-04-09

- Release v0.1.3: Phase 5 capability parity with Pulp's local_ci.py ([#7](https://github.com/danielraffel/Shipyard/pull/7))
- Phase 5 (4/4): auto-inject cloud fallback for unreachable SSH targets ([#6](https://github.com/danielraffel/Shipyard/pull/6))
- Phase 5 (3/4): Windows host mutex + Visual Studio auto-detection ([#5](https://github.com/danielraffel/Shipyard/pull/5))
- Phase 5 (2/4): prepared-state reuse for warm validation re-runs ([#4](https://github.com/danielraffel/Shipyard/pull/4))
- Phase 5 (1/4): validation contract markers + LocalExecutor enforcement ([#3](https://github.com/danielraffel/Shipyard/pull/3))
- README: add Security & Governance Profiles section + Astral attribution ([#2](https://github.com/danielraffel/Shipyard/pull/2))

<a id="v012"></a>
## [0.1.2] - 2026-04-09

- feat/shipyard phases 1 4 ([#1](https://github.com/danielraffel/Shipyard/pull/1))

[0.31.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.31.0
[0.30.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.30.0
[0.29.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.29.0
[0.28.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.28.0
[0.27.2]: https://github.com/danielraffel/Shipyard/releases/tag/v0.27.2
[0.27.1]: https://github.com/danielraffel/Shipyard/releases/tag/v0.27.1
[0.27.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.27.0
[0.26.2]: https://github.com/danielraffel/Shipyard/releases/tag/v0.26.2
[0.26.1]: https://github.com/danielraffel/Shipyard/releases/tag/v0.26.1
[0.26.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.26.0
[0.25.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.25.0
[0.24.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.24.0
[0.23.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.23.0
[0.22.9]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.9
[0.22.8]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.8
[0.22.7]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.7
[0.22.6]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.6
[0.22.5]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.5
[0.22.4]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.4
[0.22.3]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.3
[0.22.2]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.2
[0.22.1]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.1
[0.22.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.22.0
[0.21.2]: https://github.com/danielraffel/Shipyard/releases/tag/v0.21.2
[0.21.1]: https://github.com/danielraffel/Shipyard/releases/tag/v0.21.1
[0.21.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.21.0
[0.20.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.20.0
[0.19.1]: https://github.com/danielraffel/Shipyard/releases/tag/v0.19.1
[0.19.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.19.0
[0.18.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.18.0
[0.17.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.17.0
[0.16.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.16.0
[0.15.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.15.0
[0.14.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.14.0
[0.13.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.13.0
[0.12.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.12.0
[0.11.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.11.0
[0.10.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.10.0
[0.9.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.9.0
[0.8.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.8.0
[0.7.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.7.0
[0.6.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.6.0
[0.5.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.5.0
[0.4.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.4.0
[0.3.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.3.0
[0.2.0]: https://github.com/danielraffel/Shipyard/releases/tag/v0.2.0
[0.1.14]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.14
[0.1.13]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.13
[0.1.12]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.12
[0.1.11]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.11
[0.1.10]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.10
[0.1.9]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.9
[0.1.8]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.8
[0.1.7]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.7
[0.1.6]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.6
[0.1.5]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.5
[0.1.4]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.4
[0.1.3]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.3
[0.1.2]: https://github.com/danielraffel/Shipyard/releases/tag/v0.1.2
