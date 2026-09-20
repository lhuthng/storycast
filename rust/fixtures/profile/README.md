# Test fixture profile: the shipped JSON shapes without the clips.
#
# `assets/*.json` mirror a real profile's registries (same keys, tags, rules,
# palette) so contract tests — including the regressions pinned against real
# content — assert real shapes; the mp3s are NOT here. Tests stub clip files
# where file-existence matters. If the shipped registries gain a structural
# feature (a new field, a new pool), mirror it here, or the suite tests
# yesterday's format. Divergent *content* (new sounds, retags) does not need
# mirroring unless a test pins it by name.
