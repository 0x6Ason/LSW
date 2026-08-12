# Legal and distribution boundaries

This is a product constraint document, not legal advice.

LSW's original code is licensed under `GPL-3.0-or-later`. That license applies
to LSW and derivatives of LSW; it does not grant rights to proprietary operating
systems, firmware, hypervisors, installation media, trademarks, or user data.

- LSW distributes orchestration and its own guest agent. It does not distribute
  Windows, macOS, product keys, activation tokens, preactivated disks, modified
  third-party ISOs, or vendor SDKs.
- Users obtain installation media from an authorized source and remain
  responsible for license entitlement, activation, edition and VM/device limits.
- `--accept-license` records the user's explicit confirmation for their supplied
  media. The generated answer file may set the documented Windows Setup
  `AcceptEula` value from that confirmation; LSW does not silently infer consent.
- The beta does not add a `ProductKey`, bypass activation, hide required license
  pages, use `SkipMachineOOBE`, pre-create a user, or disable UAC. Normal OOBE and
  activation remain the operating system's responsibility.
- `--unattended-index` automates supported Setup disk/image selection and wipes
  only the instance's dedicated virtual Disk 0. The guided path is the default.
- The `slim` recipe runs locally and removes only an explicit list of optional
  provisioned AppX packages. It preserves Windows servicing and development
  prerequisites. Generated disks stay local unless the user separately has the
  right to distribute them.
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
