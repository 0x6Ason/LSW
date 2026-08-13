# LSW 1.0 beta

LSW 是 Linux 上的本機 Windows 開發執行環境。操作方式接近容器：選定一個預設
instance 後，可直接進入 PowerShell／CMD、執行 Windows 程式及傳輸檔案；實際隔離
邊界則是 QEMU/KVM microVM，因為 Windows 核心不能與 Linux host 共用。

目前版本為 `1.0.0-beta.2`。CLI、QMP 生命週期、Windows guest agent、安裝 seed、
檔案傳輸、ConPTY 終端機與 ephemeral overlay 已實作。逐視窗 Wayland/X11 整合尚未
完成，因此 GUI 安裝／救援暫時使用 private Unix-socket VNC。ConPTY 及真正的
Windows/KVM 開機路徑仍需在實機完成 E2E gate；詳見 [beta 狀態](docs/BETA.md)。

## 支援範圍

- Host runtime：Linux x86_64；KVM 建議使用，TCG 只適合診斷。程式碼已分離 backend
  選擇，並能產生 HVF／WHPX 參數，但 macOS／Windows host 尚未成為可交付 runtime。
- Guest：使用者自行提供及授權的 Windows 11 x64 安裝 ISO。
- Runtime：QEMU、`qemu-img`、OVMF 及 swtpm。
- LSW 不下載或散布 Windows、產品金鑰、啟用資料、預先啟用磁碟或 Tiny11 image。

「container-like」指 UX 與 image lifecycle，不代表可達到 Linux namespace 容器的
記憶體密度或冷啟動速度。

## 快速開始

先安裝發行 bundle，再檢查 host：

```bash
./install.sh
lsw doctor
```

建立 instance。`--accept-license` 只記錄你已接受自己所提供媒體的授權條款：

```bash
lsw create win-dev \
  --iso /absolute/path/Windows11.iso \
  --profile slim \
  --accept-license
```

若需要把 guest TCP service 發布給 host，可重複使用 `--publish HOST:GUEST`：

```bash
lsw create win-web \
  --iso /absolute/path/Windows11.iso \
  --publish 8080:80 \
  --publish 8443:443 \
  --accept-license
```

這些 port 只綁定 host 的 `127.0.0.1`，不會建立 LAN listener；`offline` network 不允許
額外 port publishing。LSW 會拒絕重複、已由其他 instance 使用、目前無法在 loopback
綁定，或與 agent control port 衝突的 host port。

啟動安裝：

```bash
lsw install win-dev
```

發行 bundle 會自動把 `lsw-agent.exe` 放入唯讀安裝 seed。預設為 guided install：
Windows edition、磁碟選擇及正常 OOBE 仍由使用者完成。命令會印出 private VNC Unix
socket 路徑，供支援 Unix-socket VNC 的 viewer 完成安裝；它不開 TCP VNC 或 RDP。

若要自動選擇 image 並重建專用虛擬 Disk 0，必須明確指定：

```bash
lsw install win-dev --locale zh-TW --unattended-index 6
```

這個選項會清除該 instance 的虛擬 Disk 0。它不會動到 host disk，但仍應先核對
instance 名稱及 Windows ISO 中的 image index。產生的 answer file 不含產品金鑰、
啟用繞過、`SkipMachineOOBE` 或預建帳號。

完成 OOBE 並第一次以管理員使用者登入後，agent 會安裝至 user session。之後可用：

```bash
lsw use win-dev
lsw                         # pwsh -> Windows PowerShell -> cmd fallback
lsw exec -- cmd.exe /c ver
lsw run -- notepad.exe
lsw push ./main.rs 'C:\Users\you\src\main.rs'
lsw pull 'C:\build\app.exe' ./app.exe
lsw status
lsw suspend                  # QMP stop；VM 仍留在記憶體
lsw resume
lsw stop
```

在互動式 host TTY 上，`lsw shell` 會與支援 capability 的 agent 協商 ConPTY，轉送
console input/output 並同步 terminal resize；舊版或不支援 ConPTY 的 agent 仍使用 pipe
session。此實作已通過編譯及協定測試，但 beta.2 尚未在登入後的真 Windows guest
完成 E2E 驗收。

`lsw run` 已能在登入中的 Windows session 啟動程式，但 beta 尚未把個別 HWND 映射成
Linux native window；GUI 仍只能從安裝／救援 display 看到。`lsw suspend` 只是暫停目前
QEMU process 的 in-memory VM，不是休眠或磁碟 snapshot；QEMU／host 結束後不能靠它復原。

若要在建立 VM 前檢查 Windows binary，可直接分析 PE，不需要 state directory：

```bash
lsw inspect ./app.exe
lsw inspect ./app.exe --imports
lsw inspect ./app.exe --json
```

inspector 會報告 machine、subsystem、sections、imports、CLR/certificate table 與 beta
相容性提示；「certificate table present」不等於已完成 Authenticode 密碼學驗證。

## Profiles

| Profile | 行為 | Guest Secure Boot |
| --- | --- | --- |
| `standard` | 原生 Windows，保留 servicing | 關閉 |
| `slim` | 僅移除明列的 optional provisioned AppX，啟用 CompactOS | 關閉 |
| `ephemeral` | 與 slim 相同，但每次 run 使用並丟棄 qcow2 overlay | 關閉 |
| `secure` | 不允許 test-signed 自訂 driver，需要已註冊金鑰的 OVMF vars | 開啟 |

所有 beta profile 都保留 WinSxS、Windows Update／servicing stack、MSI/MSIX、Defender
及開發工具常用依賴。LSW beta 不自動開啟 test signing，也不安裝自簽憑證或自訂 driver。

## 從 source 建置

Host binaries 不含第三方 Rust crate dependency。專案的 MSRV 是 Rust 1.76，CI 也固定以
1.76 執行 source gate：

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Windows GNU target 搭配一般 MinGW linker 時可直接建置 agent：

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu --bin lsw-agent
```

沒有系統 MinGW 時，可安裝 Zig 並設定其 executable：

```bash
LSW_ZIG=/path/to/zig scripts/build-windows-agent.sh
LSW_ZIG=/path/to/zig scripts/build-release.sh
```

release script 會產生 Linux x86_64 bundle、SHA-256 檔及 Windows PE agent。專案本身不
會自動下載 Zig、Rust target 或作業系統媒體。發行包也包含該 binary 對應的完整
source snapshot 與 build scripts。

## 授權

LSW 自有程式碼採用 [GNU GPL 3.0 或任何後續版本](LICENSE)，SPDX identifier 為
`GPL-3.0-or-later`。你可以使用、研究、修改及重新散布；若散布 LSW 或其衍生版本，
必須依 GPL 向接收者提供相同自由與對應 source。這項授權不涵蓋 Windows、macOS、
QEMU、OVMF、swtpm 或使用者自行提供的媒體；它們各自適用原權利人的條款。

## 文件

- [Beta 驗收範圍與已知限制](docs/BETA.md)
- [架構](docs/ARCHITECTURE.md)
- [安全模型](docs/SECURITY.md)
- [開發與 release gate](docs/DEVELOPMENT.md)
- [授權與散布邊界](docs/LEGAL_BOUNDARIES.md)
- [主要設計參考資料](docs/REFERENCES.md)
