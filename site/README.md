# Libra human surface

Run `python3 tools/build_site.py` with mdBook **0.5.2** installed. The deployable
artifact is `build/site`. Documentation chapters are rendered from the existing
product-owned Markdown listed in `tools/build_site.py`; do not maintain parallel
copies under `site/`.

The source installer remains `scripts/install.sh`. The site describes source
installation and links to that script; it does not invent a release binary,
hosted installer, account, API, or SaaS surface. Product maturity remains
Developer Preview.

The shared public-surface checker is pinned by content checksum to the independently
reviewed `horonomy/.github` merge `76cde284d122b7f21deebb935c711dfbc0c61b41`.
Update both `tools/public_surface.py` and `tools/public_surface.sha256` only from an
independently reviewed upstream revision.

## Activation and rollback

Source preparation and merge do not authorize publication. Before creating a
Pages deployment or attaching DNS, resolve HORO-1701's experimental/public-preview
release-contract decision with the product owner. Keep Developer Preview maturity;
the existing public release is v0.0.2, and Team Alpha remains outside this surface.

Build and validate the artifact before deployment. Reuse the established static
Cloudflare Pages pattern and attach only `libra.horonom.com`; Libra is local-first,
so no runtime hostname is warranted. After deployment, verify external DNS, TLS,
canonical URLs, headers, redirects, robots, sitemap, links, and desktop/mobile
rendering. Roll back to the last verified artifact if any check fails. The site
uses no analytics and must not expose a Pages or internal origin hostname.
