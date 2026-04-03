CREATE EXTENSION IF NOT EXISTS "uuid-ossp";

CREATE TABLE users (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    discord_id_hash TEXT UNIQUE NOT NULL,
    pseudo_id TEXT UNIQUE NOT NULL,
    global_opt_out BOOLEAN NOT NULL DEFAULT FALSE,
    opt_out_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE sessions (
    id UUID PRIMARY KEY,
    guild_id BIGINT NOT NULL,
    started_at TIMESTAMPTZ NOT NULL,
    ended_at TIMESTAMPTZ,
    game_system TEXT,
    campaign_name TEXT,
    participant_count INTEGER,
    s3_prefix TEXT,
    status TEXT NOT NULL DEFAULT 'awaiting_consent',
    collaborative_editing BOOLEAN NOT NULL DEFAULT TRUE,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE session_participants (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    session_id UUID NOT NULL REFERENCES sessions(id),
    user_id UUID REFERENCES users(id),
    consent_scope TEXT,
    consented_at TIMESTAMPTZ,
    withdrawn_at TIMESTAMPTZ,
    mid_session_join BOOLEAN NOT NULL DEFAULT FALSE,
    no_llm_training BOOLEAN NOT NULL DEFAULT FALSE,
    no_public_release BOOLEAN NOT NULL DEFAULT FALSE,
    UNIQUE(session_id, user_id)
);

CREATE TABLE consent_audit_log (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    user_id UUID REFERENCES users(id),
    session_id UUID,
    action TEXT NOT NULL,
    previous_scope TEXT,
    new_scope TEXT,
    timestamp TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    ip_address INET
);

CREATE INDEX idx_sessions_guild ON sessions(guild_id);
CREATE INDEX idx_participants_session ON session_participants(session_id);
CREATE INDEX idx_participants_user ON session_participants(user_id);
CREATE INDEX idx_users_pseudo ON users(pseudo_id);
CREATE INDEX idx_audit_user ON consent_audit_log(user_id);
