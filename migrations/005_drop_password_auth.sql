-- Cutover migration: Clerk is now the source of truth for identity.
--
-- This drops `password_hash` from `users` and tightens
-- `clerk_user_id` to NOT NULL. SQLite has no `ALTER COLUMN ... SET
-- NOT NULL` and no portable `DROP COLUMN` across all versions, so we
-- do the table-rebuild dance: create a new table with the desired
-- shape, copy rows that have a clerk_user_id, drop the old table,
-- rename, recreate indexes.
--
-- Anything in `users` without a `clerk_user_id` at this point is a
-- legacy row that was never migrated. Dev DB was wiped per product
-- decision; this migration drops orphan rows on purpose. If we ever
-- need to run this in an environment with real legacy users, the
-- pre-migration step is to provision Clerk identities and write
-- their `clerk_user_id` back via the webhook handler.

PRAGMA foreign_keys = OFF;

CREATE TABLE users_new (
    id TEXT PRIMARY KEY,
    email TEXT UNIQUE NOT NULL,
    name TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    -- Billing (from migration 003)
    tier TEXT NOT NULL DEFAULT 'free',
    stripe_customer_id TEXT,
    stripe_subscription_id TEXT,
    stripe_subscription_item_id TEXT,
    -- Clerk (was nullable in 004, now required)
    clerk_user_id TEXT NOT NULL UNIQUE,
    image_url TEXT
);

INSERT INTO users_new (
    id, email, name, created_at, updated_at,
    tier, stripe_customer_id, stripe_subscription_id, stripe_subscription_item_id,
    clerk_user_id, image_url
)
SELECT
    id, email, name, created_at, updated_at,
    tier, stripe_customer_id, stripe_subscription_id, stripe_subscription_item_id,
    clerk_user_id, image_url
FROM users
WHERE clerk_user_id IS NOT NULL;

DROP TABLE users;
ALTER TABLE users_new RENAME TO users;

-- Recreate indexes that lived on the old table.
CREATE INDEX IF NOT EXISTS idx_users_stripe_customer ON users(stripe_customer_id);

PRAGMA foreign_keys = ON;
