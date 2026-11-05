/**
 * Wire contract for the monitor snapshot.
 *
 * Owned by the producers, not by the views:
 * - `crates/agent-studio-core/src/hub.rs` (`Hub::snapshot`) — the desktop app's source.
 * - `collector/lib/hub.js` (`Hub#snapshot`) — the browser/SSE source, kept in parity
 *   by `scripts/qa-rust-parity.mjs`.
 *
 * Views must consume these shapes instead of re-casting payload fields locally.
 */

/** Status a producer can write onto a session. */
export type SessionStatus = 'unknown' | 'running' | 'wait' | 'done' | 'error' | 'aborted';

/** `idle`/`offline` never come from the wire; `sessionPresentation` derives them. */
export type PresentationStatus = SessionStatus | 'idle' | 'offline';

export type ConnectionState = 'connecting' | 'connected' | 'offline';

export type SourceState = 'ok' | 'partial' | 'disabled' | 'exited' | 'error';

export interface SessionStep {
  id: string;
  ts: number;
  label: string;
}

export interface PendingQuestionOption {
  label: string;
  description: string;
}

/** Normalized by `questionDetails()` in `collector/lib/codex.js`. */
export interface PendingQuestion {
  text: string;
  header: string;
  options: PendingQuestionOption[];
}

export interface PendingRequest {
  id: string;
  tool?: string;
  text: string;
  questions: PendingQuestion[];
  ts: number;
  /** Async Codex questions only; omitted for every other wait. */
  optional?: true;
}

export interface Session {
  id: string;
  source: string;
  sessionId: string;
  cwd: string;
  title: string;
  roundId: string;
  startedAt: number;
  updatedAt: number;
  status: SessionStatus;
  steps: SessionStep[];
  pending: PendingRequest[];
  tokens: number | null;
  endedAt: number | null;

  /**
   * 这条任务用的模型与吞吐。只有能从会话记录里读出来的适配器才有
   * （目前是 workbuddy），其余来源保持缺省。
   *
   * `speed` 是「最近 `speedSamples` 步」的平均输出速度（token/s），
   * 相邻两步的时间差里含工具执行时间，因此比真实生成速度偏低，
   * 展示时必须带「约」。
   */
  model?: string;
  /** 模型的人类可读名（`DeepSeek-V4.1-Flash`），缺失时用 `model`。 */
  modelName?: string | null;
  speed?: number | null;
  speedSamples?: number | null;

  // Present only once an adapter has observed them.
  folderId?: string | number;
  agentType?: string;
  /**
   * Which application the hook reported: `"vscode"` for the CodeBuddy VS Code
   * plugin, `"codebuddy-ide"` for the IDE. Absent on older sessions and on any
   * unknown host, both of which stay IDE hosts.
   */
  hostKind?: string;
  /** Display name of an imported `custom:<id>` source, taken from its template. */
  sourceLabel?: string;
  externalId?: string;
  webPort?: number;
  permissionChecks?: {id: string; ts: number}[];
  /**
   * Set on a Codeg session delegated by another one. Such a child only appears
   * while it waits for the user, and the rail marks it with a 子任务 badge.
   */
  subagent?: boolean;
  /** Title of the session a subagent was delegated from; absent when unreadable. */
  parentTitle?: string;
  /** Set when a host process exited; absent after a new round starts. */
  endedBy?: string;
  /** Written by the Codex read-state watcher, not by the hub ingest path. */
  viewedRoundId?: string | null;
  /** Set only on Codex sessions restored from the native bounded state file. */
  recovered?: boolean;

  // Added by the snapshot projection.
  project: string;
  stale: boolean;
  elapsed: number;
  progress: null;
}

export type SnapshotEventKind = 'wait' | 'done' | 'error';

export interface SnapshotEvent {
  id: string;
  sessionId: string;
  roundId: string;
  kind: SnapshotEventKind;
  ts: number;
  /** True for replay before the first ready poll, or for backfilled pending requests. */
  historical: boolean;
  title: string;
}

export interface SourceHealth {
  state: SourceState;
  detail: string;
  checkedAt: number;
  /** Producers spread adapter-specific extras over these three fields. */
  [extra: string]: unknown;
}

export interface Snapshot {
  version: 1;
  ready: boolean;
  ts: number;
  sources: Record<string, SourceHealth>;
  sessions: Session[];
  events: SnapshotEvent[];
}

/** `createNotificationTracker` decorates a fresh event with the time it was first seen. */
export interface MonitorAlert extends SnapshotEvent {
  receivedAt: number;
}

/** Healthy enough to trust a session's terminal state. */
export function isHealthySource(health: SourceHealth | undefined): boolean {
  return health?.state === 'ok' || health?.state === 'partial';
}
