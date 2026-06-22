# TRX64 Oracle — protocol-level differential test harness

The single mechanism that defines Phase-1 done. Because **both** daemons speak the
same WS protocol on port 4312, one rig tests protocol conformance AND cycle parity.

## Contract

```
                 same WS command sequence
   harness ──────────────┬───────────────────┐
                         ▼                    ▼
                  TS daemon (oracle)     TRX64 daemon
                  :4312 (npm run         :43xx (cargo run
                   runtime:daemon)        -p trx64-daemon)
                         │                    │
                  WS responses + .c64retrace  WS responses + .c64retrace
                         └────────► DIFF ◄─────┘
                                     │
                         first-divergence report
                       (cycle, field, expected vs got)
```

## What it does

1. Boot the TS runtime daemon (golden oracle) and the TRX64 daemon on distinct ports.
2. For each scenario in the corpus: send an identical WS JSON-RPC command sequence to
   both (session/create, load, trace/start, run, trace/finalize, queries...).
3. Assert **WS responses equal** (shape + values, modulo whitelisted volatile fields
   like sessionId/paths) and **`.c64retrace` byte-identical**.
4. On mismatch, emit the FIRST divergence only: cycle, event family, field, expected
   vs got. That report is the feedback signal the builder fixes against.

## Corpus (`tools/oracle/corpus/`)

Slices, so each subsystem gates independently:
- `cpu/`  — opcode exercisers incl. illegals, IRQ/NMI timing.
- `vic/`  — raster splits, badlines, BA-low, sprite timing.
- `cia/`  — timer A/B, TOD, interrupt cascades.
- `drive/`— IEC load, GCR.
- `full/` — real PRGs end-to-end.

## Status

**wip** (Stage 0, first backlog item). The TS daemon already exposes the full WS
surface (`runtime-daemon-client.ts`); reuse it as the oracle driver. Implementation
language: TS (reuse the existing WS client) — build out from this contract.
