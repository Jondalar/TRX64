// A scenario = an ordered sequence of WS JSON-RPC calls replayed identically against
// both daemons, plus whether to capture & diff the resulting .c64retrace.

export type Slice = "cpu" | "vic" | "cia" | "drive" | "full";

export interface Step {
  /** Literal WS JSON-RPC method string, e.g. "session/create", "session/run". */
  method: string;
  /** Params object as sent on the wire. */
  params?: Record<string, unknown>;
  /** Capture this response into the golden/candidate output set for diffing. */
  capture?: boolean;
  /** Optional label for the divergence report. */
  label?: string;
}

export interface Scenario {
  name: string;
  slice: Slice;
  /** Tiny PRG as hex bytes or a path under corpus/, loaded by the relevant step. */
  prg?: string;
  steps: Step[];
  /** Capture + diff the .c64retrace produced by this scenario's run. */
  trace?: boolean;
}

/** Golden recording for a scenario: captured responses + decoded trace. */
export interface Golden {
  scenario: string;
  responses: Array<{ label: string; method: string; result: unknown }>;
  trace?: import("./diff.js").TraceRecord[];
}
