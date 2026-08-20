# Third-party notices

## Rust dependencies

The exact Rust dependency versions are recorded in `Cargo.lock`. Release
bundles include their complete source distributions and upstream license files
under `source/vendor/`, plus an offline Cargo source configuration. This keeps
the binary bundle's corresponding source self-contained instead of relying on
the crates.io service at rebuild time.

## windows-iso-downloader / MSDL

LSW's Rust implementation of the Microsoft Windows ISO session flow is based
on the request sequence documented and implemented by
[windows-iso-downloader/MSDL](https://github.com/starkSV/windows-iso-downloader),
reviewed at commit `f6659fcc42adef041c1b9b34de6188053debdc4b`.

LSW does not bundle the MSDL Go binary, call its backend, use its telemetry, or
use its crowdsourced cache. The adapted session-flow portions remain subject to
the following notice:

> MIT License
>
> Copyright (c) 2026 TechLatest (https://tech-latest.com)
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.
