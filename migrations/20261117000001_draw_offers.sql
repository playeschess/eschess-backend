-- Draw offers. `draw_offered_by` is the colour that has an offer waiting (empty when none). The two `*_offer_ply`
-- columns remember the number of moves played when each side last made an offer, so that nobody can offer again in the
-- same position (see eschess_game::DrawOffers for the rules).
ALTER TABLE games
    ADD COLUMN draw_offered_by TEXT CHECK (draw_offered_by IN ('white', 'black')),
    ADD COLUMN white_offer_ply INTEGER CHECK (white_offer_ply >= 0),
    ADD COLUMN black_offer_ply INTEGER CHECK (black_offer_ply >= 0),
    ADD CONSTRAINT draw_offer_only_while_active CHECK (draw_offered_by IS NULL OR status = 'active');
