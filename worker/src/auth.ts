// Auth helpers. Mirrors the checks in
// collab_mode/src/signaling/auth.rs.

import * as x509 from '@peculiar/x509';
import { Convert } from 'pvtsutils';

const MAX_TIMESTAMP_OFFSET_SECS = 30;

export interface Identity {
  // Unix epoch in seconds.
  timestamp: number;
  // Original header string as the client signed it.
  signedBytes: Uint8Array;
  // pub key in DER format.
  certDer: Uint8Array;
}

export class AuthError extends Error {}

// Parse `<timestamp>:<base64-cert-DER>`.
export function parseIdentity(header: string): Identity {
  const colon = header.indexOf(':');
  if (colon === -1) {
    throw new AuthError("Identity missing ':' separator");
  }
  const tsStr = header.slice(0, colon);
  const certB64 = header.slice(colon + 1);

  const timestamp = Number(tsStr);
  if (!Number.isFinite(timestamp) || !Number.isInteger(timestamp) || timestamp < 0) {
    throw new AuthError(`Bad identity timestamp: ${tsStr}`);
  }

  let certDer: Uint8Array;
  try {
    certDer = new Uint8Array(Convert.FromBase64(certB64));
  } catch (err) {
    throw new AuthError(`cert base64: ${(err as Error).message}`);
  }

  return {
    timestamp,
    signedBytes: new TextEncoder().encode(header),
    certDer,
  };
}

// SHA-256 of the DER bytes, formatted as uppercase hex joined by `:`.
// Matches `config_man::hash_der`.
export async function hashCert(certDer: Uint8Array): Promise<string> {
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', certDer));
  const parts: string[] = new Array(digest.length);
  for (let i = 0; i < digest.length; i++) {
    parts[i] = digest[i].toString(16).padStart(2, '0').toUpperCase();
  }
  return parts.join(':');
}

// Verify (timestamp, signature). Throws on any failure. Returns the cert hash
// on success so callers don't recompute it.
export async function verifyIdentity(identity: Identity, signatureB64: string): Promise<string> {
  const now = Math.floor(Date.now() / 1000);
  const drift = Math.abs(now - identity.timestamp);
  if (drift > MAX_TIMESTAMP_OFFSET_SECS) {
    throw new AuthError(
      `Identity timestamp ${identity.timestamp} outside max allowed ${MAX_TIMESTAMP_OFFSET_SECS}s offset (now=${now})`,
    );
  }

  let sigBytes: Uint8Array;
  try {
    sigBytes = new Uint8Array(Convert.FromBase64(signatureB64));
  } catch (err) {
    throw new AuthError(`Signature base64: ${(err as Error).message}`);
  }

  let cert: x509.X509Certificate;
  try {
    cert = new x509.X509Certificate(identity.certDer);
  } catch (err) {
    throw new AuthError(`Parsing identity cert: ${(err as Error).message}`);
  }

  const key = await crypto.subtle.importKey(
    'spki',
    cert.publicKey.rawData,
    { name: 'ECDSA', namedCurve: 'P-256' },
    false,
    ['verify'],
  );

  const ok = await crypto.subtle.verify(
    { name: 'ECDSA', hash: 'SHA-256' },
    key,
    sigBytes,
    identity.signedBytes,
  );
  if (!ok) {
    throw new AuthError('Identity signature verification failed');
  }

  return await hashCert(identity.certDer);
}

// Comma-separated list, empty string decodes to []. Mirrors
// `decode_trusted_header` in collab_mode/src/signaling.rs.
export function decodeTrustedHeader(value: string): string[] {
  if (value.length === 0) return [];
  return value.split(',');
}
