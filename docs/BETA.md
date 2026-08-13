# LSW 1.0 beta 狀態

`1.0.0-beta.2` 是可供工程驗證的 Linux x86_64 beta，不是宣稱所有目標硬體都已通過
的 GA。它已把 instance、microVM lifecycle、Windows agent 與合法邊界內的 Setup
automation 串成一條完整路徑。

## 已完成並可由 source／CI gate 驗證

- manifest v3、v1/v2 migration、default instance、私有 state permissions、256-bit token
  與 loopback TCP port publishing validation
- guided／unattended Setup seed；XML 可解析，且實際包含 Windows x64 PE agent
- QEMU backend selection；Linux KVM detection，以及 KVM／TCG／HVF／WHPX command planning
- OVMF、vTPM、NVMe、e1000e、VGA、private VNC socket 與 NAT loopback host forwarding
- `lswd` protocol、QMP negotiation、狀態查詢、in-memory suspend/resume、powerdown/quit 與
  child/helper supervision
- `lsw shell`、`exec`、`run`、`push`、`pull` 的 binary protocol
- ConPTY capability negotiation、console I/O bridge、TTY restore 與 resize protocol/unit tests
- `lsw inspect` 的 bounded PE parser、imports/JSON output 及 x64 guest 相容性提示
- 實際 loopback agent E2E：stdout/stderr、guest exit code，以及中文檔名與 binary 內容逐 byte 一致
- 並行 E2E：長命令執行期間，另一個 session 可立即完成
- Rust 1.76 Linux native build、Windows GNU PE32+ cross-build、unit tests、rustfmt、clippy、
  shell checks 與 release-bundle verification；CI 另設 Windows MSVC executable load gate

上面的 ConPTY 項目指協定、host bridge、Windows binary build/load 與可在此環境執行的
測試；Windows MSVC load 是 workflow gate，不代表這台 Linux VPS 已執行該 job，更不代表
已在登入後的真 Windows console session 實際操作過互動 shell。

## 需要真實 host／Windows 驗證的 gate

這台 Codex VPS 沒有 `/dev/kvm`、QEMU、OVMF、swtpm、Windows ISO 或圖形 desktop，
而且 sandbox 不允許建立一般 filesystem Unix socket。因此以下程式路徑已實作及由
unit/planner test 覆蓋，但不能在此環境宣稱實機通過：

- KVM cold boot、Windows 11 Setup、OOBE、第一次登入與 HKCU agent autorun
- 各 Linux distribution 的 OVMF/secure-variable 路徑差異
- Windows firewall rule、QEMU slirp 的 `10.0.2.2` source matching
- `--publish` 對真實 guest TCP service 的 forwarding 與多 instance 長時間使用
- QMP 對真實 QEMU 的 suspend/resume、graceful/forced stop，以及 vTPM 長時間穩定性
- ConPTY shell 的 Unicode、Ctrl 事件、resize、斷線與長時間互動行為
- private Unix-socket VNC viewer 相容性
- ephemeral overlay 在真實 Windows shutdown/crash 下的反覆啟停
- macOS HVF／Windows WHPX host 的 executable、路徑、firmware、daemon IPC 與完整 lifecycle

## Beta 已知限制

- 可交付的 host runtime 仍僅支援 Linux x86_64。backend abstraction 已能選擇 HVF/WHPX
  並產生 QEMU acceleration argv，但 Windows/macOS host integration 尚未實作及驗證。
- ConPTY transport 已實作並由 capability 協商；不支援或較舊的 agent 會回退至 pipe
  session。真 Windows guest 的 console mode、Ctrl 事件、Unicode 與 resize 仍是 E2E gate。
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
4. 完成 OOBE/first logon，測 ConPTY shell（Unicode、Ctrl、resize、斷線）、exit-code
   propagation、1 GiB file transfer 及並行命令。
5. 發布 disposable guest TCP service，確認 listener 只在 `127.0.0.1`，並測 port collision。
6. 測 suspend/resume、graceful stop、guest crash、daemon restart 與 stale-socket recovery。
7. 分別測 `offline`、`ephemeral` 與 key-enrolled `secure` profile。

只有完成上述硬體 matrix 後，才應把這個 beta 升格為 GA。
