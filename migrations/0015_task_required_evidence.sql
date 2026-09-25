-- dagq-schema: breaking
-- Required evidence (ADR-0019 decision 5): the receipt checks (`tests`,
-- `e2e`, `subagent_review`) a task demands, as a JSON array. Validation
-- parks a run whose receipt lacks one of them as `needs_session`
-- (`evidence_missing`) instead of failing it. Existing tasks demand none.
ALTER TABLE tasks ADD COLUMN required_evidence TEXT NOT NULL DEFAULT '[]';
