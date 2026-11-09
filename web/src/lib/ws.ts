import type { ApprovalDecision, WsMessage } from '../types/api';
import { getToken } from './auth';
import { apiOrigin, basePath } from './basePath';
import { isTauri } from './tauri';
import { generateUUID } from './uuid';

export type WsMessageHandler = (msg: WsMessage) => void;
export type WsOpenHandler = () => void;
export type WsCloseHandler = (ev: CloseEvent) => void;
export type WsErrorHandler = (ev: Event) => void;
export type WsNegotiationErrorHandler = (code: string, unsentRequestIds: string[]) => void;

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
  /** Preserve the S2 requirement when rebuilding an authenticated client. */
  requireTurnControl?: boolean;
}

const DEFAULT_RECONNECT_DELAY = 1000;
const MAX_RECONNECT_DELAY = 30000;

// Keep even late side-query errors out of the ordinary turn lifecycle without
// retaining an unbounded list of completed request ids.
const SIDE_QUERY_ID_PREFIX = 'btw-';
export function isSideQueryRequestId(id: string | undefined): id is string {
  return id?.startsWith(SIDE_QUERY_ID_PREFIX) === true;
}

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
  public onNegotiationError: WsNegotiationErrorHandler | null = null;

  /** S2 (#549): negotiated epoch, or null before `turn_ready` / during a
   * reconnect window (onopen clears it until the new socket negotiates). */
  private turnEpoch: string | null = null;
  private steeringNegotiated = false;
  /** Once selected, S2 stays required across failed negotiation, credential
   * loss and reconnect. Legacy is only chosen for an anonymous attachment. */
  private turnControlRequired = false;
  private protocol: 'negotiating' | 's2' | 'legacy' | 'failed' = 'negotiating';
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
    this.turnControlRequired = options.requireTurnControl ?? false;
  }

  /** Open the WebSocket connection. */
  connect(): void {
    this.intentionallyClosed = false;
    this.clearReconnectTimer();

    const token = getToken();
    this.turnControlRequired ||= Boolean(token);
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
      // Anonymous connections are supported when pairing is disabled, but S2
      // requires an authenticated subject. Use the existing legacy contract
      // only before this attachment has ever selected S2. Losing a token
      // during reconnect must never downgrade the contract.
      this.turnEpoch = null;
      this.steeringNegotiated = false;
      this.protocol = this.turnControlRequired ? 'negotiating' : 'legacy';
      if (this.protocol === 'negotiating') {
        this.sendRaw({ type: 'turn_control', version: 1, op: 'negotiate', capabilities: ['steer'] });
      }
      this.onOpen?.();
    };

    this.ws.onmessage = (ev: MessageEvent) => {
      try {
        const msg = JSON.parse(ev.data) as WsMessage;
        const negotiationRefused = msg.type === 'turn_error' && (msg.op === 'negotiate' || msg.op === undefined)
          || msg.type === 'error' && ['BRIDGE_OPERATION_DENIED', 'AUTH_ERROR', 'UNKNOWN_MESSAGE_TYPE', 'AGENT_INIT_FAILED'].includes(msg.code ?? '');
        if (this.protocol === 'negotiating' && negotiationRefused) {
          this.protocol = 'failed';
          const unsentRequestIds = this.queuedSends.splice(0).map((queued) => queued.requestId);
          this.onNegotiationError?.(msg.code ?? 'UNKNOWN', unsentRequestIds);
          return;
        }
        // S2 (#549): `turn_ready` completes negotiation and flushes anything
        // queued while the socket was pre-negotiation.
        if (this.protocol === 'negotiating' && msg.type === 'turn_ready' && typeof msg.epoch === 'string') {
          this.turnEpoch = msg.epoch;
          this.steeringNegotiated = msg.capabilities?.includes('steer') === true;
          this.protocol = 's2';
          // Reconciliation needs to distinguish an unsent queued request from
          // an admission whose outcome belongs to the previous process epoch.
          this.onMessage?.(msg);
          while (this.turnEpoch === msg.epoch && this.queuedSends.length > 0) {
            const queued = this.queuedSends.shift()!;
            this.sendRaw({ ...queued.frame, epoch: msg.epoch });
          }
          return;
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

  /** Send through this attachment's established contract. Returns the
   * `request_id` the caller must correlate `turn_ack`/`turn_event` frames
   * against. Frames sent before `turn_ready` are queued and flushed with the
   * negotiated epoch, unless cancelled locally or negotiation is refused. */
  sendTurnMessage(content: string, attachments?: string[]): string {
    if (!this.connected) {
      throw new Error('WebSocket is not connected');
    }
    const requestId = generateUUID();
    const frame: Record<string, unknown> = {
      request_id: requestId,
      content,
    };
    if (attachments && attachments.length > 0) {
      frame.attachments = attachments;
    }
    if (this.protocol === 'legacy') {
      this.sendRaw({ type: 'message', ...frame });
    } else {
      const turnFrame = { type: 'turn_control', version: 1, op: 'message', ...frame };
      if (this.turnEpoch === null) {
        this.queuedSends.push({ requestId, frame: turnFrame });
      } else {
        this.sendRaw({ ...turnFrame, epoch: this.turnEpoch });
      }
    }
    return requestId;
  }

  /** A read-only side query has no turn admission, replay or reconnect queue.
   * The gateway explicitly accepts `btw` on negotiated S2 attachments too.
   * Its dedicated `id` is echoed on admission errors as well as results;
   * S2 `op: message` requires a UUID and does not correlate its refusals. */
  sendSideQuery(question: string): string | null {
    if (!this.connected || (this.protocol !== 'legacy' && (this.protocol !== 's2' || !this.turnEpoch))) return null;
    const requestId = `${SIDE_QUERY_ID_PREFIX}${generateUUID()}`;
    this.sendRaw({ type: 'btw', id: requestId, content: `/btw ${question}` });
    return requestId;
  }

  /** Withdraw only a message that has never reached the wire. */
  cancelQueuedMessage(requestId: string): boolean {
    const index = this.queuedSends.findIndex((queued) => queued.requestId === requestId);
    if (index < 0) return false;
    this.queuedSends.splice(index, 1);
    return true;
  }

  isMessageQueued(requestId: string): boolean {
    return this.queuedSends.some((queued) => queued.requestId === requestId);
  }

  /** Explicit correction for an observed turn. Never queued across handshake,
   * reconnect or epoch change; a successful send is not an applied receipt. */
  steerTurn(parentRequestId: string, turnId: string, epoch: string, content: string, attachments?: string[]): string | null {
    if (!this.canSteer || this.turnEpoch !== epoch) return null;
    const requestId = generateUUID();
    this.sendRaw({
      type: 'turn_control', version: 1, op: 'steer', epoch,
      request_id: requestId, parent_request_id: parentRequestId, turn_id: turnId,
      content, ...(attachments?.length ? { attachments } : {}),
    });
    return requestId;
  }

  get canSteer(): boolean {
    return this.protocol === 's2' && this.steeringNegotiated && this.turnEpoch !== null && this.connected;
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

  /** Replacements must preserve S2 even when this socket never reached ready. */
  get requiresTurnControl(): boolean {
    return this.turnControlRequired;
  }

  get isLegacy(): boolean {
    return this.protocol === 'legacy';
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
    this.queuedSends.length = 0;
    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
  }

  /** Returns true if the open transport can accept new messages. */
  get connected(): boolean {
    return this.ws?.readyState === WebSocket.OPEN && this.protocol !== 'failed';
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
