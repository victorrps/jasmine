-- Restore the named `idx_users_clerk_user_id` index that was dropped
-- by the table-rebuild dance in migration 005. Migration 004 created
-- this as a partial unique index (`WHERE clerk_user_id IS NOT NULL`);
-- 005 made `clerk_user_id` NOT NULL and added an implicit UNIQUE
-- autoindex via the column constraint, which covers point lookups —
-- but we keep the named index around so future migrations and any
-- external tooling that references it by name still resolve.
--
-- `IF NOT EXISTS` guards the case where an implicit autoindex has
-- already claimed equivalent coverage on some SQLite versions.
CREATE UNIQUE INDEX IF NOT EXISTS idx_users_clerk_user_id
    ON users(clerk_user_id);
