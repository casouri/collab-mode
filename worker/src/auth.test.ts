// Tests for the auth module. Mirrors the Rust suite at
// collab_mode/src/signaling/auth.rs (lines 80-141): round_trip,
// tampered_cert, stale/future timestamp, wrong_key — plus extra
// parser/header cases that only exist on the TypeScript side.

// `@peculiar/x509` uses `tsyringe`, which needs a Reflect.metadata
// polyfill under Node. Cloudflare's `workerd` provides this natively;
// vitest does not.
import 'reflect-metadata';
import { describe, expect, test } from 'vitest';
import * as x509 from '@peculiar/x509';
import { Convert } from 'pvtsutils';

import {
  AuthError,
  decodeTrustedHeader,
  hashCert,
  parseIdentity,
  verifyIdentity,
} from './auth';

// *** Helpers ***

interface KeyCert {
  keys: CryptoKeyPair;
  certDer: Uint8Array;
}

async function generateKeyCert(name: string): Promise<KeyCert> {
  const keys = (await crypto.subtle.generateKey(
    { name: 'ECDSA', namedCurve: 'P-256' },
    true,
    ['sign', 'verify'],
  )) as CryptoKeyPair;
  const cert = await x509.X509CertificateGenerator.createSelfSigned({
    serialNumber: '01',
    name: `CN=${name}`,
    notBefore: new Date(Date.now() - 1000),
    notAfter: new Date(Date.now() + 60_000),
    signingAlgorithm: { name: 'ECDSA', hash: 'SHA-256' },
    keys,
  });
  return { keys, certDer: new Uint8Array(cert.rawData) };
}

// Build an `X-Collab-Identity` header `<timestamp>:<base64-cert>` plus
// the matching base64 signature. Mirrors `sign_identity` from auth.rs.
async function signIdentity(
  keyCert: KeyCert,
  timestampOverride?: number,
): Promise<{ header: string; signatureB64: string }> {
  const ts = timestampOverride ?? Math.floor(Date.now() / 1000);
  const header = `${ts}:${Convert.ToBase64(keyCert.certDer.buffer as ArrayBuffer)}`;
  const sig = await crypto.subtle.sign(
    { name: 'ECDSA', hash: 'SHA-256' },
    keyCert.keys.privateKey,
    new TextEncoder().encode(header),
  );
  return { header, signatureB64: Convert.ToBase64(sig) };
}

// *** parseIdentity + verifyIdentity ***

describe('parseIdentity + verifyIdentity', () => {
  test('round trip', async () => {
    const kc = await generateKeyCert('alice');
    const { header, signatureB64 } = await signIdentity(kc);
    const identity = parseIdentity(header);

    const hash = await verifyIdentity(identity, signatureB64);
    expect(hash).toBe(await hashCert(kc.certDer));
  });

  test('stale timestamp rejected', async () => {
    const kc = await generateKeyCert('alice');
    const stale = Math.floor(Date.now() / 1000) - 31;
    const { header, signatureB64 } = await signIdentity(kc, stale);
    const identity = parseIdentity(header);

    await expect(verifyIdentity(identity, signatureB64)).rejects.toThrow(AuthError);
  });

  test('future timestamp rejected', async () => {
    const kc = await generateKeyCert('alice');
    const future = Math.floor(Date.now() / 1000) + 31;
    const { header, signatureB64 } = await signIdentity(kc, future);
    const identity = parseIdentity(header);

    await expect(verifyIdentity(identity, signatureB64)).rejects.toThrow(AuthError);
  });

  test('tampered cert rejected', async () => {
    const kc = await generateKeyCert('alice');
    const { header, signatureB64 } = await signIdentity(kc);
    const identity = parseIdentity(header);

    // Flip the outer SEQUENCE tag (byte 0 of any X.509 DER is 0x30).
    // The cert no longer parses → AuthError. Tampering inside the
    // cert body wouldn't reliably trigger a failure here because
    // `signedBytes` is captured at parse time, so the only path that
    // re-reads certDer is X509Certificate construction.
    identity.certDer[0] ^= 0x01;

    await expect(verifyIdentity(identity, signatureB64)).rejects.toThrow(AuthError);
  });

  test('tampered signed bytes rejected', async () => {
    const kc = await generateKeyCert('alice');
    const { header, signatureB64 } = await signIdentity(kc);
    const identity = parseIdentity(header);

    // Flip a byte in the signed-over bytes (anywhere past the
    // timestamp). Signature no longer matches.
    identity.signedBytes[identity.signedBytes.length - 1] ^= 0x01;

    await expect(verifyIdentity(identity, signatureB64)).rejects.toThrow(AuthError);
  });

  test('wrong key rejected', async () => {
    const alice = await generateKeyCert('alice');
    const bob = await generateKeyCert('bob');

    // Sign a header with Alice's key, then swap Alice's cert for
    // Bob's in the header — signature was made over the original,
    // but verify will pull Bob's pubkey from the cert.
    const ts = Math.floor(Date.now() / 1000);
    const aliceHeader = `${ts}:${Convert.ToBase64(alice.certDer.buffer as ArrayBuffer)}`;
    const sig = await crypto.subtle.sign(
      { name: 'ECDSA', hash: 'SHA-256' },
      alice.keys.privateKey,
      new TextEncoder().encode(aliceHeader),
    );
    const signatureB64 = Convert.ToBase64(sig);

    const bobHeader = `${ts}:${Convert.ToBase64(bob.certDer.buffer as ArrayBuffer)}`;
    const tampered = parseIdentity(bobHeader);

    await expect(verifyIdentity(tampered, signatureB64)).rejects.toThrow(AuthError);
  });
});

// *** parseIdentity parser edges ***

describe('parseIdentity parser', () => {
  test('rejects missing colon', () => {
    expect(() => parseIdentity('no-colon-here')).toThrow(AuthError);
  });

  test('rejects bad timestamp', async () => {
    const kc = await generateKeyCert('alice');
    const certB64 = Convert.ToBase64(kc.certDer.buffer as ArrayBuffer);
    expect(() => parseIdentity(`abc:${certB64}`)).toThrow(AuthError);
    expect(() => parseIdentity(`-1:${certB64}`)).toThrow(AuthError);
    expect(() => parseIdentity(`1.5:${certB64}`)).toThrow(AuthError);
  });

  test('rejects bad base64', () => {
    expect(() => parseIdentity('123:not_base64!!!')).toThrow(AuthError);
  });
});

// *** hashCert ***

describe('hashCert', () => {
  test('deterministic', async () => {
    const kc = await generateKeyCert('alice');
    const a = await hashCert(kc.certDer);
    const b = await hashCert(kc.certDer);
    expect(a).toBe(b);
  });

  test('format is uppercase hex joined by colons', async () => {
    const kc = await generateKeyCert('alice');
    const hash = await hashCert(kc.certDer);
    // SHA-256 → 32 bytes → 32 groups of two hex chars joined by 31 colons → 95 chars total.
    expect(hash).toMatch(/^[0-9A-F]{2}(:[0-9A-F]{2}){31}$/);
  });
});

// *** decodeTrustedHeader ***

describe('decodeTrustedHeader', () => {
  test('empty string returns empty array', () => {
    expect(decodeTrustedHeader('')).toEqual([]);
  });

  test('splits on commas', () => {
    expect(decodeTrustedHeader('a,b,c')).toEqual(['a', 'b', 'c']);
  });
});
