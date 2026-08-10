# wl-roms — the ROM set as its own image, so the runtime image can stay ROM-less.
#
# The runtime image (docker/Dockerfile) deliberately does NOT bake these in: they are
# Commodore's property, so an image carrying them cannot be published. Splitting them out
# means the runtime image can go to a public registry / release while the ROMs stay in a
# private, LAN-only registry.
#
# ⚠ THIS IMAGE MUST NEVER BE PUSHED TO A PUBLIC REGISTRY. Private Gitea only.
#
# Usage: it seeds a volume, which the daemon then mounts read-only. busybox rather than
# scratch because seeding needs a shell to copy with.
#
#   docker run --rm -v <romvol>:/dest <this> sh -c 'cp -n /roms/* /dest/'
#   docker run ... -v <romvol>:/opt/trx64/resources/roms:ro wl-trx64:<version>
#
# Build (manually, NOT in CI — the ROMs are gitignored and must not enter the repo):
#   docker build -f docker/roms.Dockerfile -t <registry>/wl-roms:<set> docker/
FROM busybox:1.36-musl
LABEL org.opencontainers.image.title="wl-roms" \
      org.opencontainers.image.description="C64 + 1541 ROM set for the TRX64 runtime (PRIVATE — do not publish)" \
      org.opencontainers.image.licenses="proprietary-commodore"
COPY roms/ /roms/
# Seeding is the default action, so `docker run -v vol:/dest <image>` just works.
CMD ["sh", "-c", "cp -n /roms/* /dest/ 2>/dev/null; ls -1 /dest | wc -l | xargs echo 'ROMs in volume:'"]
