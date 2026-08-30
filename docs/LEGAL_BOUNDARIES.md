# Legal and distribution boundaries

This is a product constraint document, not legal advice.

LSW's original code is licensed under `GPL-3.0-or-later`. That license applies
to LSW and derivatives of LSW; it does not grant rights to proprietary operating
systems, firmware, hypervisors, installation media, trademarks, or user data.

- LSW distributes orchestration and its own guest agent. It can download an
  official ISO directly from allowlisted Microsoft HTTPS CDNs and verify the
  SHA-256 Microsoft publishes, but it does not redistribute Windows, macOS,
  product keys, activation tokens, preactivated disks, modified third-party
  ISOs, or vendor SDKs.
- Release bundles include the locked Rust dependency sources and their upstream
  license files under `source/vendor`; `THIRD_PARTY_NOTICES.md` records the
  attribution and corresponding-source boundary.
- Users remain responsible for license entitlement, activation, edition and
  VM/device limits. `--iso` accepts user-supplied authorized offline media.
- A new interactive installation requires `[y/N]` confirmation of the
  applicable Microsoft Windows license terms. Noninteractive use fails unless
  `--accept-windows-license` is present. Only after that explicit action may the
  generated answer file set the documented Windows Setup `AcceptEula` value.
  Users can review the [Microsoft licensing
  documents](https://aka.ms/licensingdocs); the terms supplied with the media or
  applicable retail/volume agreement control. LSW does not grant a Windows
  license or activation entitlement.
- The beta does not add a `ProductKey`, bypass activation, use the deprecated
  `SkipMachineOOBE` setting, or disable UAC. The user accepts the media's license
  terms before install. LSW then uses Microsoft's supported unattended OOBE
  settings and a random one-shot local account, which is removed before setup
  is marked complete. Activation remains the operating system's responsibility.
- Optional activation installs a user-provided key only through Windows WMI.
  The key is accepted through masked input or stdin and is excluded from host
  and guest command lines, environment, seeds, base images, logs and diagnostic
  bundles.
- The default installer resolves the edition by supported WIM metadata.
  `--unattended-index` remains an advanced compatibility selector. The WinPE
  prepare phase cleans only a new private workspace; the apply phase cleans only
  the instance's dedicated target qcow2. Neither plan accepts a host block
  device.
- The `slim-v2` recipe runs locally against the user's authorized media. It
  performs only typed, exact-identity AppX/feature removal, the inbox OneDrive
  uninstaller, and bounded service/policy configuration while preserving the
  declared servicing and development boundary. Its audit report does not grant
  redistribution rights. Generated disks stay local unless the user separately
  has the right to distribute them.
- LSW neither bundles nor endorses Tiny11/Tiny10 images. A user-supplied modified
  image may be technically bootable, but provenance, redistribution rights,
  security and support remain with its supplier and user. Official beta testing
  targets unmodified, authorized Windows installation media.
- No beta profile auto-installs a self-signed certificate or custom driver. A
  future driver workflow must not weaken the host and must keep its private key
  out of redistributable images.
- Any future macOS guest backend must enforce compatible Apple hardware and the
  applicable license and must use user-supplied authorized media. A VM or
  container-like UI does not remove those terms.
- Export/image-sharing features must exclude proprietary media, keys,
  certificates, tokens, credentials and source code by default.

Windows, macOS, Apple, Microsoft and other names belong to their respective
owners. LSW should describe compatibility without implying vendor endorsement.

LSW's GPL-3.0-or-later license is not a separate product EULA and does not need
a synthetic click-through acceptance prompt. The installer identifies that
license and the bundled third-party notices independently from Windows terms.
