-- S6-036: a person's chosen colour, shown on cursors, avatars and in history. NULL means "derive a
-- stable colour from the principal id" (see `profiles::default_color`); only a palette colour may be
-- stored (the palette lives in code, the column accepts any lowercase #rrggbb so the palette can
-- change without a migration).
ALTER TABLE principals ADD COLUMN color TEXT CHECK (color IS NULL OR color ~ '^#[0-9a-f]{6}$');
