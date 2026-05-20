// Thin wrappers around the D1 schema.

import type { EndpointId } from './messages';

export async function isBlacklisted(db: D1Database, certHash: string): Promise<boolean> {
  const row = await db
    .prepare('SELECT 1 FROM blacklist WHERE cert_hash = ?')
    .bind(certHash)
    .first();
  return row !== null;
}

export async function addToBlacklist(
  db: D1Database,
  certHash: string,
  reason: string,
): Promise<void> {
  await db
    .prepare('INSERT OR IGNORE INTO blacklist(cert_hash, reason, added_at) VALUES (?, ?, ?)')
    .bind(certHash, reason, Math.floor(Date.now() / 1000))
    .run();
}

// Upsert the binding and replace the trust list.
export async function upsertBindingAndTrust(
  db: D1Database,
  endpointId: EndpointId,
  certHash: string,
  trusted: EndpointId[],
): Promise<void> {
  const now = Math.floor(Date.now() / 1000);
  const stmts: D1PreparedStatement[] = [
    db
      .prepare('INSERT OR REPLACE INTO bindings(endpoint_id, cert_hash, bound_at) VALUES (?, ?, ?)')
      .bind(endpointId, certHash, now),
    db.prepare('DELETE FROM trusted WHERE endpoint_id = ?').bind(endpointId),
  ];
  for (const peer of trusted) {
    stmts.push(
      db
        .prepare('INSERT INTO trusted(endpoint_id, trusted_id) VALUES (?, ?)')
        .bind(endpointId, peer),
    );
  }
  await db.batch(stmts);
}

// Replace the trust list for a bound endpoint.
export async function replaceTrust(
  db: D1Database,
  endpointId: EndpointId,
  trusted: EndpointId[],
): Promise<void> {
  const stmts: D1PreparedStatement[] = [
    db.prepare('DELETE FROM trusted WHERE endpoint_id = ?').bind(endpointId),
  ];
  for (const peer of trusted) {
    stmts.push(
      db
        .prepare('INSERT INTO trusted(endpoint_id, trusted_id) VALUES (?, ?)')
        .bind(endpointId, peer),
    );
  }
  await db.batch(stmts);
}

export async function isBound(db: D1Database, endpointId: EndpointId): Promise<boolean> {
  const row = await db
    .prepare('SELECT 1 FROM bindings WHERE endpoint_id = ?')
    .bind(endpointId)
    .first();
  return row !== null;
}

export async function isTrustedBy(
  db: D1Database,
  endpointId: EndpointId,
  candidate: EndpointId,
): Promise<boolean> {
  const row = await db
    .prepare('SELECT 1 FROM trusted WHERE endpoint_id = ? AND trusted_id = ?')
    .bind(endpointId, candidate)
    .first();
  return row !== null;
}

// Cleanup on disconnect: drop the binding and the trust rows.
// Blacklist rows stay.
export async function deleteEndpoint(db: D1Database, endpointId: EndpointId): Promise<void> {
  await db.batch([
    db.prepare('DELETE FROM bindings WHERE endpoint_id = ?').bind(endpointId),
    db.prepare('DELETE FROM trusted WHERE endpoint_id = ?').bind(endpointId),
  ]);
}
