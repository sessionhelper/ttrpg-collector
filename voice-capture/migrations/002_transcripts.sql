CREATE TABLE transcript_segments (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    session_id UUID NOT NULL REFERENCES sessions(id),
    segment_index INTEGER NOT NULL,
    speaker_pseudo_id TEXT NOT NULL,
    start_time DOUBLE PRECISION NOT NULL,
    end_time DOUBLE PRECISION NOT NULL,
    text TEXT NOT NULL,
    original_text TEXT NOT NULL,
    confidence DOUBLE PRECISION,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE segment_flags (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    segment_id UUID NOT NULL REFERENCES transcript_segments(id),
    flagged_by UUID NOT NULL REFERENCES users(id),
    reason TEXT NOT NULL DEFAULT 'private_info',
    flagged_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    reverted_at TIMESTAMPTZ
);

CREATE TABLE segment_edits (
    id UUID PRIMARY KEY DEFAULT uuid_generate_v4(),
    segment_id UUID NOT NULL REFERENCES transcript_segments(id),
    edited_by UUID NOT NULL REFERENCES users(id),
    original_text TEXT NOT NULL,
    new_text TEXT NOT NULL,
    edited_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX idx_segments_session ON transcript_segments(session_id);
CREATE INDEX idx_flags_segment ON segment_flags(segment_id);
CREATE INDEX idx_edits_segment ON segment_edits(segment_id);
