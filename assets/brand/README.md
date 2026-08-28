# Tollgate brand assets

Original artwork for Tollgate, licensed MIT with the rest of the repository. You
are free to use these to refer to the project (README, blog posts, talks, social
posts). Please do not modify the mark and present it as an endorsement.

## Files

- `tollgate-mark.svg` - the gate mark in a rounded badge. App icon, avatar, favicon source.
- `tollgate-logo-dark.svg` - horizontal lockup (mark + wordmark) for dark backgrounds.
- `tollgate-logo-light.svg` - horizontal lockup for light backgrounds.
- `tollgate-social.svg` - 1200x630 Open Graph / social card.
- `favicon.svg` - 32px favicon.

## Palette

- Ink (dark) `#0d1117`
- Text on dark `#e6edf3`
- Accent green `#3fb950` (on light backgrounds use `#2da44e` for contrast)
- Muted `#8b949e`
- Line `#2b3240`

Typeface: the system UI sans stack (San Francisco / Segoe UI / Roboto). No font
files are bundled, so wordmarks render with the viewer's system font.

## Exporting to PNG

The assets are SVG so they stay crisp at any size. Some platforms want a raster
image (a social/OG image usually needs PNG). Rasterise locally, for example:

```bash
# with librsvg
rsvg-convert -w 1200 -h 630 tollgate-social.svg -o tollgate-social.png
rsvg-convert -w 512  -h 512 tollgate-mark.svg   -o tollgate-mark-512.png

# or with ImageMagick
magick -background none tollgate-mark.svg -resize 512x512 tollgate-mark-512.png
```

Text in the social card is rendered as live `<text>`; if you need pixel-identical
output across machines, rasterise on one machine and reuse the PNG.
