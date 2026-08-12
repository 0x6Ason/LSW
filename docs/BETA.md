# LSW 1.0 beta 狀態

`1.0.0-beta.1` 是可供工程驗證的 Linux x86_64 beta，不是宣稱所有目標硬體都已通過
的 GA。它已把 instance、microVM lifecycle、Windows agent 與合法邊界內的 Setup
automation 串成一條完整路徑。

## 已完成並在本環境驗證

- manifest v2、v1 migration、default instance、私有 state permissions 與 256-bit token
- guided／unattended Setup seed；XML 可解析，且實際包含 Windows x64 PE agent
- QEMU/KVM/TCG command planning、OVMF、vTPM、NVMe、e1000e、VGA、private VNC socket
- `lswd` protocol、QMP negotiation、狀態查詢、powerdown/quit 與 child/helper supervision
- `lsw shell`、`exec`、`run`、`push`、`pull` 的 binary protocol
- 實際 loopback agent E2E：stdout/stderr、guest exit code、中文檔名/內容與 SHA-256 一致
- 並行 E2E：長命令執行期間，另一個 session 可立即完成
- Linux native build、Windows GNU PE32+ cross-build、unit tests、rustfmt 與 clippy gate

## 需要真實 host／Windows 驗證的 gate

這台 Codex VPS 沒有 `/dev/kvm`、QEMU、OVMF、swtpm、Windows ISO 或圖形 desktop，
而且 sandbox 不允許建立一般 filesystem Unix socket。因此以下程式路徑已實作及由
unit/planner test 覆蓋，但不能在此環境宣稱實機通過：

- KVM cold boot、Windows 11 Setup、OOBE、第一次登入與 HKCU agent autorun
- 各 Linux distribution 的 OVMF/secure-variable 路徑差異
- Windows firewall rule、QEMU slirp 的 `10.0.2.2` source matching
- QMP 對真實 QEMU 的 graceful/forced stop，以及 vTPM 長時間穩定性
- private Unix-socket VNC viewer 相容性
- ephemeral overlay 在真實 Windows shutdown/crash 下的反覆啟停

## Beta 已知限制

- Host 僅支援 Linux x86_64；Windows/macOS host backend 尚未實作。
- Guest agent 是 pipe session，尚無 ConPTY，所以互動式 console mode、Ctrl 事件與 resize
  不完整。`pwsh`/PowerShell/CMD fallback 與非互動 build command 可用。
- 尚無 per-HWND Wayland/X11 compositor bridge、clipboard、audio、GPU acceleration 或
  shared-memory graphics driver。`lsw run` 會啟動 guest 程式，但不會產生 Linux native
  application window。
- 安裝及救援仍需 private Unix-socket VNC；沒有 RDP，也沒有 TCP VNC listener。
- 沒有 suspend/resume、live migration、host folder、USB passthrough 或 image export。
- Agent authentication 尚未加密，只能用於設計中的本機 loopback/QEMU user-network path。
- QEMU 尚未套用 LSW 專屬的 seccomp/namespace/service-account sandbox。
- `secure` profile 必須由使用者提供 distribution 正確的 key-enrolled OVMF code/vars。
- LSW 程式碼採 `GPL-3.0-or-later`；發行 binary bundle 內含精確對應的 source snapshot。

## 建議的實機驗收順序

1. 在有 KVM 的 Linux x86_64 測試機執行 `lsw doctor`。
2. 使用未修改、已授權的 Windows 11 x64 ISO 建立 `standard` instance。
3. 先測 guided install，再以 disposable instance 測 `--unattended-index`。
4. 完成 OOBE/first logon，測 `lsw`、exit-code propagation、1 GiB file transfer 及並行命令。
5. 測 graceful stop、guest crash、daemon restart 與 stale-socket recovery。
6. 分別測 `offline`、`ephemeral` 與 key-enrolled `secure` profile。

只有完成上述硬體 matrix 後，才應把這個 beta 升格為 GA。
