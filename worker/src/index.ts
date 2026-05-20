// Worker entrypoint and SignalingEndpoint Durable Object. API mirrors
// collab_mode/src/signaling/server.rs.

import { DurableObject } from 'cloudflare:workers';
import {
  AuthError,
  hashCert,
  decodeTrustedHeader,
  parseIdentity,
  verifyIdentity,
  Identity,
} from './auth';
import {
  addToBlacklist,
  deleteEndpoint,
  isBlacklisted,
  isBound,
  isTrustedBy,
  replaceTrust,
  upsertBindingAndTrust,
} from './d1';
import { encodeMsg, idHash, parseMsg, SignalingMsg } from './messages';

// Header names must match the BIND_HEADER_* constants in
// collab_mode/src/signaling.rs.
const BIND_HEADER_ID = 'X-Collab-Endpoint-Id';
const BIND_HEADER_IDENTITY = 'X-Collab-Identity';
const BIND_HEADER_SIGNATURE = 'X-Collab-Signature';
const BIND_HEADER_TRUSTED = 'X-Collab-Trusted';

// Internal headers used when the Worker forwards an upgrade to the DO.
const INTERNAL_HEADER_ENDPOINT = 'X-Internal-Endpoint-Id';
const INTERNAL_HEADER_CERT_HASH = 'X-Internal-Cert-Hash';
const INTERNAL_HEADER_AUTH = 'X-Internal-Auth';

// Sender DO closes the WS and blacklists the cert after this many failed
// Connect/SDP/Candidate attempts in one session.
const REJECT_THRESHOLD = 100;

const SWEEP_INTERVAL_MS = 10 * 60_000;
const INACTIVE_TIMEOUT_MS = 10 * 60_000;
const MAX_LIFETIME_MS = 12 * 60 * 60_000;

const PING_FRAME = '{"kind":"Ping"}';
const PONG_FRAME = '{"kind":"Pong"}';

interface Attachment {
  endpointId: string;
  certHash: string;
  boundAt: number;
  rejectCount: number;
  // Set to true on the old socket when a fresh bind for the same endpoint
  // takes over, so the close handler doesn't wipe the new session's D1 rows.
  replaced?: boolean;
}

// *** DO

export class SignalingEndpoint extends DurableObject<Env> {
  async fetch(request: Request): Promise<Response> {
    if ((request.headers.get('Upgrade') ?? '').toLowerCase() === 'websocket') {
      return this.handleUpgrade(request);
    }
    const url = new URL(request.url);
    if (url.pathname === '/__route') {
      return this.handelRouteMessage(request);
    }
    return new Response('not found', { status: 404 });
  }

  // Handle initial bind. The worker has already validated identity
  // and written bindings and trusted endpoints into D1. Now just
  // manage the websocket connection.
  async handleUpgrade(request: Request): Promise<Response> {
    const endpointId = request.headers.get(INTERNAL_HEADER_ENDPOINT);
    const cHash = request.headers.get(INTERNAL_HEADER_CERT_HASH);
    if (!endpointId || !cHash) {
      return new Response('missing internal headers', { status: 500 });
    }

    // Concurrent rebind: tag the old socket as replaced so its close handler
    // doesn't delete the freshly upserted D1 rows, then close it.
    for (const old of this.ctx.getWebSockets()) {
      const att = (old.deserializeAttachment() as Attachment | null) ?? null;
      if (att) {
        att.replaced = true;
        old.serializeAttachment(att);
      }
      try {
        old.close(1000, 'rebinding');
      } catch (err) {
        console.log('Failed to close old websocket connection', err);
      }
    }

    const pair = new WebSocketPair();
    const client = pair[0];
    const server = pair[1];

    this.ctx.acceptWebSocket(server);

    this.ctx.setWebSocketAutoResponse(new WebSocketRequestResponsePair(PING_FRAME, PONG_FRAME));

    const attachment: Attachment = {
      endpointId,
      certHash: cHash,
      boundAt: Date.now(),
      rejectCount: 0,
    };
    server.serializeAttachment(attachment);

    // Timer for timout and 12h force termination.
    if ((await this.ctx.storage.getAlarm()) === null) {
      await this.ctx.storage.setAlarm(Date.now() + SWEEP_INTERVAL_MS);
    }

    return new Response(null, { status: 101, webSocket: client });
  }

  // Sibling DO calling to deliver a Connect/SDP/Candidate. Returns
  // 204 on a successful send; returns 200 with a JSON SignalingMsg
  // body when need to send an error response.
  async handelRouteMessage(request: Request): Promise<Response> {
    const raw = await request.text();
    const msg = parseMsg(raw);
    if (msg === null) {
      return new Response("Can't parse the message", { status: 400 });
    }
    if (msg.kind !== 'Connect' && msg.kind !== 'SDP' && msg.kind !== 'Candidate') {
      return new Response('Expecting Connect, SDP, or Candidate kind', { status: 400 });
    }
    const { sender, receiver } = msg;

    const sockets = this.ctx.getWebSockets();
    if (sockets.length === 0) {
      const reply: SignalingMsg = {
        kind: 'IdNotFound',
        id: sender,
        reason: `No endpoint found for id: ${receiver}`,
      };
      return Response.json(reply);
    }

    const ws = sockets[0];
    const att = ws.deserializeAttachment() as Attachment;

    // Check trust as receiver.
    if (!(await isTrustedBy(this.env.DB, att.endpointId, sender))) {
      const reply: SignalingMsg = {
        kind: 'Rejected',
        id: sender,
        reason: `${att.endpointId} does not trust sender`,
      };
      return Response.json(reply);
    }

    ws.send(raw);
    return new Response(null, { status: 204 });
  }

  // Handle incoming message from client.
  async webSocketMessage(ws: WebSocket, raw: ArrayBuffer | string): Promise<void> {
    if (typeof raw !== 'string') {
      ws.close(1003, 'We only support text message');
      return;
    }

    const msg = parseMsg(raw);
    if (msg === null) {
      ws.close(1003, 'Unsupported');
      return;
    }

    // Auto-response handles Ping. Server don’t handle Pong.
    if (msg.kind === 'Ping' || msg.kind === 'Pong') {
      return;
    }

    const att = ws.deserializeAttachment() as Attachment;

    if (msg.kind === 'Trust') {
      const { sender, trusted } = msg;
      if (sender !== att.endpointId) {
        ws.close(1003, 'You are using a endpoint id different from the one recorded');
        return;
      }
      await replaceTrust(this.env.DB, sender, trusted);
      ws.send(encodeMsg({ kind: 'Trusted', id: sender, trusted }));
      return;
    }

    if (msg.kind === 'Connect' || msg.kind === 'SDP' || msg.kind === 'Candidate') {
      const { sender, receiver } = msg;
      if (sender !== att.endpointId) {
        ws.close(1003, 'You are using a endpoint id different from the one recorded');
        return;
      }

      // Receiver bound?
      if (!(await isBound(this.env.DB, receiver))) {
        const reply: SignalingMsg = {
          kind: 'IdNotFound',
          id: sender,
          reason: `No endpoint found for id: ${receiver}`,
        };
        ws.send(encodeMsg(reply));
        await this.bumpReject(ws, att);
        return;
      }

      // Sender trusted by receiver?
      if (!(await isTrustedBy(this.env.DB, receiver, sender))) {
        const reply: SignalingMsg = {
          kind: 'Rejected',
          id: sender,
          reason: `${receiver} does not trust sender`,
        };
        ws.send(encodeMsg(reply));
        await this.bumpReject(ws, att);
        return;
      }

      // Forward to the receiver’s DO.
      const stub = this.env.SIGNALING_DO.get(this.env.SIGNALING_DO.idFromName(receiver));
      const resp = await stub.fetch('https://do/__route', {
        method: 'POST',
        headers: {
          'Content-Type': 'application/json',
        },
        body: raw,
      });

      // Receiver sends 204 if no there’s error response to send back.
      if (resp.status === 204) return;

      let respMsg: SignalingMsg;
      if (resp.status === 200) {
        respMsg = (await resp.json()) as SignalingMsg;
      } else {
        respMsg = {
          kind: 'IdNotFound',
          id: sender,
          reason: `delivery error: ${resp.status}`,
        };
        await this.bumpReject(ws, att);
      }
      ws.send(encodeMsg(respMsg));
      return;
    }

    // Bound / Trusted / IdNotFound / Rejected / Inactive aren’t valid request.
    ws.close(1003, 'Send Connect, SDP, Candidate, Trust, or Ping only');
  }

  async webSocketClose(
    ws: WebSocket,
    _code: number,
    _reason: string,
    _wasClean: boolean,
  ): Promise<void> {
    await this.cleanup(ws);
  }

  async webSocketError(ws: WebSocket, _error: unknown): Promise<void> {
    await this.cleanup(ws);
  }

  // Handle timer.
  async alarm(): Promise<void> {
    const now = Date.now();
    for (const ws of this.ctx.getWebSockets()) {
      const att = ws.deserializeAttachment() as Attachment | null;
      if (!att) continue;

      const lastPingTs = this.ctx.getWebSocketAutoResponseTimestamp(ws);
      const lastPing = lastPingTs ? lastPingTs.getTime() : att.boundAt;
      if (now - lastPing > INACTIVE_TIMEOUT_MS) {
        trySend(ws, {
          kind: 'Inactive',
          reason: 'No ping received in 10 minutes',
        });
        tryClose(ws, 1000, 'inactive');
        continue;
      }

      if (now - att.boundAt > MAX_LIFETIME_MS) {
        trySend(ws, {
          kind: 'Inactive',
          reason: '12-hour connection lifetime reached, please re-bind',
        });
        tryClose(ws, 1000, 'lifetime');
      }
    }

    // Schedule next timer.
    if (this.ctx.getWebSockets().length > 0) {
      await this.ctx.storage.setAlarm(Date.now() + SWEEP_INTERVAL_MS);
    }
  }

  // Remove this endpoint from D1.
  private async cleanup(ws: WebSocket): Promise<void> {
    const att = ws.deserializeAttachment() as Attachment | null;
    if (att && !att.replaced) {
      await deleteEndpoint(this.env.DB, att.endpointId);
    }
    if (this.ctx.getWebSockets().length === 0) {
      await this.ctx.storage.deleteAll();
    }
  }

  // Bump reject count.
  private async bumpReject(ws: WebSocket, att: Attachment): Promise<void> {
    att.rejectCount += 1;
    ws.serializeAttachment(att);
    if (att.rejectCount > REJECT_THRESHOLD) {
      await addToBlacklist(this.env.DB, att.certHash, 'rejection abuse');
      tryClose(ws, 1008, 'abuse: too many rejected attempts');
    }
  }
}

function trySend(ws: WebSocket, msg: SignalingMsg): void {
  try {
    ws.send(encodeMsg(msg));
  } catch {
    // Already closed.
  }
}

function tryClose(ws: WebSocket, code: number, reason: string): void {
  try {
    ws.close(code, reason);
  } catch {
    // Already closed.
  }
}

// *** Worker

export default {
  async fetch(request: Request, env: Env, _ctx: ExecutionContext): Promise<Response> {
    if (request.method !== 'GET') {
      return new Response('Method not allowed', { status: 405 });
    }
    if ((request.headers.get('Upgrade') ?? '').toLowerCase() !== 'websocket') {
      return new Response('Expected WebSocket upgrade', { status: 400 });
    }

    const id = request.headers.get(BIND_HEADER_ID);
    const identityHeader = request.headers.get(BIND_HEADER_IDENTITY);
    const sig = request.headers.get(BIND_HEADER_SIGNATURE);
    const trustedHeader = request.headers.get(BIND_HEADER_TRUSTED);
    if (id === null || identityHeader === null || sig === null || trustedHeader === null) {
      return new Response('missing bind header', { status: 401 });
    }

    let identity: Identity;
    try {
      identity = parseIdentity(identityHeader);
    } catch (err) {
      const msg = err instanceof AuthError ? err.message : (err as Error).message;
      return new Response(`${BIND_HEADER_IDENTITY}: ${msg}`, { status: 401 });
    }

    const certHash = await hashCert(identity.certDer);

    const limited = await env.DEFAULT_RATE_LIMITER.limit({ key: certHash });
    if (!limited.success) {
      return new Response('Too many requests', { status: 429 });
    }

    if (await isBlacklisted(env.DB, certHash)) {
      return new Response('blacklisted', { status: 403 });
    }

    try {
      await verifyIdentity(identity, sig);
    } catch (err) {
      const msg = err instanceof AuthError ? err.message : (err as Error).message;
      return new Response(`Failed authentication: ${msg}`, { status: 401 });
    }

    if (idHash(id) !== certHash) {
      return new Response(`Id hash ${idHash(id)} doesn’t match cert hash ${certHash}`, {
        status: 401,
      });
    }

    let trusted: string[];
    try {
      trusted = decodeTrustedHeader(trustedHeader);
    } catch (err) {
      return new Response(`${BIND_HEADER_TRUSTED}: ${(err as Error).message}`, { status: 401 });
    }

    await upsertBindingAndTrust(env.DB, id, certHash, trusted);

    const stub = env.SIGNALING_DO.get(env.SIGNALING_DO.idFromName(id));
    const headers = new Headers(request.headers);
    headers.set(INTERNAL_HEADER_ENDPOINT, id);
    headers.set(INTERNAL_HEADER_CERT_HASH, certHash);
    const forwarded = new Request(request.url, {
      method: 'GET',
      headers,
    });
    return stub.fetch(forwarded);
  },
} satisfies ExportedHandler<Env>;
