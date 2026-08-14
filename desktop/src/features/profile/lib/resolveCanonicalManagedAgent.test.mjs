import assert from "node:assert/strict";
import test from "node:test";

import { resolveCanonicalManagedAgent } from "./useCanonicalManagedAgentProfile.ts";

const LIVE_PK = "b".repeat(64);
const ARCHIVED_PK = "a".repeat(64);
const HISTORICAL_PK = "c".repeat(64);
const NONE_ARCHIVED = () => false;

function agent(overrides = {}) {
  return {
    name: "Instance",
    pubkey: LIVE_PK,
    personaId: "persona-1",
    status: "stopped",
    ...overrides,
  };
}

test("a persona target with a live sibling resolves to the live instance", () => {
  const archived = agent({ pubkey: ARCHIVED_PK, status: "running" });
  const live = agent({ pubkey: LIVE_PK, status: "stopped" });

  const resolved = resolveCanonicalManagedAgent({
    directManagedAgent: undefined,
    isArchived: (pubkey) => pubkey === ARCHIVED_PK,
    personaInstances: [archived, live],
    preserveRequestedInstance: false,
    pubkey: undefined,
  });

  assert.equal(resolved, live);
});

test("a persona target with all instances archived resolves to undefined", () => {
  const first = agent({ pubkey: ARCHIVED_PK });
  const second = agent({ pubkey: HISTORICAL_PK });

  const resolved = resolveCanonicalManagedAgent({
    directManagedAgent: undefined,
    isArchived: () => true,
    personaInstances: [first, second],
    preserveRequestedInstance: false,
    pubkey: undefined,
  });

  assert.equal(resolved, undefined);
});

test("an explicit archived pubkey stays exact even when a live sibling exists", () => {
  const archivedDirect = agent({ pubkey: ARCHIVED_PK });
  const live = agent({ pubkey: LIVE_PK, status: "running" });

  const resolved = resolveCanonicalManagedAgent({
    directManagedAgent: archivedDirect,
    isArchived: (pubkey) => pubkey === ARCHIVED_PK,
    personaInstances: [archivedDirect, live],
    preserveRequestedInstance: false,
    pubkey: ARCHIVED_PK,
  });

  // Without the exactness short-circuit the selector would drop the archived
  // record and return `live`, stranding the unarchive controller.
  assert.equal(resolved, archivedDirect);
});

test("an explicit archived pubkey with no managed record resolves to undefined so the panel keeps the requested key", () => {
  // A historical archived pubkey with no current managed record: directManaged
  // is undefined, and the panel falls back to the requested pubkey verbatim.
  const resolved = resolveCanonicalManagedAgent({
    directManagedAgent: undefined,
    isArchived: (pubkey) => pubkey === HISTORICAL_PK,
    personaInstances: [],
    preserveRequestedInstance: false,
    pubkey: HISTORICAL_PK,
  });

  assert.equal(resolved, undefined);
});

test("a preserved requested instance pins the exact record over canonicalization", () => {
  const requested = agent({ pubkey: HISTORICAL_PK, status: "stopped" });
  const live = agent({ pubkey: LIVE_PK, status: "running" });

  const resolved = resolveCanonicalManagedAgent({
    directManagedAgent: requested,
    isArchived: NONE_ARCHIVED,
    personaInstances: [requested, live],
    preserveRequestedInstance: true,
    pubkey: HISTORICAL_PK,
  });

  assert.equal(resolved, requested);
});

test("a non-archived historical pubkey canonicalizes to the live persona instance", () => {
  // Rule 5: #5788 canonicalization is retained for non-archived navigation.
  const requested = agent({ pubkey: HISTORICAL_PK, status: "stopped" });
  const live = agent({ pubkey: LIVE_PK, status: "running" });

  const resolved = resolveCanonicalManagedAgent({
    directManagedAgent: requested,
    isArchived: NONE_ARCHIVED,
    personaInstances: [requested, live],
    preserveRequestedInstance: false,
    pubkey: HISTORICAL_PK,
  });

  assert.equal(resolved, live);
});
