-- Clerk auth integration (additive phase).
--
-- Adds the `clerk_user_id` natural key to the `users` table without
-- yet removing password-based auth. The legacy `/auth/register` and
-- `/auth/login` paths keep working until the cutover migration drops
-- `password_hash` and tightens the constraint to NOT NULL.
--
-- Why nullable for now: we run both auth systems side-by-side during
-- the dev cutover so existing local development can keep registering
-- accounts the old way while the new Clerk-backed dashboard is being
-- wired up. Once handlers move to ClerkAuth and dev users are
-- re-provisioned via Clerk (or via the `DEV_AUTH_BYPASS` header on a
-- fresh DB), the cutover migration will:
--   1. ALTER TABLE users DROP COLUMN password_hash;
--   2. ALTER TABLE users ALTER COLUMN clerk_user_id SET NOT NULL;
--
-- ⚠ Cutover compatibility note: `ALTER TABLE … DROP COLUMN` requires
-- SQLite **3.35.0** (released 2021-03-12) or newer. SQLite has no
-- `ALTER COLUMN … SET NOT NULL` syntax at any version, so step (2)
-- requires the table-rebuild dance (`CREATE TABLE users_new AS …;
-- DROP TABLE users; ALTER TABLE users_new RENAME TO users;`) — which
-- also lets us pivot to NOT NULL on `clerk_user_id` in one shot.
-- Plan to do the cutover with the rebuild path so we don't depend on
-- `DROP COLUMN` being available on every deploy target.

ALTER TABLE users ADD COLUMN clerk_user_id TEXT;
ALTER TABLE users ADD COLUMN image_url TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_users_clerk_user_id
    ON users(clerk_user_id)
    WHERE clerk_user_id IS NOT NULL;
