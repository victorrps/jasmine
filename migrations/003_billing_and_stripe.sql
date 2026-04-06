-- Add pricing tier and Stripe integration fields to users
ALTER TABLE users ADD COLUMN tier TEXT NOT NULL DEFAULT 'free';
ALTER TABLE users ADD COLUMN stripe_customer_id TEXT;
ALTER TABLE users ADD COLUMN stripe_subscription_id TEXT;
ALTER TABLE users ADD COLUMN stripe_subscription_item_id TEXT;

CREATE INDEX IF NOT EXISTS idx_users_stripe_customer ON users(stripe_customer_id);
