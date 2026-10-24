# WASI interfaces used by the scaffold

The top-level WIT files in `deps/http`, `deps/cli`, `deps/io`, and
`deps/clocks` are from the **WebAssembly System Interface (WASI) Preview 2,
version 0.2.0** specification:

https://github.com/WebAssembly/WASI/tree/70214b878af4ce45889b4ad9d26a7ac98db8931b/preview2
(tag `v0.2.0`).

Files: `http/{handler,proxy,types}.wit`, `cli/{environment,imports}.wit`,
`io/{error,poll,streams,world}.wit`, and
`clocks/{monotonic-clock,wall-clock,world}.wit`.

Local modifications: `cli/imports.wit` retains only the `environment` import;
`http/proxy.wit` replaces the upstream proxy world with an `imports` world
containing only the `incoming-handler` import. The other ten files are unchanged.
The scaffold copies only these twelve files, not nested dependency directories.

These interfaces retain the upstream terms below; the repository's MIT license
does not replace those terms. Upstream notice from
[LICENSE.md](https://github.com/WebAssembly/WASI/blob/70214b878af4ce45889b4ad9d26a7ac98db8931b/LICENSE.md):

Copyright © 2019-2023 the Contributors to the WASI Specification, published
by the [WebAssembly Community Group][cg] under the
[W3C Community Contributor License Agreement (CLA)][cla]. A human-readable
[summary][summary] is available.

[cg]: https://www.w3.org/community/webassembly/
[cla]: https://www.w3.org/community/about/agreements/cla/
[summary]: https://www.w3.org/community/about/agreements/cla-deed/
