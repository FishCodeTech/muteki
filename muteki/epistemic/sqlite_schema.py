"""SQLite DDL moved from sqlite_store.py."""
from __future__ import annotations

_SCHEMA = """
CREATE TABLE IF NOT EXISTS run_meta (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  run_id TEXT NOT NULL UNIQUE,
  protocol_version INTEGER NOT NULL CHECK(protocol_version=2),
  manifest_digest TEXT NOT NULL,
  durability_tier TEXT NOT NULL CHECK(durability_tier IN ('D0_PROCESS','D1_HOST'))
) STRICT;
CREATE TABLE IF NOT EXISTS commands (
  command_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL,
  idempotency_key TEXT NOT NULL UNIQUE,
  payload_digest TEXT NOT NULL,
  event_count INTEGER NOT NULL CHECK(event_count>0),
  first_seq INTEGER NOT NULL,
  last_seq INTEGER NOT NULL,
  event_set_digest TEXT NOT NULL,
  outbox_set_digest TEXT NOT NULL,
  receipt_json TEXT NOT NULL,
  receipt_digest TEXT NOT NULL,
  committed_at_ns INTEGER NOT NULL,
  FOREIGN KEY(run_id) REFERENCES run_meta(run_id)
) STRICT;
CREATE TABLE IF NOT EXISTS events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  event_id TEXT NOT NULL UNIQUE,
  run_id TEXT NOT NULL,
  command_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  kind TEXT NOT NULL,
  actor TEXT NOT NULL,
  occurred_at_ns INTEGER NOT NULL CHECK(occurred_at_ns>=0),
  payload_json TEXT NOT NULL,
  parent_event_digest TEXT NOT NULL,
  event_digest TEXT NOT NULL UNIQUE,
  UNIQUE(command_id, ordinal),
  FOREIGN KEY(command_id) REFERENCES commands(command_id) DEFERRABLE INITIALLY DEFERRED,
  FOREIGN KEY(run_id) REFERENCES run_meta(run_id)
) STRICT;
CREATE TABLE IF NOT EXISTS immutable_outbox (
  outbox_id TEXT PRIMARY KEY,
  command_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  topic TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  payload_digest TEXT NOT NULL,
  UNIQUE(command_id, ordinal),
  FOREIGN KEY(command_id) REFERENCES commands(command_id) DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE TABLE IF NOT EXISTS state_projection (
  singleton INTEGER PRIMARY KEY CHECK(singleton=1),
  head_seq INTEGER NOT NULL,
  state_json TEXT NOT NULL,
  checksum TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS runtime_branches (
  branch_id TEXT PRIMARY KEY,
  state TEXT NOT NULL CHECK(state IN ('open','suspended','resolved','closed')),
  depends_on_json TEXT NOT NULL,
  max_attempts INTEGER NOT NULL CHECK(max_attempts>0),
  attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count>=0)
) STRICT;
CREATE TABLE IF NOT EXISTS budget_accounts (
  account_id TEXT PRIMARY KEY,
  parent_id TEXT,
  limits_json TEXT NOT NULL,
  settled_json TEXT NOT NULL,
  held_json TEXT NOT NULL,
  debt INTEGER NOT NULL DEFAULT 0 CHECK(debt IN (0,1)),
  FOREIGN KEY(parent_id) REFERENCES budget_accounts(account_id)
) STRICT;
CREATE TABLE IF NOT EXISTS runtime_attempts (
  attempt_id TEXT PRIMARY KEY,
  branch_id TEXT NOT NULL,
  permit_id TEXT NOT NULL UNIQUE,
  scope_digest TEXT NOT NULL,
  lease_id TEXT NOT NULL UNIQUE,
  lease_epoch INTEGER NOT NULL,
  worker_generation INTEGER NOT NULL,
  fingerprint TEXT NOT NULL,
  effect_class TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('reserved','running','terminal','unknown')),
  FOREIGN KEY(branch_id) REFERENCES runtime_branches(branch_id)
) STRICT;
CREATE TABLE IF NOT EXISTS budget_reservations (
  reservation_id TEXT PRIMARY KEY,
  account_id TEXT NOT NULL,
  attempt_id TEXT NOT NULL,
  dimensions_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('active','settled','unknown','released')),
  FOREIGN KEY(account_id) REFERENCES budget_accounts(account_id),
  FOREIGN KEY(attempt_id) REFERENCES runtime_attempts(attempt_id)
) STRICT;
CREATE TABLE IF NOT EXISTS effect_conflict_holds (
  conflict_key TEXT PRIMARY KEY,
  operation_id TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('active','unknown'))
) STRICT;
CREATE TABLE IF NOT EXISTS effect_operations (
  operation_id TEXT PRIMARY KEY,
  attempt_id TEXT NOT NULL,
  effect_class TEXT NOT NULL,
  conflict_keys_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('prepared','dispatch_may_have_started','observed','confirmed_not_applied','unknown')),
  current_ordinal INTEGER NOT NULL CHECK(current_ordinal>0),
  FOREIGN KEY(attempt_id) REFERENCES runtime_attempts(attempt_id)
) STRICT;
CREATE TABLE IF NOT EXISTS effect_attempts (
  operation_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal>0),
  state TEXT NOT NULL,
  PRIMARY KEY(operation_id,ordinal),
  FOREIGN KEY(operation_id) REFERENCES effect_operations(operation_id)
) STRICT;
CREATE TABLE IF NOT EXISTS catalog_drafts (
  draft_id TEXT PRIMARY KEY,
  policy_json TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('open','provisioning','sealed','failed'))
) STRICT;
CREATE TABLE IF NOT EXISTS catalog_attachments (
  attachment_id TEXT PRIMARY KEY,
  draft_id TEXT NOT NULL,
  digest TEXT NOT NULL,
  byte_count INTEGER NOT NULL CHECK(byte_count>=0),
  FOREIGN KEY(draft_id) REFERENCES catalog_drafts(draft_id)
) STRICT;
CREATE TABLE IF NOT EXISTS provision_operations (
  operation_id TEXT PRIMARY KEY,
  draft_id TEXT NOT NULL,
  allocated_run_id TEXT NOT NULL UNIQUE,
  target_root TEXT NOT NULL,
  manifest_digest TEXT NOT NULL,
  owner_epoch INTEGER NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('preparing','run_allocated','run_materialized','sealed','failed_seal')),
  FOREIGN KEY(draft_id) REFERENCES catalog_drafts(draft_id)
) STRICT;
CREATE TABLE IF NOT EXISTS catalog_runs (
  run_id TEXT PRIMARY KEY,
  operation_id TEXT NOT NULL UNIQUE,
  manifest_digest TEXT NOT NULL,
  anchor_digest TEXT,
  state TEXT NOT NULL CHECK(state IN ('allocating','sealed','failed_seal','archived','purged')),
  FOREIGN KEY(operation_id) REFERENCES provision_operations(operation_id)
) STRICT;
"""

_LIFECYCLE_SCHEMA = """
CREATE TABLE IF NOT EXISTS archive_operations (
  operation_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL UNIQUE,
  owner_epoch INTEGER NOT NULL CHECK(owner_epoch>0),
  state TEXT NOT NULL CHECK(state IN ('requested','archived')),
  run_receipt_digest TEXT NOT NULL DEFAULT '',
  archive_receipt_digest TEXT NOT NULL DEFAULT '',
  requested_at_ns INTEGER NOT NULL CHECK(requested_at_ns>=0),
  FOREIGN KEY(run_id) REFERENCES catalog_runs(run_id)
) STRICT;
CREATE TABLE IF NOT EXISTS purge_operations (
  operation_id TEXT PRIMARY KEY,
  run_id TEXT NOT NULL UNIQUE,
  owner_epoch INTEGER NOT NULL CHECK(owner_epoch>0),
  state TEXT NOT NULL CHECK(state IN ('purge_pending','purged','purge_failed','purge_unknown')),
  plan_digest TEXT NOT NULL,
  plan_receipt_digest TEXT NOT NULL,
  absence_receipt_digest TEXT NOT NULL DEFAULT '',
  requested_at_ns INTEGER NOT NULL CHECK(requested_at_ns>=0),
  FOREIGN KEY(run_id) REFERENCES catalog_runs(run_id)
) STRICT;
CREATE TABLE IF NOT EXISTS purge_plan_items (
  operation_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal>=0),
  locator TEXT NOT NULL,
  adapter TEXT NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('pending','absent','unknown')),
  action_receipt_digest TEXT NOT NULL DEFAULT '',
  absence_receipt_digest TEXT NOT NULL DEFAULT '',
  PRIMARY KEY(operation_id,ordinal),
  UNIQUE(operation_id,locator),
  FOREIGN KEY(operation_id) REFERENCES purge_operations(operation_id)
) STRICT;
CREATE TABLE IF NOT EXISTS catalog_tombstones (
  run_id TEXT PRIMARY KEY,
  purge_operation_id TEXT NOT NULL UNIQUE,
  plan_digest TEXT NOT NULL,
  absence_receipt_digest TEXT NOT NULL,
  purged_at_ns INTEGER NOT NULL CHECK(purged_at_ns>=0),
  FOREIGN KEY(run_id) REFERENCES catalog_runs(run_id),
  FOREIGN KEY(purge_operation_id) REFERENCES purge_operations(operation_id)
) STRICT;
"""

_RECEIPT_OBJECT_SCHEMA = """
CREATE TABLE IF NOT EXISTS command_receipt_objects (
  receipt_digest TEXT PRIMARY KEY,
  command_id TEXT NOT NULL UNIQUE,
  first_seq INTEGER NOT NULL CHECK(first_seq>0),
  last_seq INTEGER NOT NULL CHECK(last_seq>=first_seq),
  object_digest TEXT NOT NULL,
  byte_count INTEGER NOT NULL CHECK(byte_count>0),
  state TEXT NOT NULL CHECK(state IN ('resolved','unresolved','unknown','rebound')),
  diagnostic_receipt_digest TEXT NOT NULL DEFAULT '',
  FOREIGN KEY(command_id) REFERENCES commands(command_id) DEFERRABLE INITIALLY DEFERRED
) STRICT;
"""


def _immutable_triggers(table: str) -> str:
    return f"""
CREATE TRIGGER IF NOT EXISTS {table}_no_update
BEFORE UPDATE ON {table} BEGIN SELECT RAISE(ABORT, '{table} is append-only'); END;
CREATE TRIGGER IF NOT EXISTS {table}_no_delete
BEFORE DELETE ON {table} BEGIN SELECT RAISE(ABORT, '{table} is append-only'); END;
"""
