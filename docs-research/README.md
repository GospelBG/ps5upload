# AVA1 research and reviews

One directory per review session, numbered in order and named after the `ava1` commit the
session reviewed. Files inside are numbered in reading order. Written on the `ava1-design`
branch by the cloud research session; the local implementation sessions read them and act.

| dir | ava1 head reviewed | contents |
|---|---|---|
| `001-ava1-e64002f/` | e64002f | round-1 design review (Sonnet), round-2 design review (Fable 5.1), 7z/RAR archive-source research for Task 11 |
| `002-ava1-f660dcc/` | f660dcc | review of e64002f..f660dcc: sequential sources, 7z/RAR uploads, skip-existing, download tune, RPC limits, P3 management transport |
| `003-ava1-f660dcc/` | f660dcc | 01 whole-system review against FTX2 and the state of the art; designs: 02 durable-by-log small files, 03 lane receive path and governor, 04 stored zip downloads with resume, 05 group-level delta (v1.1); 06 consolidated SPEC/schema change list in apply order |

Each new session adds `NNN-ava1-<short sha>/` with its own numbered notes and a row here.
