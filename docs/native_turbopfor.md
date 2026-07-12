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

Open-Meteo `om-file-format` includes TurboPFor source files whose headers mention GPL v2. Before production use in a commercial deployment, confirm whether compiling and using this native decoder on the server is acceptable for the product's licensing posture. Do not redistribute the native library or bundled source without legal review.

Suggested build flow:

1. Review and approve the license/compliance posture.
2. Run `scripts/build_turbopfor_decoder.sh` on the target Ubuntu server or in a compatible build environment.
3. Set `OM_TURBOPFOR_LIB` in the 1Panel task command environment.
4. Run the local test suite before enabling production downloads.
