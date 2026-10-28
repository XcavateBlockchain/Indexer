-- Supporting index for the lawyer directory's claim-history predicates (ADR-46).
--
-- `listings(claimedBy:)` and `listings(assignableTo:)` both probe candidacies by
-- (lawyer, listing_id): the former with EXISTS, the latter with NOT EXISTS. The
-- existing (listing_id, round) index cannot serve a lawyer-led lookup. A plain
-- B-tree is required because claimed history deliberately includes closed
-- candidacies and earlier rounds; a partial active-only index would make the
-- "Claimed" feed incomplete and could make its page and count plans disagree.

CREATE INDEX idx_mkt_candidacy_lawyer_listing
    ON marketplace_lawyer_candidacy (lawyer, listing_id);
