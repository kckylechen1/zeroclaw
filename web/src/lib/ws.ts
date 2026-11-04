import type { ApprovalDecision, WsMessage } from '../types/api';
import { getToken } from './auth';
import { apiOrigin, basePath } from './basePath';
import { isTauri } from './tauri';
import { generateUUID } from './uuid';

export type WsMessageHandler = (msg: WsMessage) => void;
export type WsOpenHandler = () => void;
export type WsCloseHandler = (ev: CloseEvent) => void;
export type WsErrorHandler = (ev: Event) => void;

export interface WebSocketClientOptions {
  /** Agent alias to bind this socket to (required by the gateway). */
  agentAlias: string;
  /** Base URL override. Defaults to current host with ws(s) protocol. */
  baseUrl?: string;
  /** Delay in ms before attempting reconnect. Doubles on each failure up to maxReconnectDelay. */
  reconnectDelay?: number;
  /** Maximum reconnect delay in ms. */
  maxReconnectDelay?: number;
  /** Set to false to disable auto-reconnect. Default true. */
  autoReconnect?: boolean;
}

const DEFAULT_RECONNECT_DELAY = 1000;
const MAX_RECONNECT_DELAY = 30000;

const SESSION_ID_KEY_PREFIX = 'zeroclaw_session_id';

/** Return a stable session ID for the given agent alias, persisted in
 * localStorage. Each agent gets its own session so parallel conversations
 * don't collide. */
export function getOrCreateSessionId(agentAlias: string): string {
  const key = `${SESSION_ID_KEY_PREFIX}.${agentAlias}`;
  let id = localStorage.getItem(key);
  if (!id) {
    id = generateUUID();
    localStorage.setItem(key, id);
  }
  return id;
}

export class WebSocketClient {
  private ws: WebSocket | null = null;
  private currentDelay: number;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private intentionallyClosed = false;

  public onMessage: WsMessageHandler | null = null;
  public onOpen: WsOpenHandler | null = null;
  public onClose: WsCloseHandler | null = null;
  public onError: WsErrorHandler | null = null;

  /** S2 (#549): negotiated epoch, or null before `turn_ready` / during a
   * reconnect window (onopen clears it until the new socket negotiates). */
  private turnEpoch: string | null = null;
  /** S2 (#549): latched on the first successful negotiation. Distinguishes a
   * reconnect window (epoch null but S2 in use — scoped cancellation is the
   * only Stop path) from a socket that genuinely never negotiated (legacy
   * REST-abort fallback). */
  private negotiatedOnce = false;
  /** S2 (#549): the server's replay-cache event bound from `turn_ready`
   * limits. Events with seq beyond it are live-only (never replayed), which
   * bounds the client's applied-sequence dedup set. */
  private replayEventLimit: number | null = null;
  /** Requests accepted for admission but not yet sent (pre-`turn_ready`). */
  private readonly queuedSends: { requestId: string; frame: Record<string, unknown> }[] = [];

  private readonly agentAlias: string;
  private readonly baseUrl: string;
  private readonly reconnectDelay: number;
  private readonly maxReconnectDelay: number;
  private readonly autoReconnect: boolean;

  constructor(options: WebSocketClientOptions) {
    this.agentAlias = options.agentAlias;
    let defaultBase: string;
    if (isTauri() && apiOrigin) {
      // In Tauri, derive ws URL from the gateway origin.
      defaultBase = apiOrigin.replace(/^http/, 'ws');
    } else {
      const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
      defaultBase = `${protocol}//${window.location.host}`;
    }
    this.baseUrl = options.baseUrl ?? defaultBase;
    this.reconnectDelay = options.reconnectDelay ?? DEFAULT_RECONNECT_DELAY;
    this.maxReconnectDelay = options.maxReconnectDelay ?? MAX_RECONNECT_DELAY;
    this.autoReconnect = options.autoReconnect ?? true;
    this.currentDelay = this.reconnectDelay;
  }

  /** Open the WebSocket connection. */
  connect(): void {
    this.intentionallyClosed = false;
    this.clearReconnectTimer();

    const token = getToken();
    const sessionId = getOrCreateSessionId(this.agentAlias);
    const params = new URLSearchParams();
    if (token) params.set('token', token);
    params.set('session_id', sessionId);
    params.set('agent', this.agentAlias);
    params.set('surface', 'web');
    const url = `${this.baseUrl}${basePath}/ws/chat?${params.toString()}`;

    const protocols: string[] = ['zeroclaw.v1'];
    if (token) protocols.push(`bearer.${token}`);
    this.ws = new WebSocket(url, protocols);

    this.ws.onopen = () => {
      this.currentDelay = this.reconnectDelay;
      // S2 (#549): negotiation precedes every admission on this socket; the
      // gateway refuses legacy `message` frames once negotiated, and refuses
      // turn ops before it. The epoch arrives with `turn_ready` below.
      this.turnEpoch = null;
      this.sendRaw({ type: 'turn_control', version: 1, op: 'negotiate' });
      this.onOpen?.();
    };

    this.ws.onmessage = (ev: MessageEvent) => {
      try {
        const msg = JSON.parse(ev.data) as WsMessage;
        // S2 (#549): `turn_ready` completes negotiation and flushes anything
        // queued while the socket was pre-negotiation.
        if (msg.type === 'turn_ready' && typeof msg.epoch === 'string') {
          this.turnEpoch = msg.epoch;
          this.negotiatedOnce = true;
          const replayEvents = msg.limits?.replay_events;
          if (typeof replayEvents === 'number' && replayEvents > 0) {
            this.replayEventLimit = replayEvents;
          }
          while (this.turnEpoch === msg.epoch && this.queuedSends.length > 0) {
            const queued = this.queuedSends.shift()!;
            this.sendRaw({ ...queued.frame, epoch: msg.epoch });
          }
        }
        this.onMessage?.(msg);
      } catch {
        // Ignore non-JSON frames
      }
    };

    this.ws.onclose = (ev: CloseEvent) => {
      this.onClose?.(ev);
      this.scheduleReconnect();
    };

    this.ws.onerror = (ev: Event) => {
      this.onError?.(ev);
    };
  }

  /** Send a chat message as a negotiated S2 turn (#549). Returns the
   * `request_id` the caller must correlate `turn_ack`/`turn_event` frames
   * against. Frames sent before `turn_ready` are queued and flushed with the
   * negotiated epoch; they are never silently dropped. */
  sendTurnMessage(content: string, attachments?: string[]): string {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      throw new Error('WebSocket is not connected');
    }
    const requestId = generateUUID();
    const frame: Record<string, unknown> = {
      type: 'turn_control',
      version: 1,
      op: 'message',
      request_id: requestId,
      content,
    };
    if (attachments && attachments.length > 0) {
      frame.attachments = attachments;
    }
    if (this.turnEpoch === null) {
      this.queuedSends.push({ requestId, frame });
    } else {
      this.sendRaw({ ...frame, epoch: this.turnEpoch });
    }
    return requestId;
  }

  /** Cancel the observed turn (S2 scoped cancellation). The control MUST
   * carry the original message's `requestId`: the server journals and looks
   * the cancellation up under that admission's identity — a fresh id would
   * always resolve to `unknown`. Returns false when not negotiated. */
  cancelTurn(requestId: string, turnId: string): boolean {
    if (this.turnEpoch === null || !this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return false;
    }
    this.sendRaw({
      type: 'turn_control',
      version: 1,
      op: 'cancel',
      epoch: this.turnEpoch,
      request_id: requestId,
      turn_id: turnId,
    });
    return true;
  }

  /** Replay events after `afterSeq` for an admitted request (S2 resume). */
  resumeTurn(requestId: string, afterSeq: number): void {
    if (this.turnEpoch === null || !this.ws || this.ws.readyState !== WebSocket.OPEN) {
      return;
    }
    this.sendRaw({
      type: 'turn_control',
      version: 1,
      op: 'resume',
      epoch: this.turnEpoch,
      request_id: requestId,
      after_seq: afterSeq,
    });
  }

  /** The negotiated S2 epoch, or null before `turn_ready` / after reconnect. */
  get epoch(): string | null {
    return this.turnEpoch;
  }

  /** True once ANY socket of this client completed S2 negotiation. Stop and
   * model-switch use this (not `epoch`) to decide whether the legacy REST
   * abort fallback is allowed: during a reconnect window the epoch is null
   * but a session-level abort could still cancel a newer turn server-side. */
  get hasNegotiated(): boolean {
    return this.negotiatedOnce;
  }

  /** Server replay-cache bound (events), or null before negotiation. */
  get replayLimit(): number | null {
    return this.replayEventLimit;
  }

  /** Negotiation includes the same auth query params as the socket itself, so
   * admission is bound to the authenticated principal. */
  private sendRaw(frame: Record<string, unknown>): void {
    this.ws?.send(JSON.stringify(frame));
  }

  /**
   * Reply to a supervised-mode tool `approval_request`. The backend matches
   * the response by `request_id` and resolves the parked approval oneshot.
   * If the socket is closed the request will auto-deny on the server side
   * after the timeout, so we silently no-op rather than throwing.
   */
  sendApprovalResponse(requestId: string, decision: ApprovalDecision): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;
    this.ws.send(
      JSON.stringify({ type: 'approval_response', request_id: requestId, decision }),
    );
  }

  /** Close the connection without auto-reconnecting. */
  disconnect(): void {
    this.intentionallyClosed = true;
    this.clearReconnectTimer();
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
  }

  /** Returns true if the socket is open. */
  get connected(): boolean {
    return this.ws?.readyState === WebSocket.OPEN;
  }

  // ---------------------------------------------------------------------------
  // Reconnection logic
  // ---------------------------------------------------------------------------

  private scheduleReconnect(): void {
    if (this.intentionallyClosed || !this.autoReconnect) return;

    this.reconnectTimer = setTimeout(() => {
      this.currentDelay = Math.min(this.currentDelay * 2, this.maxReconnectDelay);
      this.connect();
    }, this.currentDelay);
  }

  private clearReconnectTimer(): void {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }
}
