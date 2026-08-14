# LSW 1.0 beta 狀態

`1.0.0-beta.5` 是可供工程驗證的 Linux x86_64 beta，不是宣稱所有目標硬體都已通過
的 GA。它已把 instance、microVM lifecycle、Windows agent 與合法邊界內的 Setup
automation 串成一條完整路徑。

## 已完成並可由 source／CI gate 驗證

- manifest v4、v1/v2/v3 migration、default instance、私有 state permissions、256-bit token
  與 loopback TCP port publishing validation
- guided／unattended Setup seed；XML 可解析，且實際包含 Windows x64 PE agent
- QEMU backend selection；Linux KVM detection，以及 KVM／TCG／HVF／WHPX command planning
- OVMF、vTPM、NVMe、e1000e、VGA、private VNC socket 與 NAT loopback host forwarding
- `lswd` protocol、QMP negotiation、狀態查詢、in-memory suspend/resume、powerdown/quit 與
  child/helper supervision
- `lsw shell`、`exec`、`run`、`push`、`pull` 的 binary protocol；以及 capability-gated
  `session-control-v1` 的 explicit stdin EOF、cancel/disconnect cleanup 與 legacy fallback；
  capability-gated `session-lease-v1` 提供 1–300 秒 lease，標準 120 秒／30 秒 heartbeat
- Unix child 在 `exec` 前進入獨立 process group，所有仍留在 group 內的 processes 會在
  leader 正常結束、取消、斷線、協定錯誤或 lease expiry 時清除；Windows child 則在 resume
  前強制加入 kill-on-close Job Object，建立／加入失敗時 fail closed
- ConPTY capability negotiation、console I/O bridge、TTY restore 與 resize protocol/unit tests
- `lsw inspect` 的 bounded PE parser、imports/JSON output 及 x64 guest 相容性提示
- 實際 loopback agent E2E：stdout/stderr、guest exit code，以及中文檔名與 binary 內容逐 byte 一致
- 並行 E2E：長命令執行期間，另一個 session 可立即完成
- 受控 session loopback E2E：stdin close 正常 EOF、authenticated cancel 回傳 130、
  disconnect/lease expiry 釋放仍在 owned group 的 processes、malformed frame/authentication
  rejection、heartbeat liveness 及 legacy half-close
- 在無 KVM／Windows media 的 Codex VPS，以 Ubuntu 官方暫存套件實際跑通 QEMU 8.2.2
  TCG + OVMF、`qemu-img`、swtpm/vTPM command traffic、TCP QMP
  `stop`/`cont`/`quit`，以及兩個通往 guest 5040/8080 的 `127.0.0.1` usernet hostfwd
  endpoint；兩者在 QEMU quit 後釋放，QEMU／swtpm 皆以 status 0 結束
- CI 另有不允許 skip 的 product lifecycle gate：用真正的 `lsw` manifest、preparation、
  `QemuPlanner` 與 `lswd` 驗證 OVMF、NVMe、e1000e、vTPM、兩種 loopback hostfwd，以及
  install/start/status/suspend/resume/forced-stop；它使用 placeholder 而非 Windows media
- Rust 1.76 Linux native build、Windows GNU PE32+ cross-build、unit tests、rustfmt、clippy、
  shell checks 與 release-bundle verification；CI 另設 Windows MSVC native agent tests／
  executable load gate，以及 timeout-bounded QEMU firmware/product lifecycle gates

上面的 ConPTY 項目指協定、host bridge、Windows binary cross-build 與可在此環境執行的
測試；Windows MSVC native Job/ConPTY tests 是 workflow gate，不代表這台 Linux VPS 已
執行該 job，更不代表已在登入後的真 Windows console session 實際操作過互動 shell。

## 需要真實 host／Windows 驗證的 gate

這台 Codex VPS 沒有 `/dev/kvm`、Windows ISO 或圖形 desktop，sandbox 也不允許建立
一般 pathname Unix socket（AF_UNIX `bind` 回傳 `EPERM`）。QEMU／OVMF／swtpm 原本未
預裝，但以上述暫存 Ubuntu 官方套件完成了 firmware-level TCG smoke。真實 LSW planner
與 daemon 的 host-side lifecycle 由 CI 的 pathname-socket product gate 負責，本 VPS 沒有
執行該 gate；以下 Windows 工作負載仍不能宣稱實機通過：

- KVM cold boot、Windows 11 Setup、OOBE、第一次登入與 HKCU agent autorun；以及
  完整 shutdown 後，在不再手動登入、不掛載 ISO/seed 的情況下用裸 `lsw`
  恢復 agent-backed ConPTY shell
- 各 Linux distribution 的 OVMF/secure-variable 路徑差異
- Windows firewall rule、QEMU slirp 的 `10.0.2.2` source matching
- 低階 loopback hostfwd listener 已驗證；`--publish` 對真實 guest TCP service 的資料傳輸
  及多 instance 長時間使用尚未驗證
- TCP QMP 的 `stop`/`cont`/`quit` 已對真實 QEMU 驗證，CI product gate 也會以 filesystem
  QMP socket 驗證 `lswd` 的 suspend/resume/forced stop；管理真正 Windows workload 的
  graceful/forced stop 與 vTPM soak 尚未驗證
- ConPTY shell 的 Unicode、Ctrl 事件、resize、斷線與長時間互動行為
- private Unix-socket VNC viewer 相容性
- ephemeral overlay 在真實 Windows shutdown/crash 下的反覆啟停
- macOS HVF／Windows WHPX host 的 executable、路徑、firmware、daemon IPC 與完整 lifecycle

## Beta 已知限制

- 可交付的 host runtime 仍僅支援 Linux x86_64。backend abstraction 已能選擇 HVF/WHPX
  並產生 QEMU acceleration argv，但 Windows/macOS host integration 尚未實作及驗證。
- ConPTY transport 已實作並由 capability 協商；不支援或較舊的 agent 會回退至 pipe
  session。真 Windows guest 的 console mode、Ctrl 事件、Unicode 與 resize 仍是 E2E gate。
- `session-control-v1` 能區分 stdin EOF、authenticated cancel 及啟用選項後的 disconnect；
  `session-lease-v1` 為 opted-in half-open peer 提供有界回收，且清理不再只針對 leader。
  Unix group ownership 只能回收仍在該 group 的 ordinary descendants；guest process 可用
  `setsid`／`setpgid` 逃離；正常 leader 被 reap 後的 group cleanup 也仍有極小的數字 PGID
  reuse race。Windows Job Object 提供強制 ownership，但若 host 的 nested-job
  policy 不允許 assign，session 會 fail closed。兩者都不是 guest security sandbox。
- 尚無 per-HWND Wayland/X11 compositor bridge、clipboard、audio、GPU acceleration 或
  shared-memory graphics driver。`lsw run` 會啟動 guest 程式，但不會產生 Linux native
  application window。
- 安裝及救援仍需 private Unix-socket VNC；沒有 RDP，也沒有 TCP VNC listener。
- suspend/resume 只對仍在執行的 QEMU process 使用 QMP `stop`/`cont`；沒有 RAM
  save-to-disk、跨 host 復原、live migration、host folder、USB passthrough 或 image export。
- Agent authentication 尚未加密，只能用於設計中的本機 loopback/QEMU user-network path。
- `--publish` 只建立 `127.0.0.1` TCP listener，但 guest service 本身仍需視為不受信任；
  不應再透過其他工具把它轉送到 LAN／Internet。
- QEMU 尚未套用 LSW 專屬的 seccomp/namespace/service-account sandbox。
- `secure` profile 必須由使用者提供 distribution 正確的 key-enrolled OVMF code/vars。
- LSW 程式碼採 `GPL-3.0-or-later`；發行 binary bundle 內含精確對應的 source snapshot。

## 建議的實機驗收順序

1. 在有 KVM 的 Linux x86_64 測試機執行 `lsw doctor`。
2. 使用未修改、已授權的 Windows 11 x64 ISO 建立 `standard` instance。
3. 先測 guided install，再以 disposable instance 測 `--unattended-index`。
4. 完成 OOBE/first logon，測 ConPTY shell（Unicode、Ctrl、resize、stdin EOF、取消、
   斷線與 lease expiry）、exit-code propagation、descendant cleanup、1 GiB file transfer
   及並行命令；再完整 shutdown、關閉 viewer，以裸 `lsw` 驗證無第二次
   手動登入的冷啟動恢復。
5. 發布 disposable guest TCP service，確認 listener 只在 `127.0.0.1`，並測 port collision。
6. 測 suspend/resume、graceful stop、guest crash、daemon restart 與 stale-socket recovery。
7. 分別測 `offline`、`ephemeral` 與 key-enrolled `secure` profile。

只有完成上述硬體 matrix 後，才應把這個 beta 升格為 GA。
