# Vanilla upstream compatibility

- Keep upstream WebRTC and libvpx vanilla. Do not add downstream patches, copied upstream behavioral replacements, private hooks, or generated/overlay equivalents.
- Investigate our protocol, lifecycle and design first. Prefer correct supported APIs and removal of unnecessary complexity over upstream modifications.
- If reliable behavior appears to require modifying upstream, stop and raise the evidence and compatibility implications to the human. Do not implement the modification.
