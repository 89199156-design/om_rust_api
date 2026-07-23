# Native TurboPFor Decoder

The Python download gateway can parse OM metadata and plan LUT/data byte ranges, but OM v3 compressed LUT blocks require the native TurboPFor `p4nddec64` decoder.

Runtime contract:

```bash
export OM_TURBOPFOR_LIB=/opt/1panel/apps/weather_om_downloader/native/libom_turbopfor.so
```

The shared library must export:

```c
size_t p4nddec64(unsigned char *in, size_t n, uint64_t *out);
```

Deployment boundary:

- This is not an Open-Meteo container.
- This is not a Swift/Vapor service.
- Silicon Valley still runs the Python download gateway only.
- The native library is a small decoder dependency used only to decode compressed OM LUT offsets.

Compliance note:

The downloader build selects TurboPFor source files whose upstream headers
state GPL version 2 or later from the exact revision pinned in
`downloader/scripts/build_turbopfor_decoder.sh`. Keep this library on the
server only. Do not include it in a client, SDK, container sent to a third
party, or other native distribution without preserving upstream source and
notices and completing a release-specific review.

The point API uses a different native artifact: it compiles the complete
`om-file-format` C library, whose upstream FFI package declares
`GPL-2.0-only`. Compatibility between that artifact and the API's
`AGPL-3.0-or-later` code is unresolved. Dynamic loading and documentation do
not resolve the question. Client or third-party distribution of that artifact
is prohibited by project policy pending upstream clarification or qualified
legal review. See `THIRD_PARTY_NOTICES.md`.

Suggested build flow:

1. Review and approve the license/compliance posture.
2. Run `scripts/build_turbopfor_decoder.sh` on the target Ubuntu server or in a compatible build environment.
3. Set `OM_TURBOPFOR_LIB` in the 1Panel task command environment.
4. Run the local test suite before enabling production downloads.
