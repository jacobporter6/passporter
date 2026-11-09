#!/bin/sh
# Regenerates the PNG icons from the SVG sources with rsvg-convert (librsvg).
# The PNGs are committed; run this only after editing an SVG.
set -eu
cd "$(dirname "$0")/../public/icons"
rsvg-convert -w 192 -h 192 icon.svg -o icon-192.png
rsvg-convert -w 512 -h 512 icon.svg -o icon-512.png
rsvg-convert -w 512 -h 512 icon-maskable.svg -o icon-maskable-512.png
rsvg-convert -w 180 -h 180 icon-maskable.svg -o apple-touch-icon.png
