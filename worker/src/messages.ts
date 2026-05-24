// Mirrors of the Rust SignalingMsg variants in
// collab_mode/src/signaling.rs.

export type EndpointId = string;

export type SignalingMsg =
  | { kind: 'Ping' }
  | { kind: 'Pong' }
  | { kind: 'Bound'; id: EndpointId; trusted: EndpointId[] }
  | { kind: 'Connect'; sender: EndpointId; receiver: EndpointId; initiator: boolean }
  | { kind: 'SDP'; sender: EndpointId; receiver: EndpointId; sdp: string }
  | { kind: 'Candidate'; sender: EndpointId; receiver: EndpointId; candidate: string }
  | { kind: 'Trust'; sender: EndpointId; trusted: EndpointId[] }
  | { kind: 'Trusted'; id: EndpointId; trusted: EndpointId[] }
  | { kind: 'IdNotFound'; id: EndpointId; reason: string }
  | { kind: 'Rejected'; id: EndpointId; reason: string }
  | { kind: 'Terminate'; reason: string };

export function parseMsg(raw: string): SignalingMsg | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (
    parsed === null ||
    typeof parsed !== 'object' ||
    typeof (parsed as { kind?: unknown }).kind !== 'string'
  ) {
    return null;
  }
  return parsed as SignalingMsg;
}

export function encodeMsg(msg: SignalingMsg): string {
  return JSON.stringify(msg);
}

// Extracts the cert-hash portion from an endpoint id of the form
// `<name>::<cert_hash>`. If there's no `::`, the whole id is treated as the
// hash. Mirrors `id_hash` in collab_mode/src/types.rs.
export function idHash(id: string): string {
  const idx = id.indexOf('::');
  return idx === -1 ? id : id.slice(idx + 2);
}
