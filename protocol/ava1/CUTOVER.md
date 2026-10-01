# AVA1 cutover checklist (documentation and tooling)

The `ava1` branch's user-facing docs already describe AVA1. These mentions of
FTX2 or ports 9113/9114 name tooling or settings that still exist on the branch
and change when that code does. The cutover release (project 3) ships only when
every box is ticked and `git grep -i ftx2 -- '*.md' ':!CHANGELOG.md'` is empty.

The CHANGELOG keeps its FTX2 entries: they describe releases that shipped FTX2.

- [ ] `README.md` "Test" section — "in-process mock FTX2 server" → the AVA1 mock/host-C tests
- [ ] `CONTRIBUTING.md:45` — "mock-FTX2 integration tests"
- [ ] `engine/README.md` — `ps5upload-tests` row ("mock FTX2 server"); dev commands using `:9113` / `:9114`
- [ ] `TESTING.md` — `PS5_ADDR=…:9113`, `make validate` waiting for `:9113`, curl examples with `:9114`
- [ ] `tests/README.md` — "full FTX2 stack", `PS5_ADDR` default `:9113`, `--ps5-addr` description
- [ ] `tests/lab/README.md` — `:9113`/`:9114`, `ftx2_control.py`, `ftx2_probe.py`
- [ ] `bench/README.md` — `run-ftx2-upload.mjs`, `check-ftx2-baseline.mjs`, `ftx2-upload-main.json` baselines, `--ps5-addr=…:9113`
- [ ] `FAQ.md` — `FTX2_ZIP_RAM_THRESHOLD_MB` and `FTX2_ARCHIVE_STAGE_MB` environment variables (rename to `PS5UPLOAD_*` and accept the old names for one release)
- [ ] In-app strings (`client/src/i18n/locales/*.ts`) that mention FTX2, ports 9113/9114 or "transfer port"
